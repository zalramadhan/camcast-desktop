//! Server HTTPS kecil: menyajikan /phone dan relay signaling WebSocket dari HP.
//! (Sisi viewer ada di dalam proses yang sama, lewat channel `sig`.)

use std::{
    net::SocketAddr,
    sync::{atomic::Ordering, Arc},
};

use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        State,
    },
    response::{Html, IntoResponse},
    routing::get,
    Router,
};
use axum_server::tls_rustls::RustlsConfig;
use tokio::sync::broadcast;

use crate::engine::{Shared, Sig};

pub const PORT: u16 = 8443;
const PHONE_HTML: &str = include_str!("../static/phone.html");

#[derive(Clone)]
struct SigState {
    sig: Sig,
    shared: Arc<Shared>,
}

pub async fn run(
    ip: String,
    sig: Sig,
    shared: Arc<Shared>,
) -> Result<(), Box<dyn std::error::Error>> {
    // Sertifikat self-signed (kamera & WebRTC di HP wajib HTTPS)
    let ck = rcgen::generate_simple_self_signed(vec![ip, "localhost".to_string()])?;
    let tls = RustlsConfig::from_pem(
        ck.cert.pem().into_bytes(),
        ck.key_pair.serialize_pem().into_bytes(),
    )
    .await?;

    let app = Router::new()
        .route("/", get(|| async { "CamCast aktif. Buka /phone di HP." }))
        .route("/phone", get(|| async { Html(PHONE_HTML) }))
        .route("/ws/signal/phone", get(phone_ws))
        .with_state(SigState { sig, shared });

    let addr = SocketAddr::from(([0, 0, 0, 0], PORT));
    axum_server::bind_rustls(addr, tls)
        .serve(app.into_make_service())
        .await?;
    Ok(())
}

async fn phone_ws(ws: WebSocketUpgrade, State(s): State<SigState>) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_phone(socket, s))
}

async fn handle_phone(mut socket: WebSocket, s: SigState) {
    s.shared.phone_online.fetch_add(1, Ordering::Relaxed);
    s.shared.repaint();
    let mut rx = s.sig.subscribe();
    loop {
        tokio::select! {
            m = rx.recv() => match m {
                Ok((from, text)) => {
                    // teruskan hanya pesan dari viewer (app desktop) ke HP
                    if from == "viewer" && socket.send(Message::Text(text)).await.is_err() {
                        break;
                    }
                }
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(_) => break,
            },
            msg = socket.recv() => match msg {
                Some(Ok(Message::Text(t))) => {
                    let _ = s.sig.send(("phone", t));
                }
                Some(Ok(Message::Close(_))) | Some(Err(_)) | None => break,
                _ => {}
            },
        }
    }
    s.shared.phone_online.fetch_sub(1, Ordering::Relaxed);
    let _ = s.sig.send(("phone", r#"{"type":"bye"}"#.to_string()));
    s.shared.repaint();
}
