//! CamCast Desktop
//!
//! Satu program, tiga bagian:
//!   - window native (egui)         : preview, kontrol rekam, mic, QR
//!   - GStreamer webrtcbin          : terima video WebRTC dari HP, decode, rekam
//!   - server HTTPS (axum, thread)  : menyajikan /phone + signaling

mod engine;
mod server;
mod ui;

use std::sync::Arc;

use eframe::egui;
use tokio::sync::broadcast;

fn main() {
    gst::init().expect("gagal init GStreamer (apakah GStreamer terinstall?)");

    let ip = local_ip_address::local_ip()
        .map(|i| i.to_string())
        .unwrap_or_else(|_| "127.0.0.1".to_string());
    let url = format!("https://{ip}:{}/phone", server::PORT);
    println!("HP buka: {url}");

    let (sig, sig_rx) = broadcast::channel(128);
    let shared = Arc::new(engine::Shared::default());
    let engine = Arc::new(engine::Engine::new(shared.clone(), sig.clone()));

    // runtime tokio di thread terpisah (server + loop signaling viewer)
    {
        let (engine, shared, sig, ip) = (engine.clone(), shared.clone(), sig.clone(), ip.clone());
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .expect("tokio runtime");
            rt.block_on(async move {
                tokio::spawn(engine::viewer_loop(engine, sig_rx));
                if let Err(e) = server::run(ip, sig, shared.clone()).await {
                    shared.set_status(format!("Server error: {e}"));
                    eprintln!("server error: {e}");
                }
            });
        });
    }

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("CamCast")
            .with_inner_size([1180.0, 720.0]),
        ..Default::default()
    };

    eframe::run_native(
        "CamCast",
        options,
        Box::new(move |cc| {
            let _ = shared.ctx.set(cc.egui_ctx.clone());
            Ok(Box::new(ui::CamCastApp::new(shared, engine, url)))
        }),
    )
    .expect("gagal menjalankan window");
}
