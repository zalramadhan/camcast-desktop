//! Window desktop native (egui): preview kamera, kontrol rekam, mic + monitor, QR.

use std::{
    sync::{atomic::Ordering, Arc},
    time::{Duration, Instant},
};

use eframe::egui::{self, Color32, RichText};
use qrcode::QrCode;

use crate::engine::{list_mics, Engine, Mic, Quality, Shared};

const GREEN: Color32 = Color32::from_rgb(22, 163, 74);
const AMBER: Color32 = Color32::from_rgb(217, 119, 6);
const RED: Color32 = Color32::from_rgb(220, 38, 38);

pub struct CamCastApp {
    shared: Arc<Shared>,
    engine: Arc<Engine>,
    tex: Option<egui::TextureHandle>,
    res: (usize, usize),
    mics: Vec<(String, Option<gst::Device>)>,
    mic_sel: usize,
    use_mic: bool,
    monitor_audible: bool,
    bitrate_mbps: u32,
    enc_fps: u32,
    applied: Option<(bool, usize)>,
    meter: f32,
    url: String,
    qr: Option<QrCode>,
    last_count: u64,
    last_tick: Instant,
    fps: f32,
}

impl CamCastApp {
    pub fn new(shared: Arc<Shared>, engine: Arc<Engine>, url: String) -> Self {
        let qr = QrCode::new(url.as_bytes()).ok();
        let q = engine.quality();
        CamCastApp {
            shared,
            engine,
            tex: None,
            res: (0, 0),
            mics: list_mics(),
            mic_sel: 0,
            use_mic: true,
            monitor_audible: false,
            bitrate_mbps: q.bitrate_mbps,
            enc_fps: q.fps,
            applied: None,
            meter: 0.0,
            url,
            qr,
            last_count: 0,
            last_tick: Instant::now(),
            fps: 0.0,
        }
    }

    fn mic_choice(&self) -> Mic {
        if !self.use_mic {
            return Mic::Off;
        }
        match &self.mics[self.mic_sel].1 {
            Some(d) => Mic::Device(d.clone()),
            None => Mic::Default,
        }
    }
}

fn draw_qr(ui: &mut egui::Ui, code: &QrCode, px: f32) {
    let n = code.width();
    let colors = code.to_colors();
    let (rect, _) = ui.allocate_exact_size(egui::vec2(px, px), egui::Sense::hover());
    let p = ui.painter_at(rect);
    p.rect_filled(rect, 0.0, Color32::WHITE);
    let cell = px / (n as f32 + 8.0); // quiet zone 4 modul
    for y in 0..n {
        for x in 0..n {
            if colors[y * n + x] == qrcode::Color::Dark {
                let r = egui::Rect::from_min_size(
                    rect.min + egui::vec2((x as f32 + 4.0) * cell, (y as f32 + 4.0) * cell),
                    egui::vec2(cell, cell),
                );
                p.rect_filled(r, 0.0, Color32::BLACK);
            }
        }
    }
}

/// Bar level mic. `frac` 0..1 (skala -60..0 dBFS).
fn draw_meter(ui: &mut egui::Ui, frac: f32) {
    let w = ui.available_width().min(260.0);
    let (rect, _) = ui.allocate_exact_size(egui::vec2(w, 14.0), egui::Sense::hover());
    let p = ui.painter_at(rect);
    p.rect_filled(rect, 3.0, Color32::from_gray(40));
    let frac = frac.clamp(0.0, 1.0);
    let col = if frac > 0.92 {
        RED
    } else if frac > 0.75 {
        AMBER
    } else {
        GREEN
    };
    let fill = egui::Rect::from_min_size(rect.min, egui::vec2(rect.width() * frac, rect.height()));
    p.rect_filled(fill, 3.0, col);
}

impl eframe::App for CamCastApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // fps
        let dt = self.last_tick.elapsed();
        if dt >= Duration::from_secs(1) {
            let c = self.shared.frame_count.load(Ordering::Relaxed);
            self.fps = (c - self.last_count) as f32 / dt.as_secs_f32();
            self.last_count = c;
            self.last_tick = Instant::now();
        }

        // monitor mic: (re)start kalau pilihan mic berubah
        let want = (self.use_mic, self.mic_sel);
        if self.applied != Some(want) {
            self.applied = Some(want);
            if self.use_mic {
                self.engine.start_mic_monitor(self.mic_choice());
            } else {
                self.engine.stop_mic_monitor();
            }
        }

        // level mic: ambil peak terakumulasi, lalu turun pelan (peak-hold)
        let (peak, last_buf, last_sound) = {
            let mut st = self.shared.mic.lock().unwrap();
            let p = st.peak;
            st.peak = 0.0;
            st.rms = 0.0;
            (p, st.last_buf, st.last_sound)
        };
        self.meter = peak.max(self.meter * 0.88);
        let db = 20.0 * self.meter.max(1e-4).log10();
        let meter_frac = ((db + 60.0) / 60.0).clamp(0.0, 1.0);

        // frame terbaru -> tekstur
        let frame = self.shared.frame.lock().unwrap().take();
        if let Some(f) = frame {
            let img = egui::ColorImage::from_rgba_unmultiplied([f.w, f.h], &f.rgba);
            match &mut self.tex {
                Some(t) => t.set(img, egui::TextureOptions::LINEAR),
                None => {
                    self.tex = Some(ctx.load_texture("video", img, egui::TextureOptions::LINEAR))
                }
            }
            self.res = (f.w, f.h);
        }
        let connected = self.fps > 0.5;
        if !connected && self.tex.is_some() && self.shared.dims.lock().unwrap().0 == 0 {
            self.tex = None; // sesi berakhir
        }

        let rec_secs = self
            .shared
            .rec
            .lock()
            .unwrap()
            .as_ref()
            .map(|r| r.started.elapsed().as_secs());
        let recording = rec_secs.is_some();
        let phone_online = self.shared.phone_online.load(Ordering::Relaxed) > 0;
        let rtp = *self.shared.rtp.lock().unwrap();

        egui::TopBottomPanel::top("top").show(ctx, |ui| {
            ui.horizontal(|ui| {
                let (txt, col) = if connected {
                    ("● Streaming", GREEN)
                } else if phone_online {
                    ("● HP terhubung, menunggu video", AMBER)
                } else {
                    ("● HP offline", Color32::GRAY)
                };
                ui.label(RichText::new(txt).color(col));
                ui.separator();
                if self.res.0 > 0 && connected {
                    let mut line = format!("{}×{}  {:.0} fps", self.res.0, self.res.1, self.fps);
                    let mut warn = false;
                    if let Some(s) = rtp {
                        if s.bitrate_kbps > 0.0 {
                            line.push_str(&format!("  {:.1} Mbps", s.bitrate_kbps / 1000.0));
                        }
                        if s.lost_pct >= 0.5 || s.jitter_ms >= 100.0 {
                            warn = true;
                            line.push_str(&format!(
                                "  · {:.1}% paket hilang / jitter {:.0} ms",
                                s.lost_pct, s.jitter_ms
                            ));
                        }
                    }
                    ui.label(if warn {
                        RichText::new(line).color(AMBER)
                    } else {
                        RichText::new(line)
                    });
                    ui.separator();
                }
                if let Some(s) = rec_secs {
                    ui.label(
                        RichText::new(format!("● REC {:02}:{:02}", s / 60, s % 60)).color(RED),
                    );
                }
            });
        });

        egui::SidePanel::left("side").exact_width(280.0).show(ctx, |ui| {
            ui.add_space(6.0);
            ui.heading("Sambungkan HP");
            ui.label("Scan dengan Chrome di HP (WiFi yang sama):");
            if let Some(qr) = &self.qr {
                draw_qr(ui, qr, 240.0);
            }
            ui.add(egui::Label::new(RichText::new(&self.url).small().monospace()).wrap());
            ui.label(RichText::new("Terima peringatan sertifikat di HP.").small());

            ui.separator();
            ui.heading("Mic PC");
            ui.checkbox(&mut self.use_mic, "Pakai mic (monitor + rekam)");
            ui.add_enabled_ui(self.use_mic && !recording, |ui| {
                egui::ComboBox::from_label("Mic")
                    .selected_text(self.mics[self.mic_sel].0.clone())
                    .show_ui(ui, |ui| {
                        for (i, (name, _)) in self.mics.iter().enumerate() {
                            ui.selectable_value(&mut self.mic_sel, i, name.as_str());
                        }
                    });
            });

            // indikator: mic aktif atau tidak
            let now = Instant::now();
            let (txt, col) = if !self.use_mic {
                ("Mic nonaktif", Color32::GRAY)
            } else if last_buf.map_or(true, |t| now.duration_since(t) > Duration::from_secs(1)) {
                ("⚠ Tidak ada sinyal dari mic", RED)
            } else if last_sound.map_or(false, |t| now.duration_since(t) < Duration::from_secs(3)) {
                ("● Mic aktif (suara terdeteksi)", GREEN)
            } else {
                ("Mic hidup, tapi hening: cek mute / pilih mic lain", AMBER)
            };
            ui.label(RichText::new(txt).color(col));
            draw_meter(ui, if self.use_mic { meter_frac } else { 0.0 });
            if self.use_mic {
                ui.label(RichText::new(format!("{:.0} dB", db.max(-60.0))).small());
            }

            // monitor ke speaker
            let resp = ui.add_enabled(
                self.use_mic,
                egui::Checkbox::new(&mut self.monitor_audible, "Dengar mic di speaker"),
            );
            if resp.changed() {
                self.engine.set_monitor_audible(self.monitor_audible);
            }
            ui.label(
                RichText::new("Pakai headphone, kalau tidak suara bisa menggema (feedback). Ada jeda kecil, itu normal.")
                    .small(),
            );

            ui.separator();
            ui.heading("Kualitas stream");
            let rb = ui.add(egui::Slider::new(&mut self.bitrate_mbps, 2..=20).text("Mbps"));
            let rf = ui.add(egui::Slider::new(&mut self.enc_fps, 10..=30).text("fps"));
            if rb.changed() || rf.changed() {
                self.engine.set_quality(Quality {
                    bitrate_mbps: self.bitrate_mbps,
                    fps: self.enc_fps,
                });
            }
            ui.label(
                RichText::new(
                    "Resolusi tetap 1080p: HP boleh menurunkan fps, bukan mengecilkan gambar. \
                     Berlaku langsung, tanpa memutus koneksi.",
                )
                .small(),
            );

            ui.separator();
            ui.heading("Rekam");
            ui.horizontal(|ui| {
                ui.add_enabled_ui(connected && !recording, |ui| {
                    if ui.button("● Rekam").clicked() {
                        let mic = self.mic_choice();
                        match self.engine.start_recording(mic) {
                            Ok(p) => self.shared.set_status(format!("Merekam ke {p}")),
                            Err(e) => self.shared.set_status(e),
                        }
                    }
                });
                ui.add_enabled_ui(recording, |ui| {
                    if ui.button("■ Stop").clicked() {
                        self.engine.stop_recording();
                    }
                });
            });

            ui.separator();
            let status = self.shared.status.lock().unwrap().clone();
            ui.add(egui::Label::new(RichText::new(status).small()).wrap());
        });

        egui::CentralPanel::default()
            .frame(egui::Frame::none().fill(Color32::BLACK))
            .show(ctx, |ui| match (&self.tex, self.res) {
                (Some(tex), (w, h)) if w > 0 && h > 0 => {
                    let avail = ui.available_size();
                    let scale = (avail.x / w as f32).min(avail.y / h as f32);
                    let size = egui::vec2(w as f32 * scale, h as f32 * scale);
                    ui.centered_and_justified(|ui| {
                        ui.image((tex.id(), size));
                    });
                }
                _ => {
                    ui.centered_and_justified(|ui| {
                        ui.label(RichText::new("Menunggu kamera HP…").color(Color32::GRAY));
                    });
                }
            });

        // meter mic butuh refresh cepat; kalau mic mati cukup santai
        let next = if self.use_mic { 50 } else { 500 };
        ctx.request_repaint_after(Duration::from_millis(next));
    }
}
