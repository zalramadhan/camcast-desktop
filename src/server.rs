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

use crate::engine::{config_dir, Shared, Sig};

pub const PORT: u16 = 8443;
const PHONE_HTML: &str = include_str!("../static/phone.html");

const CERT_FILE: &str = "cert.pem";
const KEY_FILE: &str = "key.pem";
const HOSTS_FILE: &str = "tls-hosts.txt";

/// Sertifikat self-signed yang dipakai ulang antar-jalan app, supaya HP cukup
/// sekali menekan "Terima peringatan sertifikat". Dibuat ulang hanya kalau host/IP
/// yang harus dicakup berubah (mis. IP LAN berganti karena DHCP).
fn load_or_create_cert(hosts: &[String]) -> Result<(String, String), Box<dyn std::error::Error>> {
    let dir = config_dir();
    if let Some(dir) = &dir {
        let (cert, key) = (dir.join(CERT_FILE), dir.join(KEY_FILE));
        let hosts_file = dir.join(HOSTS_FILE);
        if let (Ok(saved), Ok(c), Ok(k)) = (
            std::fs::read_to_string(&hosts_file),
            std::fs::read_to_string(&cert),
            std::fs::read_to_string(&key),
        ) {
            let covered = hosts
                .iter()
                .all(|h| saved.lines().any(|s| s.trim() == h));
            // ditambah cert/key tidak kosong, untuk berjaga file rusak
            if covered && !c.trim().is_empty() && !k.trim().is_empty() {
                return Ok((c, k));
            }
        }
    }

    let ck = rcgen::generate_simple_self_signed(hosts.to_vec())?;
    let (cert_pem, key_pem) = (ck.cert.pem(), ck.key_pair.serialize_pem());
    if let Some(dir) = &dir {
        let _ = std::fs::create_dir_all(dir);
        let _ = std::fs::write(dir.join(CERT_FILE), &cert_pem);
        let _ = std::fs::write(dir.join(KEY_FILE), &key_pem);
        let _ = std::fs::write(dir.join(HOSTS_FILE), hosts.join("\n"));
        // kunci privat: hanya pemilik yang boleh membaca
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(
                dir.join(KEY_FILE),
                std::fs::Permissions::from_mode(0o600),
            );
        }
    }
    Ok((cert_pem, key_pem))
}

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
    // Sertifikat self-signed (kamera & WebRTC di HP wajib HTTPS), disimpan agar
    // peringatan sertifikat di HP hanya muncul sekali.
    let hosts = vec![ip.clone(), "localhost".to_string()];
    let (cert_pem, key_pem) = load_or_create_cert(&hosts)?;
    let tls = RustlsConfig::from_pem(cert_pem.into_bytes(), key_pem.into_bytes()).await?;

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

#[cfg(test)]
mod tests {
    use super::load_or_create_cert;

    #[test]
    fn sertifikat_dipakai_ulang_selama_host_sama() {
        // jangan sentuh config asli milik user
        let dir = std::env::temp_dir().join(format!("camcast-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::env::set_var("XDG_CONFIG_HOME", &dir);

        let hosts = vec!["192.168.1.5".to_string(), "localhost".to_string()];
        let (cert1, key1) = load_or_create_cert(&hosts).unwrap();
        let (cert2, key2) = load_or_create_cert(&hosts).unwrap();
        assert_eq!(cert1, cert2, "cert harus dipakai ulang, bukan dibuat baru");
        assert_eq!(key1, key2, "key harus dipakai ulang");

        // IP berubah -> sertifikat baru, karena SAN harus mencakup IP itu
        let other = vec!["10.0.0.9".to_string(), "localhost".to_string()];
        let (cert3, _) = load_or_create_cert(&other).unwrap();
        assert_ne!(cert1, cert3, "host berbeda harus membuat sertifikat baru");

        let _ = std::fs::remove_dir_all(&dir);
    }
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
