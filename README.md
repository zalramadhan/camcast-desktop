# CamCast Desktop v0.3

HP Android (kamera) -> app desktop native di Linux (preview + rekam + mic PC).

## Arsitektur
Satu program Rust:
- **egui/eframe**: window native (preview, tombol rekam, pilih mic, QR untuk HP).
- **GStreamer webrtcbin**: menerima video WebRTC dari HP, decode VP8, tampilkan.
  Jitter buffer diset 30 ms (default GStreamer 200 ms) untuk latensi rendah.
- **axum (thread terpisah)**: HTTPS self-signed, menyajikan `/phone` dan relay signaling.
- **Rekam**: paket VP8 dari HP disalin langsung (tanpa re-encode) + audio mic PC (Opus)
  ke `~/Videos/CamCast/rec_*.mkv`, lalu dikonversi ke `.mp4` dengan ffmpeg (opsional).

## Instal dependensi (Ubuntu/Debian)
    sudo apt install build-essential pkg-config ffmpeg \
      libgstreamer1.0-dev libgstreamer-plugins-base1.0-dev libgstreamer-plugins-bad1.0-dev \
      gstreamer1.0-plugins-base gstreamer1.0-plugins-good gstreamer1.0-plugins-bad \
      gstreamer1.0-nice gstreamer1.0-pulseaudio \
      libxkbcommon-dev libwayland-dev libx11-dev libgl1-mesa-dev
Rust: https://rustup.rs

Cek plugin: `gst-inspect-1.0 webrtcbin vp8dec matroskamux opusenc`

## Pasang sebagai aplikasi (sekali klik)
    ./install.sh
Script ini build release, memasang `~/.local/bin/camcast`, ikon di menu aplikasi, dan
shortcut di Desktop. Setelah itu cukup klik ikon **CamCast**. Di HP, tekan Mulai satu kali;
sesudahnya halaman /phone otomatis menyala lagi setiap dibuka (simpan sebagai bookmark),
dan otomatis menyambung ulang kalau app di PC baru dibuka belakangan.
Ingin berhenti auto-start di HP? Tekan Stop di halaman /phone.
Untuk melihat log/error, jalankan `camcast` dari terminal.

## Jalankan manual
    cargo run --release
1. Window CamCast terbuka dan menampilkan QR.
2. Scan QR dengan Chrome di HP (WiFi sama), terima peringatan sertifikat, tekan Mulai.
3. Video muncul di window. Atur mic, lalu tekan Rekam / Stop.

## Monitor mic
- Selama kotak **Pakai mic** dicentang, meter level selalu jalan (sebelum dan saat rekam).
  Indikator: hijau = suara terdeteksi, kuning = hidup tapi hening (cek mute), merah = tidak ada sinyal.
- **Dengar mic di speaker** (default mati) memutar suara mic ke speaker. Pakai headphone supaya tidak feedback.
  Ada jeda kecil (~0,1-0,3 detik) karena buffer audio sistem; itu normal.

## Troubleshooting
- Tidak tersambung: buka firewall, mis. `sudo ufw allow from 192.168.0.0/16` (UDP acak dipakai WebRTC)
  dan `sudo ufw allow 8443/tcp`.
- Tidak ada video: pastikan `gstreamer1.0-nice` dan `gstreamer1.0-plugins-bad` terpasang.
- Mic kosong: pilih perangkat lain di dropdown Mic, atau cek `pavucontrol`.
- Rekaman tanpa MP4: install ffmpeg; file `.mkv` tetap tersimpan.
- Ikon Desktop tidak bisa diklik: klik kanan -> Allow Launching (GNOME) / Trust (lainnya).
