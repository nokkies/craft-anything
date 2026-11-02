//! A tiny static file server on loopback, for previewing and checking a built website.
//!
//! GET/HEAD only, a bounded number of short-lived connection threads, paths confined to the root
//! (no `..`, symlinks resolved and re-checked), Host/Origin validated against loopback, unknown
//! extension-less paths fall back to `index.html` (single-page apps). It stops when the
//! [`StaticServer`] is dropped.

use crate::standin_server::{host_is_loopback, origin_is_local};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

/// Most connections served at once; the rest are refused rather than spawning unbounded threads.
const MAX_CONNECTIONS: usize = 32;
/// Largest request head (request line + headers) we read.
const MAX_HEAD_BYTES: u64 = 16 * 1024;

pub struct StaticServer {
    pub port: u16,
    pub root: PathBuf,
    stop: Arc<AtomicBool>,
}

impl StaticServer {
    pub fn url(&self) -> String {
        format!("http://127.0.0.1:{}/", self.port)
    }
}

impl Drop for StaticServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

/// Serve `root` on `127.0.0.1` at a free port.
pub fn serve_dir(root: &Path) -> std::io::Result<StaticServer> {
    let root = std::fs::canonicalize(root)?;
    let listener = TcpListener::bind("127.0.0.1:0")?;
    listener.set_nonblocking(true)?;
    let port = listener.local_addr()?.port();
    let stop = Arc::new(AtomicBool::new(false));
    let active = Arc::new(AtomicUsize::new(0));
    let (r, s, a) = (root.clone(), stop.clone(), active.clone());
    std::thread::Builder::new().name("static-server".into()).spawn(move || {
        while !s.load(Ordering::SeqCst) {
            match listener.accept() {
                Ok((stream, _)) => {
                    if a.fetch_add(1, Ordering::SeqCst) >= MAX_CONNECTIONS {
                        a.fetch_sub(1, Ordering::SeqCst);
                        let mut stream = stream;
                        let _ = stream.write_all(b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
                        continue;
                    }
                    let r = r.clone();
                    let conn_active = a.clone();
                    let spawned = std::thread::Builder::new().name("static-conn".into()).spawn(move || {
                        let _ = handle(stream, &r, port);
                        conn_active.fetch_sub(1, Ordering::SeqCst);
                    });
                    if spawned.is_err() {
                        a.fetch_sub(1, Ordering::SeqCst);
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => std::thread::sleep(Duration::from_millis(15)),
                Err(_) => std::thread::sleep(Duration::from_millis(50)),
            }
        }
    })?;
    Ok(StaticServer { port, root, stop })
}

fn content_type(p: &Path) -> &'static str {
    match p.extension().and_then(|e| e.to_str()).map(str::to_ascii_lowercase).as_deref() {
        Some("html" | "htm") => "text/html; charset=utf-8",
        Some("js" | "mjs") => "text/javascript; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("json" | "map") => "application/json",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        Some("avif") => "image/avif",
        Some("ico") => "image/x-icon",
        Some("woff") => "font/woff",
        Some("woff2") => "font/woff2",
        Some("ttf") => "font/ttf",
        Some("otf") => "font/otf",
        Some("wasm") => "application/wasm",
        Some("txt") => "text/plain; charset=utf-8",
        Some("mp4") => "video/mp4",
        Some("webm") => "video/webm",
        _ => "application/octet-stream",
    }
}

fn percent_decode(s: &str) -> Option<String> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b.get(i) {
            Some(b'%') => {
                let hex = s.get(i + 1..i + 3)?;
                out.push(u8::from_str_radix(hex, 16).ok()?);
                i += 3;
            }
            Some(c) => {
                out.push(*c);
                i += 1;
            }
            None => break,
        }
    }
    String::from_utf8(out).ok()
}

/// Map a request path to a file under `root`, or `None` when it is outside or missing.
pub fn resolve(root: &Path, url_path: &str) -> Option<PathBuf> {
    let path = url_path.split(['?', '#']).next().unwrap_or("/");
    let decoded = percent_decode(path)?;
    let mut rel = PathBuf::new();
    for c in Path::new(decoded.trim_start_matches('/')).components() {
        match c {
            Component::Normal(n) => rel.push(n),
            Component::CurDir => {}
            _ => return None,
        }
    }
    let mut candidate = root.join(&rel);
    if candidate.is_dir() {
        candidate = candidate.join("index.html");
    }
    // A static export may write a page as `about.html` (Next.js without trailing slashes).
    if !candidate.is_file() && rel.extension().is_none() {
        let page = root.join(&rel).with_extension("html");
        candidate = if !rel.as_os_str().is_empty() && page.is_file() { page } else { root.join("index.html") };
    }
    let real = std::fs::canonicalize(&candidate).ok()?;
    (real.starts_with(root) && real.is_file()).then_some(real)
}

fn refuse(stream: &mut TcpStream, status: &str, body: &str) -> std::io::Result<()> {
    let head = format!("HTTP/1.1 {status}\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
    stream.write_all(head.as_bytes())?;
    stream.write_all(body.as_bytes())
}

fn handle(mut stream: TcpStream, root: &Path, port: u16) -> std::io::Result<()> {
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    stream.set_write_timeout(Some(Duration::from_secs(20)))?;
    let mut reader = BufReader::new((&stream).take(MAX_HEAD_BYTES));
    let mut line = String::new();
    reader.read_line(&mut line)?;
    let mut host: Option<String> = None;
    let mut origin: Option<String> = None;
    let (mut hosts, mut origins) = (0, 0);
    let mut h = String::new();
    while reader.read_line(&mut h)? > 2 {
        let trimmed = h.trim_end_matches(['\r', '\n']);
        if let Some((name, value)) = trimmed.split_once(':') {
            let value = value.trim().to_string();
            match name.trim().to_ascii_lowercase().as_str() {
                "host" => {
                    hosts += 1;
                    host = Some(value);
                }
                "origin" => {
                    origins += 1;
                    origin = Some(value);
                }
                _ => {}
            }
        }
        h.clear();
    }
    if hosts > 1 || origins > 1 {
        return refuse(&mut stream, "400 Bad Request", "One Host header, at most one Origin.");
    }
    if !host.is_some_and(|h| host_is_loopback(h.as_str(), Some(port))) {
        return refuse(&mut stream, "403 Forbidden", "This preview only answers loopback Host headers.");
    }
    if origin.is_some_and(|o| !origin_is_local(o.as_str())) {
        return refuse(&mut stream, "403 Forbidden", "This preview only answers local Origin headers.");
    }
    let mut parts = line.split_whitespace();
    let (method, target) = (parts.next().unwrap_or_default(), parts.next().unwrap_or("/"));
    if method != "GET" && method != "HEAD" {
        return refuse(&mut stream, "405 Method Not Allowed", "Only GET and HEAD.");
    }
    let Some(path) = resolve(root, target) else {
        return refuse(&mut stream, "404 Not Found", "Not found");
    };
    let mut file = match std::fs::File::open(&path) {
        Ok(f) => f,
        Err(_) => return refuse(&mut stream, "404 Not Found", "Not found"),
    };
    let len = file.metadata().map(|m| m.len()).unwrap_or(0);
    let head =
        format!("HTTP/1.1 200 OK\r\nContent-Type: {}\r\nContent-Length: {len}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n", content_type(&path));
    stream.write_all(head.as_bytes())?;
    if method == "GET" {
        // Stream instead of reading the whole file into memory; a large asset can't exhaust us.
        let mut remaining = len;
        let mut buf = [0u8; 64 * 1024];
        while remaining > 0 {
            let want = remaining.min(buf.len() as u64) as usize;
            let n = file.read(&mut buf[..want])?;
            if n == 0 {
                break;
            }
            stream.write_all(&buf[..n])?;
            remaining = remaining.saturating_sub(n as u64);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(port: u16, path: &str, host: &str, origin: Option<&str>) -> String {
        let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
        let mut req = format!("GET {path} HTTP/1.1\r\nHost: {host}\r\n");
        if let Some(o) = origin {
            req.push_str(&format!("Origin: {o}\r\n"));
        }
        req.push_str("\r\n");
        s.write_all(req.as_bytes()).unwrap();
        let mut out = String::new();
        s.read_to_string(&mut out).unwrap();
        out
    }

    fn get(port: u16, path: &str) -> String {
        request(port, path, &format!("127.0.0.1:{port}"), None)
    }

    #[test]
    fn serves_files_spa_fallback_and_refuses_escapes() {
        let dir = tempfile::tempdir().unwrap();
        let site = dir.path().join("dist");
        std::fs::create_dir_all(site.join("assets")).unwrap();
        std::fs::write(site.join("index.html"), "<h1>home</h1>").unwrap();
        std::fs::write(site.join("assets/app.js"), "console.log(1)").unwrap();
        std::fs::write(dir.path().join("secret.txt"), "nope").unwrap();
        let srv = serve_dir(&site).unwrap();
        assert!(get(srv.port, "/").contains("<h1>home</h1>"));
        let js = get(srv.port, "/assets/app.js?v=1");
        assert!(js.contains("text/javascript") && js.contains("console.log(1)"));
        assert!(get(srv.port, "/pricing").contains("<h1>home</h1>"), "client-side routes fall back to index.html");
        std::fs::write(site.join("about.html"), "<h1>about</h1>").unwrap();
        assert!(get(srv.port, "/about").contains("<h1>about</h1>") && get(srv.port, "/about/").contains("<h1>about</h1>"), "an exported page");
        assert!(get(srv.port, "/missing.png").starts_with("HTTP/1.1 404"));
        for bad in ["/../secret.txt", "/%2e%2e/secret.txt", "/assets/../../secret.txt"] {
            assert!(!get(srv.port, bad).contains("nope"), "{bad}");
        }
        let url = srv.url();
        drop(srv);
        assert!(url.starts_with("http://127.0.0.1:"));
    }

    #[test]
    fn host_and_origin_must_be_loopback() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("index.html"), "ok").unwrap();
        let srv = serve_dir(dir.path()).unwrap();
        assert!(get(srv.port, "/").contains("ok"));
        assert!(request(srv.port, "/", "evil.example", None).starts_with("HTTP/1.1 403"));
        assert!(request(srv.port, "/", &format!("127.0.0.1:{}", srv.port), Some("https://evil.example")).starts_with("HTTP/1.1 403"));
        assert!(request(srv.port, "/", &format!("localhost:{}", srv.port), Some("http://localhost:5173")).contains("ok"));
        let mut s = TcpStream::connect(("127.0.0.1", srv.port)).unwrap();
        s.write_all(b"GET / HTTP/1.1\r\n\r\n").unwrap();
        let mut out = String::new();
        s.read_to_string(&mut out).unwrap();
        assert!(out.starts_with("HTTP/1.1 403"), "missing Host is refused");
    }
}
