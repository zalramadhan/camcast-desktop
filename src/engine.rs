//! Engine: menerima video WebRTC dari HP (GStreamer webrtcbin), menampilkan frame,
//! dan merekam (VP8 passthrough + audio mic PC) ke MKV, lalu konversi ke MP4 (ffmpeg).

use std::{
    path::Path,
    sync::{
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
        Arc, Mutex, OnceLock,
    },
    time::{Duration, Instant},
};

use eframe::egui;
use gst::prelude::*;
use serde_json::{json, Value};
use tokio::sync::broadcast;

pub type Sig = broadcast::Sender<(&'static str, String)>;
pub type SigRx = broadcast::Receiver<(&'static str, String)>;

/// Latensi jitter buffer webrtcbin (ms). Default GStreamer 200 ms.
/// 100 ms = kompromi: paket yang telat sedikit masih ditahan sehingga frame tidak
/// dibuang (fps stabil di WiFi yang naik-turun), dengan tambahan latensi preview
/// sekitar 0,1 detik. Turunkan ke ~30 ms kalau butuh preview paling responsif dan
/// jaringannya bersih.
const JITTER_LATENCY_MS: u32 = 100;

/// Default kualitas stream. Bisa diubah dari UI desktop dan dikirim ke HP lewat
/// signaling (pesan `quality`). Bitrate dalam Mbps.
pub const DEFAULT_BITRATE_MBPS: u32 = 12;
pub const DEFAULT_FPS: u32 = 25;

/// Setelan encoder HP + fps rekaman.
#[derive(Clone, Copy, PartialEq)]
pub struct Quality {
    pub bitrate_mbps: u32,
    pub fps: u32,
}

impl Default for Quality {
    fn default() -> Self {
        Quality {
            bitrate_mbps: DEFAULT_BITRATE_MBPS,
            fps: DEFAULT_FPS,
        }
    }
}

/// Folder setelan aplikasi: $XDG_CONFIG_HOME/camcast (fallback ~/.config/camcast).
pub fn config_dir() -> Option<std::path::PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|s| !s.is_empty())
        .map(std::path::PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".config"))
        })?;
    Some(base.join("camcast"))
}

/// Lokasi setelan tersimpan: <config_dir>/config.json.
fn config_path() -> Option<std::path::PathBuf> {
    Some(config_dir()?.join("config.json"))
}

impl Quality {
    /// Setelan terakhir yang dipakai; file tidak ada / rusak -> default.
    pub fn load() -> Quality {
        let Some(path) = config_path() else {
            return Quality::default();
        };
        let Ok(text) = std::fs::read_to_string(path) else {
            return Quality::default();
        };
        let Ok(v) = serde_json::from_str::<Value>(&text) else {
            return Quality::default();
        };
        Quality {
            bitrate_mbps: v["bitrate_mbps"]
                .as_u64()
                .map_or(DEFAULT_BITRATE_MBPS, |n| n.clamp(2, 20) as u32),
            fps: v["fps"]
                .as_u64()
                .map_or(DEFAULT_FPS, |n| n.clamp(10, 30) as u32),
        }
    }

    /// Simpan setelan. Gagal tulis tidak boleh mengganggu streaming, jadi diabaikan.
    pub fn save(&self) {
        let Some(path) = config_path() else { return };
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let text = json!({ "bitrate_mbps": self.bitrate_mbps, "fps": self.fps }).to_string();
        let _ = std::fs::write(path, text);
    }
}

// Cabang decode + rekam, dibuat saat pad video dari webrtcbin muncul.
const BRANCH: &str = concat!(
    "rtpvp8depay ! tee name=t ",
    "t. ! queue max-size-buffers=2 leaky=downstream ! vp8dec ! videoconvert ! ",
    "video/x-raw,format=RGBA ! appsink name=disp sync=false max-buffers=1 drop=true ",
    "t. ! queue max-size-buffers=120 ! appsink name=recsink sync=false max-buffers=120 drop=false"
);

// Cabang monitor mic: meter level (appsink) + opsional diputar ke speaker (volume 0 = senyap).
const MONITOR_TAIL: &str = concat!(
    "audioconvert ! audioresample ! audio/x-raw,format=S16LE,rate=48000 ! tee name=t ",
    "t. ! queue max-size-time=200000000 leaky=downstream ! ",
    "appsink name=meter sync=false max-buffers=10 drop=true ",
    "t. ! queue max-size-time=100000000 leaky=downstream ! ",
    "volume name=vol volume=0.0 ! autoaudiosink sync=false"
);

/// Level mic terbaru (dibaca UI). peak/rms diakumulasi (max) sampai UI mengambilnya.
#[derive(Default)]
pub struct MicState {
    pub peak: f32,
    pub rms: f32,
    pub last_buf: Option<Instant>,
    pub last_sound: Option<Instant>,
}

pub struct Frame {
    pub w: usize,
    pub h: usize,
    pub rgba: Vec<u8>,
}

/// Statistik stream yang benar-benar diterima, dari `webrtcbin get-stats`.
/// (webrtcbin tidak decode, jadi fps/resolusi diambil dari frame yang sampai.)
#[derive(Clone, Copy, Default)]
pub struct RtpStats {
    pub bitrate_kbps: f64,
    pub lost_pct: f64,
    pub jitter_ms: f64,
}

#[derive(Default)]
pub struct Shared {
    pub frame: Mutex<Option<Frame>>,
    pub dims: Mutex<(u32, u32)>,
    pub frame_count: AtomicU64,
    pub phone_online: AtomicUsize,
    /// Setelan bitrate/fps yang dikirim ke HP.
    pub quality: Mutex<Quality>,
    /// Statistik RTP terakhir; None = belum ada data / sesi berakhir.
    pub rtp: Mutex<Option<RtpStats>>,
    pub rec: Mutex<Option<Recorder>>,
    pub status: Mutex<String>,
    pub key_pad: Mutex<Option<gst::Pad>>,
    pub mic: Mutex<MicState>,
    pub ctx: OnceLock<egui::Context>,
}

impl Shared {
    pub fn repaint(&self) {
        if let Some(c) = self.ctx.get() {
            c.request_repaint();
        }
    }
    pub fn set_status(&self, s: impl Into<String>) {
        *self.status.lock().unwrap() = s.into();
        self.repaint();
    }
}

pub enum Mic {
    Off,
    Default,
    Device(gst::Device),
}

/// Daftar mikrofon; entri pertama = default sistem.
pub fn list_mics() -> Vec<(String, Option<gst::Device>)> {
    let mut v = vec![("Default sistem".to_string(), None)];
    let mon = gst::DeviceMonitor::new();
    mon.add_filter(Some("Audio/Source"), None::<&gst::Caps>);
    if mon.start().is_ok() {
        for d in mon.devices() {
            v.push((d.display_name().to_string(), Some(d)));
        }
        mon.stop();
    }
    v
}

// ------------------------------------------------------------------ Recorder

pub struct Recorder {
    pipeline: gst::Pipeline,
    src: gst_app::AppSrc,
    pub path: String,
    pub started: Instant,
    /// Fps yang dipakai header rekaman dan konversi ffmpeg (CFR).
    pub fps: u32,
    first: Option<gst::ClockTime>,
    offset: gst::ClockTime,
    got_key: bool,
    /// Caps terakhir yang dikirim ke appsrc; diperbarui kalau resolusi HP berubah.
    caps: Option<gst::Caps>,
}

impl Recorder {
    /// Dorong buffer VP8 ke pipeline rekam. Mulai dari keyframe; timestamp di-rebase
    /// ke running-time pipeline rekam supaya sinkron dengan audio mic.
    fn push(&mut self, sample: &gst::Sample) {
        let Some(buf) = sample.buffer() else { return };
        if !self.got_key {
            if buf.flags().contains(gst::BufferFlags::DELTA_UNIT) {
                return;
            }
            self.got_key = true;
        }
        // HP bisa mengganti resolusi di tengah sesi (Chrome men-scale saat bandwidth
        // turun). Kalau caps tidak ikut diperbarui, muxer menulis bitstream yang tidak
        // cocok dengan header dan hasilnya rusak. Mulai dari keyframe supaya aman.
        if let Some(caps) = sample.caps().map(|c| c.to_owned()) {
            if self.caps.as_ref() != Some(&caps) {
                let _ = self.src.set_caps(Some(&caps));
                self.caps = Some(caps);
            }
        }
        let Some(pts) = buf.pts() else { return };
        if self.first.is_none() {
            self.first = Some(pts);
            self.offset = self
                .pipeline
                .current_running_time()
                .unwrap_or(gst::ClockTime::ZERO);
        }
        let first = self.first.unwrap();
        let Some(mut out) = sample.buffer_owned() else { return };
        {
            let o = out.make_mut();
            o.set_pts(self.offset + pts.saturating_sub(first));
            o.set_dts(gst::ClockTime::NONE);
            o.set_duration(gst::ClockTime::NONE);
        }
        let _ = self.src.push_buffer(out);
    }
}

fn mk(name: &str) -> Result<gst::Element, gst::glib::BoolError> {
    gst::ElementFactory::make(name).build()
}

/// Folder rekaman tetap (tidak tergantung dari mana app dibuka): ~/Videos/CamCast
fn rec_dir() -> std::path::PathBuf {
    let home = std::env::var_os("HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    let videos = home.join("Videos");
    (if videos.is_dir() { videos } else { home }).join("CamCast")
}

fn build_recorder(
    path: &str,
    w: u32,
    h: u32,
    fps: u32,
    mic: Mic,
) -> Result<Recorder, Box<dyn std::error::Error>> {
    std::fs::create_dir_all(Path::new(path).parent().unwrap_or(Path::new(".")))?;
    let pipeline = gst::Pipeline::new();

    // is-live=false: appsrc tidak menunggu clock pipeline rekam, jadi dorongan buffer
    // dari pipeline penerima tidak pernah tertahan/balik menekan cabang tampilan.
    let src = gst::ElementFactory::make("appsrc")
        .property("is-live", false)
        .property("format", gst::Format::Time)
        .property("do-timestamp", false)
        .property("max-bytes", 0u64)
        .build()?
        .downcast::<gst_app::AppSrc>()
        .map_err(|_| "elemen bukan appsrc")?;
    src.set_caps(Some(
        &gst::Caps::builder("video/x-vp8")
            .field("width", w as i32)
            .field("height", h as i32)
            .field("framerate", gst::Fraction::new(fps as i32, 1))
            .build(),
    ));
    let src_el: gst::Element = src.clone().upcast();

    let qv = mk("queue")?;
    let mux = gst::ElementFactory::make("matroskamux")
        .property("streamable", true)
        .build()?;
    let sink = gst::ElementFactory::make("filesink")
        .property("location", path)
        .build()?;

    pipeline.add_many([&src_el, &qv, &mux, &sink])?;
    gst::Element::link_many([&src_el, &qv])?;
    let vpad = mux.request_pad_simple("video_%u").ok_or("pad video mux")?;
    qv.static_pad("src").ok_or("pad src queue")?.link(&vpad)?;
    mux.link(&sink)?;

    if !matches!(mic, Mic::Off) {
        let a_src = match mic {
            Mic::Device(d) => d.create_element(Some("mic"))?,
            _ => mk("autoaudiosrc")?,
        };
        let conv = mk("audioconvert")?;
        let res = mk("audioresample")?;
        let qa = mk("queue")?;
        let enc = mk("opusenc")?;
        pipeline.add_many([&a_src, &conv, &res, &qa, &enc])?;
        gst::Element::link_many([&a_src, &conv, &res, &qa, &enc])?;
        let apad = mux.request_pad_simple("audio_%u").ok_or("pad audio mux")?;
        enc.static_pad("src").ok_or("pad src opusenc")?.link(&apad)?;
    }

    pipeline.set_state(gst::State::Playing)?;

    Ok(Recorder {
        pipeline,
        src,
        path: path.to_string(),
        started: Instant::now(),
        fps,
        first: None,
        offset: gst::ClockTime::ZERO,
        got_key: false,
        caps: None,
    })
}

// -------------------------------------------------------------------- Engine

struct Session {
    pipeline: gst::Pipeline,
    webrtc: gst::Element,
    alive: Arc<AtomicBool>,
}

struct MicMonitor {
    pipeline: gst::Pipeline,
    vol: gst::Element,
}

pub struct Engine {
    shared: Arc<Shared>,
    sig: Sig,
    session: Mutex<Option<Session>>,
    monitor: Mutex<Option<MicMonitor>>,
    audible: AtomicBool,
}

impl Engine {
    pub fn new(shared: Arc<Shared>, sig: Sig) -> Self {
        // pakai setelan terakhir dari disk, bukan selalu default
        *shared.quality.lock().unwrap() = Quality::load();
        Engine {
            shared,
            sig,
            session: Mutex::new(None),
            monitor: Mutex::new(None),
            audible: AtomicBool::new(false),
        }
    }

    pub fn hello(&self) {
        let _ = self
            .sig
            .send(("viewer", json!({ "type": "hello" }).to_string()));
    }

    pub fn quality(&self) -> Quality {
        *self.shared.quality.lock().unwrap()
    }

    /// Kirim setelan bitrate/fps ke HP. HP menerapkannya lewat setParameters,
    /// jadi berlaku langsung tanpa negosiasi ulang.
    fn send_quality(&self) {
        let q = self.quality();
        let msg = json!({
            "type": "quality",
            "maxBitrate": q.bitrate_mbps * 1_000_000,
            "maxFramerate": q.fps,
        });
        let _ = self.sig.send(("viewer", msg.to_string()));
    }

    pub fn set_quality(&self, q: Quality) {
        if self.quality() == q {
            return;
        }
        *self.shared.quality.lock().unwrap() = q;
        q.save(); // ingat pilihan terakhir
        self.send_quality();
    }

    /// Pesan signaling dari HP.
    pub fn on_signal(&self, text: &str) {
        let Ok(v) = serde_json::from_str::<Value>(text) else { return };
        match v["type"].as_str() {
            // kualitas dulu, supaya offer dari HP sudah memakai setelan UI
            Some("hello-phone") => {
                self.send_quality();
                self.hello();
            }
            Some("offer") => {
                if let Some(sdp) = v["sdp"].as_str() {
                    self.start_session(sdp);
                }
            }
            Some("candidate") => self.add_candidate(&v["candidate"]),
            Some("bye") => self.stop_session(),
            _ => {}
        }
    }

    fn add_candidate(&self, c: &Value) {
        let (Some(cand), Some(mline)) = (c["candidate"].as_str(), c["sdpMLineIndex"].as_u64())
        else {
            return;
        };
        if cand.is_empty() {
            return;
        }
        if let Some(s) = self.session.lock().unwrap().as_ref() {
            let mline = mline as u32;
            s.webrtc
                .emit_by_name::<()>("add-ice-candidate", &[&mline, &cand]);
        }
    }

    pub fn stop_session(&self) {
        let s = self.session.lock().unwrap().take();
        if let Some(s) = s {
            s.alive.store(false, Ordering::Relaxed);
            let _ = s.pipeline.set_state(gst::State::Null);
        }
        self.stop_recording();
        *self.shared.frame.lock().unwrap() = None;
        *self.shared.dims.lock().unwrap() = (0, 0);
        *self.shared.key_pad.lock().unwrap() = None;
        *self.shared.rtp.lock().unwrap() = None;
        self.shared.repaint();
    }

    fn start_session(&self, sdp: &str) {
        self.stop_session();
        if let Err(e) = self.try_start(sdp) {
            self.shared.set_status(format!("Gagal memulai WebRTC: {e}"));
        }
    }

    fn try_start(&self, sdp: &str) -> Result<(), Box<dyn std::error::Error>> {
        let shared = self.shared.clone();
        let sig = self.sig.clone();

        let pipeline = gst::Pipeline::new();
        let webrtc = mk("webrtcbin")?;
        if webrtc.find_property("latency").is_some() {
            webrtc.set_property("latency", JITTER_LATENCY_MS);
        }
        pipeline.add(&webrtc)?;

        // ICE lokal -> HP
        {
            let sig = sig.clone();
            webrtc.connect("on-ice-candidate", false, move |values| {
                let mline = values[1].get::<u32>().ok()?;
                let cand = values[2].get::<String>().ok()?;
                let msg = json!({
                    "type": "candidate",
                    "candidate": { "candidate": cand, "sdpMLineIndex": mline }
                });
                let _ = sig.send(("viewer", msg.to_string()));
                None
            });
        }

        // Pad video muncul -> pasang cabang decode + rekam
        {
            let shared = shared.clone();
            let pipeline_weak = pipeline.downgrade();
            webrtc.connect_pad_added(move |_, pad| {
                let Some(pipeline) = pipeline_weak.upgrade() else { return };
                if pad.direction() != gst::PadDirection::Src {
                    return;
                }
                let caps = pad.current_caps().unwrap_or_else(|| pad.query_caps(None));
                let Some(s) = caps.structure(0) else { return };
                if s.get::<&str>("media").ok() != Some("video") {
                    return;
                }
                if s.get::<&str>("encoding-name").ok() != Some("VP8") {
                    shared.set_status("Codec HP bukan VP8 (halaman /phone memaksa VP8).");
                    return;
                }
                if let Err(e) = attach_branch(&pipeline, pad, shared.clone()) {
                    shared.set_status(format!("Gagal memasang cabang video: {e}"));
                }
            });
        }

        // Pantau error GStreamer
        let alive = Arc::new(AtomicBool::new(true));
        if let Some(bus) = pipeline.bus() {
            let alive = alive.clone();
            let shared = shared.clone();
            std::thread::spawn(move || {
                while alive.load(Ordering::Relaxed) {
                    if let Some(msg) = bus.timed_pop_filtered(
                        gst::ClockTime::from_mseconds(500),
                        &[gst::MessageType::Error],
                    ) {
                        if let gst::MessageView::Error(e) = msg.view() {
                            shared.set_status(format!("GStreamer: {}", e.error()));
                        }
                    }
                }
            });
        }

        // Statistik inbound RTP tiap detik (bitrate, paket hilang, jitter)
        {
            let shared = shared.clone();
            let alive = alive.clone();
            let w = webrtc.clone();
            std::thread::spawn(move || poll_stats(w, shared, alive));
        }

        pipeline.set_state(gst::State::Playing)?;

        // offer dari HP -> set-remote-description -> create-answer -> set-local -> kirim answer
        let desc = gst_sdp::SDPMessage::parse_buffer(sdp.as_bytes())?;
        let offer = gst_webrtc::WebRTCSessionDescription::new(gst_webrtc::WebRTCSDPType::Offer, desc);

        let w1 = webrtc.clone();
        let sig1 = sig.clone();
        let promise = gst::Promise::with_change_func(move |_reply| {
            let w2 = w1.clone();
            let sig2 = sig1.clone();
            let promise = gst::Promise::with_change_func(move |reply| {
                let Ok(Some(reply)) = reply else {
                    eprintln!("create-answer gagal");
                    return;
                };
                let Some(answer) = reply
                    .value("answer")
                    .ok()
                    .and_then(|v| v.get::<gst_webrtc::WebRTCSessionDescription>().ok())
                else {
                    eprintln!("answer tidak ada");
                    return;
                };
                w2.emit_by_name::<()>("set-local-description", &[&answer, &None::<gst::Promise>]);
                let text = answer.sdp().as_text().unwrap_or_default();
                let _ = sig2.send(("viewer", json!({ "type": "answer", "sdp": text }).to_string()));
            });
            w1.emit_by_name::<()>("create-answer", &[&None::<gst::Structure>, &promise]);
        });
        webrtc.emit_by_name::<()>("set-remote-description", &[&offer, &promise]);

        *self.session.lock().unwrap() = Some(Session {
            pipeline,
            webrtc,
            alive,
        });
        shared.set_status("Menyambung ke HP…");
        Ok(())
    }

    // ---------------------------------------------------------- monitor mic

    /// (Re)start monitor mic: meter level selalu jalan; suara ke speaker tergantung `audible`.
    pub fn start_mic_monitor(&self, mic: Mic) {
        self.stop_mic_monitor();
        if matches!(mic, Mic::Off) {
            return;
        }
        match self.build_monitor(mic) {
            Ok(m) => *self.monitor.lock().unwrap() = Some(m),
            Err(e) => self.shared.set_status(format!("Monitor mic gagal: {e}")),
        }
    }

    pub fn stop_mic_monitor(&self) {
        if let Some(m) = self.monitor.lock().unwrap().take() {
            let _ = m.pipeline.set_state(gst::State::Null);
        }
        *self.shared.mic.lock().unwrap() = MicState::default();
    }

    /// Nyalakan / matikan suara mic di speaker (meter tetap jalan).
    pub fn set_monitor_audible(&self, on: bool) {
        self.audible.store(on, Ordering::Relaxed);
        if let Some(m) = self.monitor.lock().unwrap().as_ref() {
            m.vol.set_property("volume", if on { 1.0f64 } else { 0.0f64 });
        }
    }

    fn build_monitor(&self, mic: Mic) -> Result<MicMonitor, Box<dyn std::error::Error>> {
        let pipeline = gst::Pipeline::new();
        let src = match mic {
            Mic::Device(d) => d.create_element(Some("micmon"))?,
            _ => mk("autoaudiosrc")?,
        };
        let tail = gst::parse::bin_from_description(MONITOR_TAIL, true)?;
        pipeline.add_many([&src, tail.upcast_ref::<gst::Element>()])?;
        src.link(&tail)?;

        let vol = tail.by_name("vol").ok_or("elemen volume")?;
        let on = self.audible.load(Ordering::Relaxed);
        vol.set_property("volume", if on { 1.0f64 } else { 0.0f64 });

        let meter = tail
            .by_name("meter")
            .and_then(|e| e.downcast::<gst_app::AppSink>().ok())
            .ok_or("appsink meter")?;
        let shared = self.shared.clone();
        meter.set_callbacks(
            gst_app::AppSinkCallbacks::builder()
                .new_sample(move |sink| {
                    let sample = sink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                    let buffer = sample.buffer().ok_or(gst::FlowError::Error)?;
                    let map = buffer.map_readable().map_err(|_| gst::FlowError::Error)?;
                    let (mut peak, mut sum, mut n) = (0f32, 0f64, 0usize);
                    for c in map.as_slice().chunks_exact(2) {
                        let v = i16::from_le_bytes([c[0], c[1]]) as f32 / 32768.0;
                        peak = peak.max(v.abs());
                        sum += (v * v) as f64;
                        n += 1;
                    }
                    let rms = if n > 0 { (sum / n as f64).sqrt() as f32 } else { 0.0 };
                    let now = Instant::now();
                    let mut st = shared.mic.lock().unwrap();
                    st.peak = st.peak.max(peak);
                    st.rms = st.rms.max(rms);
                    st.last_buf = Some(now);
                    if peak > 0.01 {
                        st.last_sound = Some(now); // di atas sekitar -40 dBFS
                    }
                    Ok(gst::FlowSuccess::Ok)
                })
                .build(),
        );

        pipeline.set_state(gst::State::Playing)?;
        Ok(MicMonitor { pipeline, vol })
    }

    // ------------------------------------------------------------- rekaman

    pub fn start_recording(&self, mic: Mic) -> Result<String, String> {
        if self.shared.rec.lock().unwrap().is_some() {
            return Err("Sudah merekam".into());
        }
        let (w, h) = *self.shared.dims.lock().unwrap();
        if w == 0 || h == 0 {
            return Err("Belum ada video dari HP".into());
        }
        let path = rec_dir()
            .join(format!(
                "rec_{}.mkv",
                chrono::Local::now().format("%Y%m%d_%H%M%S")
            ))
            .to_string_lossy()
            .into_owned();
        let fps = self.shared.quality.lock().unwrap().fps.clamp(1, 60);
        let rec = build_recorder(&path, w, h, fps, mic)
            .map_err(|e| format!("Gagal membuat pipeline rekam: {e}"))?;
        *self.shared.rec.lock().unwrap() = Some(rec);

        // minta keyframe baru dari HP supaya rekaman bisa mulai cepat
        if let Some(pad) = self.shared.key_pad.lock().unwrap().as_ref() {
            pad.push_event(
                gst_video::UpstreamForceKeyUnitEvent::builder()
                    .all_headers(true)
                    .build(),
            );
        }
        Ok(path)
    }

    pub fn stop_recording(&self) {
        let Some(rec) = self.shared.rec.lock().unwrap().take() else { return };
        let shared = self.shared.clone();
        std::thread::spawn(move || {
            shared.set_status("Menyelesaikan rekaman…");
            let _ = rec.src.end_of_stream();
            rec.pipeline.send_event(gst::event::Eos::new());
            if let Some(bus) = rec.pipeline.bus() {
                let _ = bus.timed_pop_filtered(
                    gst::ClockTime::from_seconds(5),
                    &[gst::MessageType::Eos, gst::MessageType::Error],
                );
            }
            let _ = rec.pipeline.set_state(gst::State::Null);

            // konversi MKV (VP8/Opus) -> MP4 (H.264/AAC), kalau ffmpeg ada
            let mp4 = Path::new(&rec.path).with_extension("mp4");
            shared.set_status("Konversi ke MP4…");
            let ok = std::process::Command::new("ffmpeg")
                .args(["-y", "-loglevel", "error", "-i", &rec.path])
                .args(["-c:v", "libx264", "-preset", "veryfast", "-pix_fmt", "yuv420p"])
                .args(["-c:a", "aac"])
                // output -r = CFR, supaya MP4 tidak mewarisi timestamp yang naik-turun
                .args(["-r", &rec.fps.to_string()])
                .arg(&mp4)
                .status()
                .map(|s| s.success())
                .unwrap_or(false);
            if ok {
                shared.set_status(format!("Tersimpan: {}", mp4.display()));
            } else {
                shared.set_status(format!("Tersimpan: {} (MP4 gagal / ffmpeg tidak ada)", rec.path));
            }
        });
    }
}

// ------------------------------------------------------------ cabang video

fn attach_branch(
    pipeline: &gst::Pipeline,
    pad: &gst::Pad,
    shared: Arc<Shared>,
) -> Result<(), Box<dyn std::error::Error>> {
    let bin = gst::parse::bin_from_description(BRANCH, true)?;
    pipeline.add(&bin)?;
    bin.sync_state_with_parent()?;
    let sink_pad = bin.static_pad("sink").ok_or("bin tanpa sink pad")?;
    pad.link(&sink_pad)?;
    *shared.key_pad.lock().unwrap() = Some(sink_pad);

    // tampilan
    let disp = bin
        .by_name("disp")
        .and_then(|e| e.downcast::<gst_app::AppSink>().ok())
        .ok_or("appsink disp")?;
    {
        let shared = shared.clone();
        disp.set_callbacks(
            gst_app::AppSinkCallbacks::builder()
                .new_sample(move |sink| {
                    let sample = sink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                    let buffer = sample.buffer().ok_or(gst::FlowError::Error)?;
                    let caps = sample.caps().ok_or(gst::FlowError::Error)?;
                    let info = gst_video::VideoInfo::from_caps(caps)
                        .map_err(|_| gst::FlowError::Error)?;
                    let map = buffer.map_readable().map_err(|_| gst::FlowError::Error)?;
                    let (w, h) = (info.width() as usize, info.height() as usize);
                    let stride = info.stride()[0] as usize;
                    let data = map.as_slice();
                    if data.len() < stride * (h - 1) + w * 4 {
                        return Err(gst::FlowError::Error);
                    }
                    let mut rgba = Vec::with_capacity(w * h * 4);
                    for row in 0..h {
                        rgba.extend_from_slice(&data[row * stride..row * stride + w * 4]);
                    }
                    *shared.dims.lock().unwrap() = (w as u32, h as u32);
                    *shared.frame.lock().unwrap() = Some(Frame { w, h, rgba });
                    shared.repaint();
                    Ok(gst::FlowSuccess::Ok)
                })
                .build(),
        );
    }

    // jalur rekam
    let recsink = bin
        .by_name("recsink")
        .and_then(|e| e.downcast::<gst_app::AppSink>().ok())
        .ok_or("appsink recsink")?;
    recsink.set_callbacks(
        gst_app::AppSinkCallbacks::builder()
            .new_sample(move |sink| {
                let sample = sink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                // cabang rekam tidak leaky, jadi hitungan ini = frame yang benar-benar
                // diterima dari HP (bukan frame yang sempat tampil di window)
                shared.frame_count.fetch_add(1, Ordering::Relaxed);
                if let Some(r) = shared.rec.lock().unwrap().as_mut() {
                    r.push(&sample);
                }
                Ok(gst::FlowSuccess::Ok)
            })
            .build(),
    );
    Ok(())
}

// ------------------------------------------------------------- statistik RTP

struct RawStats {
    bytes_received: u64,
    packets_received: u64,
    packets_lost: u64,
    jitter_ms: f64,
}

fn stat_u64(s: &gst::StructureRef, key: &str) -> Option<u64> {
    if let Ok(v) = s.get::<u64>(key) {
        return Some(v);
    }
    if let Ok(v) = s.get::<u32>(key) {
        return Some(v as u64);
    }
    if let Ok(v) = s.get::<i64>(key) {
        return Some(v.max(0) as u64);
    }
    None
}

/// Ambil sub-struktur `inbound-rtp` dengan paket terbanyak (video HP).
fn parse_stats(report: &gst::StructureRef) -> Option<RawStats> {
    let mut best: Option<RawStats> = None;
    for (_id, val) in report.iter() {
        let Ok(s) = val.get::<gst::Structure>() else {
            continue;
        };
        let Some(packets_received) = stat_u64(&s, "packets-received") else {
            continue;
        };
        let bytes_received = stat_u64(&s, "bytes-received").unwrap_or(0);
        if packets_received == 0 && bytes_received == 0 {
            continue;
        }
        let cand = RawStats {
            bytes_received,
            packets_received,
            packets_lost: stat_u64(&s, "packets-lost").unwrap_or(0),
            jitter_ms: s.get::<f64>("jitter").map(|j| j * 1000.0).unwrap_or(0.0),
        };
        if best
            .as_ref()
            .map_or(true, |b| cand.packets_received > b.packets_received)
        {
            best = Some(cand);
        }
    }
    best
}

/// Minta statistik ke webrtcbin (action signal `get-stats`) dan tunggu hasilnya.
fn fetch_stats(webrtc: &gst::Element) -> Option<RawStats> {
    let (tx, rx) = std::sync::mpsc::channel::<Option<RawStats>>();
    let promise = gst::Promise::with_change_func(move |reply| {
        let parsed = reply.ok().flatten().and_then(parse_stats);
        let _ = tx.send(parsed);
    });
    webrtc.emit_by_name::<()>("get-stats", &[&None::<gst::Pad>, &promise]);
    rx.recv_timeout(Duration::from_millis(1500)).ok().flatten()
}

fn poll_stats(webrtc: gst::Element, shared: Arc<Shared>, alive: Arc<AtomicBool>) {
    let mut prev: Option<(Instant, u64)> = None;
    while alive.load(Ordering::Relaxed) {
        if let Some(s) = fetch_stats(&webrtc) {
            let now = Instant::now();
            let kbps = match prev {
                Some((t0, b0)) if now > t0 => {
                    let dt = now.duration_since(t0).as_secs_f64();
                    let db = s.bytes_received.saturating_sub(b0);
                    if dt >= 0.2 {
                        db as f64 * 8.0 / dt / 1000.0
                    } else {
                        0.0
                    }
                }
                _ => 0.0,
            };
            prev = Some((now, s.bytes_received));
            let total = s.packets_received + s.packets_lost;
            *shared.rtp.lock().unwrap() = Some(RtpStats {
                bitrate_kbps: kbps,
                lost_pct: if total > 0 {
                    s.packets_lost as f64 * 100.0 / total as f64
                } else {
                    0.0
                },
                jitter_ms: s.jitter_ms,
            });
        }
        // tidur ~1 detik, tapi cepat berhenti saat sesi ditutup
        let until = Instant::now() + Duration::from_millis(1000);
        while alive.load(Ordering::Relaxed) && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(50));
        }
    }
    *shared.rtp.lock().unwrap() = None;
}

// ------------------------------------------------------ loop signaling viewer

pub async fn viewer_loop(engine: Arc<Engine>, mut rx: SigRx) {
    engine.hello();
    loop {
        match rx.recv().await {
            Ok((from, text)) => {
                if from == "phone" {
                    engine.on_signal(&text);
                }
            }
            Err(broadcast::error::RecvError::Lagged(_)) => continue,
            Err(_) => break,
        }
    }
}
