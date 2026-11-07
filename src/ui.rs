//! Window desktop native (egui): preview kamera, kontrol rekam, mic + monitor, QR.

use std::{
    sync::{atomic::Ordering, Arc},
    time::{Duration, Instant},
};

use eframe::egui::{self, Color32, RichText};
use qrcode::QrCode;

use crate::engine::{list_mics, Engine, Level, Look, Mic, Quality, Shared};

const GREEN: Color32 = Color32::from_rgb(22, 163, 74);
const AMBER: Color32 = Color32::from_rgb(217, 119, 6);
const RED: Color32 = Color32::from_rgb(220, 38, 38);
const HINT: Color32 = Color32::from_gray(170);

fn level_color(l: Level) -> Color32 {
    match l {
        Level::Info => Color32::from_gray(200),
        Level::Ok => GREEN,
        Level::Warn => AMBER,
        Level::Error => RED,
    }
}

fn level_icon(l: Level) -> &'static str {
    match l {
        Level::Info => "•",
        Level::Ok => "●",
        Level::Warn => "⚠",
        Level::Error => "✕",
    }
}

pub struct CamCastApp {
    shared: Arc<Shared>,
    engine: Arc<Engine>,
    tex: Option<egui::TextureHandle>,
    res: (usize, usize),
    mics: Vec<(String, Option<gst::Device>)>,
    mic_sel: usize,
    /// Mic hidup (meter + monitor). Kalau mati, mic sama sekali tidak dibuka.
    use_mic: bool,
    /// Masukkan suara mic ke rekaman.
    record_mic: bool,
    monitor_audible: bool,
    bitrate_mbps: u32,
    enc_fps: u32,
    /// -50..50 (%) -> videobalance brightness -0.5..0.5
    brightness: i32,
    /// 50..150 (%) -> videobalance contrast 0.5..1.5
    contrast: i32,
    mirror: bool,
    fullscreen: bool,
    /// Kunci fokus (single-shot) di HP.
    focus_locked: bool,
    /// Titik fokus dalam koordinat preview 0..1 (untuk reticle).
    focus_pt: Option<(f32, f32)>,
    /// Frame terakhir yang tampil (untuk simpan foto).
    last_frame: Option<(usize, usize, Vec<u8>)>,
    applied: Option<(bool, usize)>,
    meter: f32,
    url: String,
    qr: Option<QrCode>,
    was_online: bool,
    last_count: u64,
    last_tick: Instant,
    fps: f32,
}

impl CamCastApp {
    pub fn new(shared: Arc<Shared>, engine: Arc<Engine>, url: String) -> Self {
        let qr = QrCode::new(url.as_bytes()).ok();
        let q = engine.quality();
        let l = engine.look();
        CamCastApp {
            shared,
            engine,
            tex: None,
            res: (0, 0),
            mics: list_mics(),
            mic_sel: 0,
            use_mic: true,
            record_mic: true,
            monitor_audible: false,
            bitrate_mbps: q.bitrate_mbps,
            enc_fps: q.fps,
            brightness: (l.brightness * 100.0).round() as i32,
            contrast: (l.contrast * 100.0).round() as i32,
            mirror: l.mirror,
            fullscreen: false,
            focus_locked: false,
            focus_pt: None,
            last_frame: None,
            applied: None,
            meter: 0.0,
            url,
            qr,
            was_online: false,
            last_count: 0,
            last_tick: Instant::now(),
            fps: 0.0,
        }
    }

    /// Mic untuk masuk ke rekaman (Off kalau tidak dipakai sama sekali).
    fn record_mic_choice(&self) -> Mic {
        if !self.use_mic || !self.record_mic {
            return Mic::Off;
        }
        self.monitor_mic()
    }

    /// Mic untuk monitor/meter; tidak pernah Off.
    fn monitor_mic(&self) -> Mic {
        match &self.mics[self.mic_sel].1 {
            Some(d) => Mic::Device(d.clone()),
            None => Mic::Default,
        }
    }

    /// Kirim koreksi tampilan ke preview (langsung, tanpa restart).
    fn push_look(&mut self) {
        self.engine.set_look(Look {
            brightness: self.brightness as f32 / 100.0,
            contrast: self.contrast as f32 / 100.0,
            mirror: self.mirror,
        });
    }

    /// Kirim titik fokus ke HP. `focus_pt` disimpan dalam koordinat preview,
    /// jadi dibalik lagi kalau gambar sedang dicerminkan.
    fn push_focus(&mut self) {
        let (px, py) = self.focus_pt.unwrap_or((0.5, 0.5));
        let x = if self.mirror { 1.0 - px } else { px };
        self.engine.set_focus(self.focus_locked, x, py);
    }

    fn take_photo(&mut self) {
        let Some((w, h, rgba)) = self.last_frame.clone() else {
            self.shared.set_warn("Belum ada frame untuk difoto");
            return;
        };
        match self.engine.take_photo(rgba, w, h) {
            Ok(p) => self.shared.set_status(format!("Menyimpan foto ke {p}")),
            Err(e) => self.shared.set_error(e),
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

/// Bar level mic dengan skala dB. `frac` 0..1 memetakan -60..0 dBFS.
fn draw_meter(ui: &mut egui::Ui, frac: f32) {
    let w = ui.available_width().min(260.0);
    let (rect, _) = ui.allocate_exact_size(egui::vec2(w, 26.0), egui::Sense::hover());
    let bar = egui::Rect::from_min_size(rect.min, egui::vec2(rect.width(), 14.0));
    let p = ui.painter_at(rect);
    p.rect_filled(bar, 3.0, Color32::from_gray(40));

    let frac = frac.clamp(0.0, 1.0);
    let col = if frac > 0.92 {
        RED
    } else if frac > 0.75 {
        AMBER
    } else {
        GREEN
    };
    let fill = egui::Rect::from_min_size(bar.min, egui::vec2(bar.width() * frac, bar.height()));
    p.rect_filled(fill, 3.0, col);

    // penanda skala: -40, -20, -6 dBFS
    for db in [-40.0_f32, -20.0, -6.0] {
        let f = ((db + 60.0) / 60.0).clamp(0.0, 1.0);
        let x = bar.left() + bar.width() * f;
        p.line_segment(
            [egui::pos2(x, bar.top()), egui::pos2(x, bar.bottom())],
            egui::Stroke::new(1.0_f32, Color32::from_gray(115)),
        );
        p.text(
            egui::pos2(x, bar.bottom() + 2.0),
            egui::Align2::CENTER_TOP,
            format!("{db:.0}"),
            egui::FontId::proportional(9.0),
            HINT,
        );
    }

    // indikator clip di ujung kanan bar
    if frac > 0.98 {
        p.text(
            egui::pos2(bar.right() - 3.0, bar.center().y),
            egui::Align2::RIGHT_CENTER,
            "CLIP",
            egui::FontId::proportional(9.0),
            Color32::WHITE,
        );
    }
}

impl eframe::App for CamCastApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // fps: frame yang benar-benar diterima dari HP
        let dt = self.last_tick.elapsed();
        if dt >= Duration::from_secs(1) {
            let c = self.shared.frame_count.load(Ordering::Relaxed);
            self.fps = (c - self.last_count) as f32 / dt.as_secs_f32();
            self.last_count = c;
            self.last_tick = Instant::now();
        }

        // monitor mic: (re)start kalau mic dinyalakan / pilihan perangkat berubah
        let want = (self.use_mic, self.mic_sel);
        if self.applied != Some(want) {
            self.applied = Some(want);
            if self.use_mic {
                self.engine.start_mic_monitor(self.monitor_mic());
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
            self.last_frame = Some((f.w, f.h, f.rgba));
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
        let rec_path = self
            .shared
            .rec
            .lock()
            .unwrap()
            .as_ref()
            .map(|r| r.path.clone());
        let recording = rec_secs.is_some();
        let phone_online = self.shared.phone_online.load(Ordering::Relaxed) > 0;
        let rtp = *self.shared.rtp.lock().unwrap();
        let (status_level, status_text) = {
            let s = self.shared.status.lock().unwrap();
            (s.level, s.text.clone())
        };
        let log_lines: Vec<(Level, String)> = {
            let log = self.shared.log.lock().unwrap();
            log.iter().cloned().collect()
        };
        // QR ditutup otomatis saat HP pertama kali tersambung (masih bisa dibuka lagi)
        let close_qr = phone_online && !self.was_online;
        self.was_online = phone_online;

        // Shortcut: Space = rekam/stop, F = fullscreen, Esc = keluar fullscreen.
        // Dilewati kalau ada widget yang sedang fokus, supaya tidak bentrok.
        if ctx.memory(|m| m.focused()).is_none()
            && ctx.input(|i| i.key_pressed(egui::Key::Space))
        {
            if recording {
                self.engine.stop_recording();
            } else if connected {
                let mic = self.record_mic_choice();
                match self.engine.start_recording(mic) {
                    Ok(p) => self.shared.set_ok(format!("Merekam ke {p}")),
                    Err(e) => self.shared.set_error(e),
                }
            }
        }
        if ctx.input(|i| i.key_pressed(egui::Key::F)) {
            self.fullscreen = !self.fullscreen;
            ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(self.fullscreen));
        }
        if self.fullscreen && ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.fullscreen = false;
            ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(false));
        }
        if ctx.input(|i| i.key_pressed(egui::Key::S)) {
            self.take_photo();
        }

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
                    // lebar tetap supaya baris tidak bergeser tiap angka berubah
                    let mut line = format!("{:>4}×{:<4}", self.res.0, self.res.1);
                    line.push_str(&format!(" {:>4.0} fps", self.fps));
                    let mut warn = false;
                    if let Some(s) = rtp {
                        if s.bitrate_kbps > 0.0 {
                            line.push_str(&format!("  {:>5.1} Mbps", s.bitrate_kbps / 1000.0));
                        }
                        if s.lost_pct >= 0.5 || s.jitter_ms >= 100.0 {
                            warn = true;
                            line.push_str(&format!(
                                "  · {:.1}% hilang / {:.0} ms jitter",
                                s.lost_pct, s.jitter_ms
                            ));
                        }
                    }
                    let text = RichText::new(line).monospace();
                    ui.label(if warn { text.color(AMBER) } else { text });
                }
            });
        });

        // Aksi utama selalu terlihat: rekam, durasi, path, dan status.
        egui::TopBottomPanel::bottom("actions").show(ctx, |ui| {
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                let label = if recording { "■  Stop" } else { "●  Rekam" };
                let btn = egui::Button::new(RichText::new(label).size(16.0).color(Color32::WHITE))
                    .min_size(egui::vec2(120.0, 34.0))
                    .fill(if recording { RED } else { GREEN });
                if ui.add_enabled(recording || connected, btn).clicked() {
                    if recording {
                        self.engine.stop_recording();
                    } else {
                        let mic = self.record_mic_choice();
                        match self.engine.start_recording(mic) {
                            Ok(p) => self.shared.set_ok(format!("Merekam ke {p}")),
                            Err(e) => self.shared.set_error(e),
                        }
                    }
                }
                ui.add_enabled_ui(self.last_frame.is_some(), |ui| {
                    if ui
                        .button("📷 Foto")
                        .on_hover_text("Simpan frame ini sebagai PNG (S)")
                        .clicked()
                    {
                        self.take_photo();
                    }
                });
                match (rec_secs, rec_path) {
                    (Some(s), path) => {
                        ui.label(
                            RichText::new(format!("{:02}:{:02}", s / 60, s % 60))
                                .monospace()
                                .size(18.0)
                                .color(RED),
                        );
                        if let Some(p) = path {
                            ui.add(
                                egui::Label::new(RichText::new(p).small().monospace().color(HINT))
                                    .wrap(),
                            );
                        }
                    }
                    (None, _) if !connected => {
                        ui.label(
                            RichText::new("Tombol aktif setelah video HP masuk")
                                .small()
                                .color(HINT),
                        );
                    }
                    _ => {}
                }
            });
            ui.add_space(2.0);
            ui.add(
                egui::Label::new(
                    RichText::new(format!(
                        "{} {}",
                        level_icon(status_level),
                        if status_text.is_empty() {
                            "Siap"
                        } else {
                            status_text.as_str()
                        }
                    ))
                    .small()
                    .color(level_color(status_level)),
                )
                .wrap(),
            );
            ui.add_space(4.0);
        });

        egui::SidePanel::left("side").exact_width(300.0).show(ctx, |ui| {
            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    ui.add_space(6.0);
                    ui.heading("Sambungkan HP");

                    ui.horizontal(|ui| {
                        ui.add(
                            egui::Label::new(RichText::new(&self.url).small().monospace())
                                .wrap(),
                        );
                    });
                    if ui.small_button("Salin tautan").clicked() {
                        let url = self.url.clone();
                        ui.output_mut(|o| o.copied_text = url);
                        self.shared.set_status("Tautan disalin");
                    }
                    if self.qr.is_some() {
                        let id = ui.make_persistent_id("qr_hp");
                        let mut state =
                            egui::collapsing_header::CollapsingState::load_with_default_open(
                                ui.ctx(),
                                id,
                                !phone_online,
                            );
                        if close_qr {
                            state.set_open(false);
                        }
                        state
                            .show_header(ui, |ui| {
                                ui.label(RichText::new("QR untuk HP").small().color(HINT));
                            })
                            .body(|ui| {
                                if let Some(qr) = &self.qr {
                                    let px = (ui.available_width() - 8.0).clamp(120.0, 240.0);
                                    draw_qr(ui, qr, px);
                                }
                                ui.add(
                                    egui::Label::new(
                                        RichText::new(
                                            "Scan dengan Chrome di HP (WiFi sama), lalu terima \
                                             peringatan sertifikat.",
                                        )
                                        .small()
                                        .color(HINT),
                                    )
                                    .wrap(),
                                );
                            });
                    }

                    ui.separator();
                    ui.heading("Mic PC");
                    ui.checkbox(&mut self.use_mic, "Aktifkan mic (meter hidup)");
                    ui.add_enabled_ui(self.use_mic, |ui| {
                        ui.checkbox(&mut self.record_mic, "Rekam suara mic");
                    });
                    ui.horizontal(|ui| {
                        ui.add_enabled_ui(self.use_mic && !recording, |ui| {
                            egui::ComboBox::from_id_salt("mic")
                                .selected_text(self.mics[self.mic_sel].0.clone())
                                .width(190.0)
                                .show_ui(ui, |ui| {
                                    for (i, (name, _)) in self.mics.iter().enumerate() {
                                        ui.selectable_value(&mut self.mic_sel, i, name.as_str());
                                    }
                                });
                        });
                        if ui
                            .small_button("↻")
                            .on_hover_text("Muat ulang daftar mic")
                            .clicked()
                        {
                            self.mics = list_mics();
                            if self.mics.is_empty() {
                                self.mics.push(("Default sistem".to_string(), None));
                            }
                            self.mic_sel = self.mic_sel.min(self.mics.len() - 1);
                            self.applied = None; // paksa monitor restart
                        }
                    });
                    if recording && self.use_mic {
                        ui.label(
                            RichText::new("Mic dikunci selama merekam").small().color(HINT),
                        );
                    }

                    let now = Instant::now();
                    let (txt, col) = if !self.use_mic {
                        ("Mic nonaktif", Color32::GRAY)
                    } else if last_buf.map_or(true, |t| now.duration_since(t) > Duration::from_secs(1))
                    {
                        ("⚠ Tidak ada sinyal dari mic", RED)
                    } else if last_sound
                        .map_or(false, |t| now.duration_since(t) < Duration::from_secs(3))
                    {
                        ("● Suara terdeteksi", GREEN)
                    } else {
                        ("Hening: cek mute / pilih mic lain", AMBER)
                    };
                    ui.label(RichText::new(txt).small().color(col));
                    draw_meter(ui, if self.use_mic { meter_frac } else { 0.0 });
                    if self.use_mic {
                        ui.label(
                            RichText::new(format!("{:.0} dB", db.max(-60.0)))
                                .small()
                                .monospace()
                                .color(HINT),
                        );
                    }

                    let resp = ui.add_enabled(
                        self.use_mic,
                        egui::Checkbox::new(&mut self.monitor_audible, "Dengar mic di speaker"),
                    );
                    if resp.changed() {
                        self.engine.set_monitor_audible(self.monitor_audible);
                    }
                    ui.add(
                        egui::Label::new(
                            RichText::new(
                                "Pakai headphone supaya tidak feedback. Ada jeda kecil karena \
                                 buffer audio sistem; itu normal.",
                            )
                            .small()
                            .color(HINT),
                        )
                        .wrap(),
                    );

                    ui.separator();
                    ui.heading("Tampilan kamera");
                    let lb = ui.add(
                        egui::Slider::new(&mut self.brightness, -50..=50)
                            .text("cerah %")
                            .fixed_decimals(0),
                    );
                    let lc = ui.add(
                        egui::Slider::new(&mut self.contrast, 50..=150)
                            .text("kontras %")
                            .fixed_decimals(0),
                    );
                    if lb.changed() || lc.changed() {
                        self.push_look();
                    }
                    if ui.checkbox(&mut self.mirror, "Cermin (mirror)").changed() {
                        self.push_look();
                    }
                    if ui
                        .checkbox(&mut self.focus_locked, "Kunci fokus")
                        .on_hover_text("Klik preview untuk pilih titik fokus")
                        .changed()
                    {
                        self.push_focus();
                    }
                    ui.horizontal(|ui| {
                        if ui
                            .button("Balik kamera")
                            .on_hover_text("Depan ⇄ belakang (lewat HP)")
                            .clicked()
                        {
                            self.engine.flip_camera();
                            self.shared.set_status("Minta HP berganti kamera");
                        }
                        if ui
                            .button("Reset")
                            .on_hover_text("Kembalikan cerah/kontras/cermin")
                            .clicked()
                        {
                            self.brightness = 0;
                            self.contrast = 100;
                            self.mirror = false;
                            self.push_look();
                        }
                    });
                    ui.add(
                        egui::Label::new(
                            RichText::new(
                                "Efek tampil di preview; rekaman mengikuti saat konversi MP4. \
                                 Balik kamera butuh HP sedang streaming.",
                            )
                            .small()
                            .color(HINT),
                        )
                        .wrap(),
                    );

                    ui.separator();
                    ui.heading("Kualitas stream");
                    let rb = ui.add(
                        egui::Slider::new(&mut self.bitrate_mbps, 2..=20)
                            .text("Mbps")
                            .fixed_decimals(0),
                    );
                    let rf = ui.add(
                        egui::Slider::new(&mut self.enc_fps, 10..=30)
                            .text("fps")
                            .fixed_decimals(0),
                    );
                    if rb.changed() || rf.changed() {
                        self.engine.set_quality(Quality {
                            bitrate_mbps: self.bitrate_mbps,
                            fps: self.enc_fps,
                        });
                    }
                    ui.add(
                        egui::Label::new(
                            RichText::new(
                                "Resolusi tetap 1080p: HP boleh menurunkan fps, bukan \
                                 mengecilkan gambar. Berlaku langsung, tanpa memutus koneksi.",
                            )
                            .small()
                            .color(HINT),
                        )
                        .wrap(),
                    );

                    ui.separator();
                    egui::CollapsingHeader::new("Log")
                        .default_open(false)
                        .show(ui, |ui| {
                            if log_lines.is_empty() {
                                ui.label(RichText::new("Belum ada pesan.").small().color(HINT));
                                return;
                            }
                            for (lvl, line) in &log_lines {
                                ui.add(
                                    egui::Label::new(
                                        RichText::new(format!("{} {}", level_icon(*lvl), line))
                                            .small()
                                            .color(level_color(*lvl)),
                                    )
                                    .wrap(),
                                );
                            }
                        });
                    ui.add_space(6.0);
                    ui.add(
                        egui::Label::new(
                            RichText::new("Space = rekam/stop · S = foto · F = fullscreen")
                                .small()
                                .color(HINT),
                        )
                        .wrap(),
                    );
                    ui.add_space(8.0);
                });
        });

        let tex_id = self.tex.as_ref().map(|t| t.id());
        egui::CentralPanel::default()
            .frame(egui::Frame::none().fill(Color32::BLACK))
            .show(ctx, |ui| match (tex_id, self.res) {
                (Some(tex_id), (w, h)) if w > 0 && h > 0 => {
                    let avail = ui.available_size();
                    let scale = (avail.x / w as f32).min(avail.y / h as f32);
                    let size = egui::vec2(w as f32 * scale, h as f32 * scale);
                    let resp = ui.centered_and_justified(|ui| ui.image((tex_id, size))).inner;
                    let rect = resp.rect;
                    // klik di preview = titik fokus (koordinat ternormalisasi 0..1)
                    if resp.clicked() {
                        if let Some(pos) = resp.interact_pointer_pos() {
                            let nx = ((pos.x - rect.left()) / rect.width()).clamp(0.0, 1.0);
                            let ny = ((pos.y - rect.top()) / rect.height()).clamp(0.0, 1.0);
                            self.focus_pt = Some((nx, ny));
                            self.push_focus();
                        }
                    }
                    if let Some(s) = rec_secs {
                        let p = ui.painter();
                        p.rect_stroke(rect, 0.0, egui::Stroke::new(3.0_f32, RED));
                        let galley = p.layout_no_wrap(
                            format!("● REC {:02}:{:02}", s / 60, s % 60),
                            egui::FontId::monospace(16.0),
                            Color32::WHITE,
                        );
                        let pos = rect.left_top() + egui::vec2(12.0, 12.0);
                        p.rect_filled(
                            egui::Rect::from_min_size(
                                pos - egui::vec2(6.0, 4.0),
                                galley.size() + egui::vec2(12.0, 8.0),
                            ),
                            4.0,
                            Color32::from_black_alpha(190),
                        );
                        p.galley(pos, galley, RED);
                    }
                    // penanda titik fokus
                    if let Some((fx, fy)) = self.focus_pt {
                        let c = rect.min + egui::vec2(fx * rect.width(), fy * rect.height());
                        let p = ui.painter();
                        let r = egui::Rect::from_center_size(c, egui::vec2(26.0, 26.0));
                        p.rect_stroke(r, 2.0, egui::Stroke::new(1.5_f32, Color32::WHITE));
                        let g = egui::Stroke::new(1.0_f32, Color32::from_white_alpha(170));
                        for (a, b) in [
                            (egui::vec2(-17.0, 0.0), egui::vec2(-7.0, 0.0)),
                            (egui::vec2(7.0, 0.0), egui::vec2(17.0, 0.0)),
                            (egui::vec2(0.0, -17.0), egui::vec2(0.0, -7.0)),
                            (egui::vec2(0.0, 7.0), egui::vec2(0.0, 17.0)),
                        ] {
                            p.line_segment([c + a, c + b], g);
                        }
                    }
                }
                _ => {
                    ui.centered_and_justified(|ui| {
                        ui.vertical_centered(|ui| {
                            if phone_online {
                                ui.label(
                                    RichText::new("HP sudah tersambung")
                                        .size(18.0)
                                        .strong(),
                                );
                                ui.add_space(4.0);
                                ui.label(
                                    RichText::new("Tekan Mulai di halaman /phone untuk mengirim video.")
                                        .color(AMBER),
                                );
                            } else {
                                ui.label(RichText::new("HP belum tersambung").size(18.0).strong());
                                ui.add_space(4.0);
                                ui.label(
                                    RichText::new("Buka tautan ini di Chrome pada HP:")
                                        .color(HINT),
                                );
                                ui.label(
                                    RichText::new(&self.url).monospace().color(Color32::WHITE),
                                );
                                ui.add_space(4.0);
                                ui.label(
                                    RichText::new(
                                        "QR ada di panel kiri (klik \"QR untuk HP\").",
                                    )
                                    .small()                                        .color(HINT),
                                );
                            }
                        });
                    });
                }
            });

        // meter mic butuh refresh cepat; kalau mic mati cukup santai
        let next = if self.use_mic { 50 } else { 250 };
        ctx.request_repaint_after(Duration::from_millis(next));
    }
}
