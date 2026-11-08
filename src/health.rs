//! Optional health check for uptime monitors. Off unless GAIA_HEALTH_ADDR is
//! set (for example 0.0.0.0:8080). `GET /health` (or `/`) answers `ok` (200) only while
//! the Discord gateway connection is up, `disconnected` (503) otherwise. It
//! reveals nothing else: no guild names, counts or versions.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// Whether the gateway is connected; set from serenity's connection events.
pub static CONNECTED: AtomicBool = AtomicBool::new(false);

pub fn set_connected(up: bool) {
    CONNECTED.store(up, Ordering::SeqCst);
}

/// The full HTTP response for the first line of a request.
fn respond(request_line: &str) -> String {
    let mut parts = request_line.split_whitespace();
    let (method, path) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""));
    let path = path.split('?').next().unwrap_or("");
    let (status, body) = match (method, path) {
        // "/" too: a reverse proxy may strip its path prefix (kori.my/gaia-health → /).
        ("GET" | "HEAD", "/health" | "/") if CONNECTED.load(Ordering::SeqCst) => ("200 OK", "ok"),
        ("GET" | "HEAD", "/health" | "/") => ("503 Service Unavailable", "disconnected"),
        _ => ("404 Not Found", "not found"),
    };
    let body = if method == "HEAD" { "" } else { body };
    format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/plain; charset=utf-8\r\nCache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

/// Serves the health check until the process exits. Each connection gets one
/// small request read with a timeout, so slow or huge requests cost nothing.
pub async fn serve(addr: SocketAddr) -> std::io::Result<()> {
    let listener = TcpListener::bind(addr).await?;
    loop {
        let Ok((mut stream, _)) = listener.accept().await else { continue };
        tokio::spawn(async move {
            let mut buf = [0u8; 1024];
            let read = tokio::time::timeout(Duration::from_secs(5), stream.read(&mut buf)).await;
            let Ok(Ok(n)) = read else { return };
            let text = String::from_utf8_lossy(&buf[..n]);
            let line = text.lines().next().unwrap_or("");
            let _ = stream.write_all(respond(line).as_bytes()).await;
            let _ = stream.shutdown().await;
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn answers_only_what_monitors_need() {
        set_connected(false);
        assert!(respond("GET /health HTTP/1.1").starts_with("HTTP/1.1 503"));
        set_connected(true);
        let ok = respond("GET /health?x=1 HTTP/1.1");
        assert!(ok.starts_with("HTTP/1.1 200") && ok.ends_with("\r\n\r\nok"));
        assert!(respond("HEAD /health HTTP/1.1").ends_with("\r\n\r\n"));
        assert!(respond("GET / HTTP/1.1").starts_with("HTTP/1.1 200"));
        assert!(respond("GET /admin HTTP/1.1").starts_with("HTTP/1.1 404"));
        assert!(respond("POST /health HTTP/1.1").starts_with("HTTP/1.1 404"));
        assert!(respond("").starts_with("HTTP/1.1 404"));
        set_connected(false);
    }
}
