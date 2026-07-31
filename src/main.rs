//! acestream-audio: pulls an AceStream engine stream once and serves it to many
//! listeners as audio (ADTS/MP3) or as fragmented MP4 with the video track
//! stream-copied.
//!
//!     GET /audio?id=<40-hex content id>[&fmt=adts|mp3]
//!     GET /video?id=<40-hex content id>
//!     GET /status
//!
//! Copy-first: audio already decodable by browsers is stream-copied untouched,
//! and video is *always* copied. One engine pull per content id feeds every
//! format, so a second listener — in any format — costs the engine nothing.

mod engine;
mod format;
mod mp4;
mod probe;
mod registry;
mod status;

use std::io::{Read, Write};
use std::net::TcpListener;
use std::os::fd::AsRawFd;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::thread;
use std::time::Instant;

use tiny_http::{Header, Request, Response, Server, StatusCode};

use engine::log;
use format::OutputFormat;
use registry::{Config, Registry, Subscription};

fn main() {
    let engine_host = match std::env::var("ENGINE_HOST") {
        Ok(v) if !v.is_empty() => v,
        _ => {
            log::error("ENGINE_HOST must be set");
            std::process::exit(1);
        }
    };
    let addr = normalise_addr(&env_or("LISTEN_ADDR", ":8080"));
    let frag_duration_ms = match std::env::var("FRAG_DURATION_MS") {
        Ok(v) if !v.is_empty() => match v.parse::<u32>() {
            Ok(ms) if ms > 0 => Some(ms),
            _ => {
                log::error(&format!(
                    "FRAG_DURATION_MS must be a positive integer, got {v:?}"
                ));
                std::process::exit(1);
            }
        },
        _ => None,
    };

    let listener = match TcpListener::bind(&addr) {
        Ok(l) => l,
        Err(e) => {
            log::error(&format!("could not bind {addr}: {e}"));
            std::process::exit(1);
        }
    };
    set_nodelay(&listener);

    let server = match Server::from_listener(listener, None) {
        Ok(s) => s,
        Err(e) => {
            log::error(&format!("could not start the http server: {e}"));
            std::process::exit(1);
        }
    };

    let registry = Registry::new(Config {
        engine_host: engine_host.clone(),
        frag_duration_ms,
    });
    let started = Instant::now();

    log::info(&format!(
        "listening on {addr} (engine host: {engine_host}, fragments: {})",
        frag_duration_ms
            .map(|ms| format!("{ms}ms"))
            .unwrap_or_else(|| "keyframe".into())
    ));

    for request in server.incoming_requests() {
        let registry = registry.clone();
        // A thread per request, deliberately not a pool: every stream is
        // long-lived, so a pool would be fully occupied by a handful of
        // listeners and new requests would never be served.
        if let Err(e) = thread::Builder::new()
            .name("request".into())
            .spawn(move || handle(&registry, request, started))
        {
            log::error(&format!("could not spawn a request thread: {e}"));
        }
    }
}

fn env_or(key: &str, fallback: &str) -> String {
    match std::env::var(key) {
        Ok(v) if !v.is_empty() => v,
        _ => fallback.to_owned(),
    }
}

/// Accept Go's `:8080` shorthand alongside a full `host:port`.
fn normalise_addr(addr: &str) -> String {
    match addr.strip_prefix(':') {
        Some(port) => format!("0.0.0.0:{port}"),
        None => addr.to_owned(),
    }
}

/// tiny_http does not expose accepted sockets, but Linux copies TCP_NODELAY
/// from the listener on accept, so setting it once here covers every listener.
/// Without it Nagle can add up to 40ms per write, which is significant against
/// a sub-100ms audio target.
fn set_nodelay(listener: &TcpListener) {
    let on: libc::c_int = 1;
    let rc = unsafe {
        libc::setsockopt(
            listener.as_raw_fd(),
            libc::IPPROTO_TCP,
            libc::TCP_NODELAY,
            &on as *const _ as *const libc::c_void,
            std::mem::size_of_val(&on) as libc::socklen_t,
        )
    };
    if rc != 0 {
        log::warn("could not set TCP_NODELAY on the listener; expect added latency");
    }
}

// ---------------------------------------------------------------------------
// Routing
// ---------------------------------------------------------------------------

fn handle(registry: &Arc<Registry>, request: Request, started: Instant) {
    let url = request.url().to_owned();
    let path = url.split('?').next().unwrap_or("/").to_owned();
    let method = request.method().as_str().to_owned();

    if method == "OPTIONS" {
        // We advertise CORS, so answer the preflight even though a plain GET of
        // a media URL does not trigger one.
        let _ = request.respond(
            Response::empty(StatusCode(204))
                .with_header(header("Access-Control-Allow-Origin", "*"))
                .with_header(header("Access-Control-Allow-Methods", "GET, OPTIONS"))
                .with_header(header("Access-Control-Allow-Headers", "*")),
        );
        return;
    }
    if method != "GET" && method != "HEAD" {
        return reply(request, 405, "method not allowed");
    }

    match path.as_str() {
        "/audio" | "/video" => serve_stream(registry, request, &url, &path),
        "/healthz" => reply(request, 200, "ok"),
        "/status" => {
            let body = status::snapshot(registry, started).to_string();
            let response = Response::from_string(body)
                .with_header(header("Content-Type", "application/json"))
                .with_header(header("Cache-Control", "no-store"))
                .with_header(header("Access-Control-Allow-Origin", "*"));
            let _ = request.respond(response);
        }
        _ => reply(
            request,
            200,
            "usage:\n  GET /audio?id=<40-hex content id>[&fmt=adts|mp3]\n  \
             GET /video?id=<40-hex content id>\n  GET /status\n",
        ),
    }
}

fn header(k: &str, v: &str) -> Header {
    Header::from_bytes(k.as_bytes(), v.as_bytes()).expect("static header is valid")
}

fn reply(request: Request, code: u16, body: &str) {
    let response = Response::from_string(body)
        .with_status_code(StatusCode(code))
        .with_header(header("Access-Control-Allow-Origin", "*"));
    let _ = request.respond(response);
}

/// Extract a query parameter. Values here are a hex id and a fixed word list,
/// neither of which can be percent-encoded, so no decoding is needed.
fn query_param(url: &str, key: &str) -> Option<String> {
    let query = url.split_once('?')?.1;
    query.split('&').find_map(|pair| {
        let (k, v) = pair.split_once('=')?;
        (k == key).then(|| v.to_owned())
    })
}

fn valid_content_id(id: &str) -> bool {
    id.len() == 40 && id.chars().all(|c| c.is_ascii_hexdigit())
}

fn serve_stream(registry: &Arc<Registry>, request: Request, url: &str, path: &str) {
    let peer = request
        .remote_addr()
        .map(|a| a.to_string())
        .unwrap_or_else(|| "unknown".into());

    let id = query_param(url, "id").unwrap_or_default();
    if !valid_content_id(&id) {
        return reply(request, 400, "id must be a 40-char hex content id");
    }

    let fmt = if path == "/video" {
        OutputFormat::Fmp4
    } else {
        let raw = query_param(url, "fmt").unwrap_or_else(|| "adts".into());
        match OutputFormat::parse_audio(&raw) {
            Some(f) => f,
            None => return reply(request, 400, "fmt must be adts or mp3"),
        }
    };

    let sub = match registry.subscribe(&id, fmt, &peer) {
        Ok(s) => s,
        Err(e) => {
            log::error(&format!("{peer}: {id}/{fmt}: {e}"));
            return reply(request, 502, &format!("could not start stream: {e}"));
        }
    };

    if request.method().as_str() == "HEAD" {
        let _ = request.respond(
            Response::empty(StatusCode(200))
                .with_header(header("Content-Type", fmt.content_type()))
                .with_header(header("Access-Control-Allow-Origin", "*")),
        );
        return;
    }

    log::info(&format!("{peer}: streaming {id} as {fmt}"));
    let counter = sub.counter();
    let start = Instant::now();

    stream_body(request, fmt, sub);

    log::info(&format!(
        "{peer}: {id}/{fmt} ended after {}s, {} bytes sent",
        start.elapsed().as_secs(),
        counter.load(Ordering::Relaxed)
    ));
}

/// Write the response head and body straight to the socket.
///
/// Deliberately bypasses `Request::respond`: tiny_http chooses Identity
/// transfer encoding for HTTP/1.0 clients and for anything sending
/// `TE: identity`, and with no known content length it then buffers the entire
/// body through `read_to_end` — which on an endless stream never returns and
/// grows without bound. Close-delimited framing (RFC 7230 §3.3.3) is valid for
/// both HTTP/1.0 and HTTP/1.1 responses and costs only keep-alive, which is
/// worthless when a connection carries exactly one hours-long stream.
///
/// It also puts flushing under our control, which is what makes the low-latency
/// ffmpeg flags meaningful end to end.
fn stream_body(request: Request, fmt: OutputFormat, mut sub: Subscription) {
    let head = format!(
        "HTTP/1.1 200 OK\r\n\
         Content-Type: {}\r\n\
         Cache-Control: no-store\r\n\
         Access-Control-Allow-Origin: *\r\n\
         Connection: close\r\n\
         \r\n",
        fmt.content_type()
    );

    let mut w = request.into_writer();
    if w.write_all(head.as_bytes()).is_err() || w.flush().is_err() {
        return;
    }

    let mut buf = vec![0u8; 64 * 1024];
    loop {
        match sub.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                // A write error is how a departed client is noticed; the
                // Subscription drop that follows tears down anything it was the
                // last listener for.
                if w.write_all(&buf[..n]).is_err() {
                    break;
                }
                // Flush per chunk: buffering here would undo -flush_packets.
                if w.flush().is_err() {
                    break;
                }
            }
            Err(_) => break,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_only_40_char_hex_ids() {
        assert!(valid_content_id(&"a".repeat(40)));
        assert!(valid_content_id("0123456789ABCDEFabcdef0123456789abcdef01"));
        assert!(!valid_content_id(&"a".repeat(39)));
        assert!(!valid_content_id(&"a".repeat(41)));
        assert!(!valid_content_id(""));
        assert!(!valid_content_id(&"g".repeat(40)));
        // Server-side url construction plus this check is what keeps a caller
        // from steering the engine request somewhere else.
        assert!(!valid_content_id("../../etc/passwd"));
        assert!(!valid_content_id("http://internal/admin"));
    }

    #[test]
    fn reads_query_parameters() {
        let url = "/audio?id=abc&fmt=mp3";
        assert_eq!(query_param(url, "id").as_deref(), Some("abc"));
        assert_eq!(query_param(url, "fmt").as_deref(), Some("mp3"));
        assert_eq!(query_param(url, "missing"), None);
        assert_eq!(query_param("/audio", "id"), None);
        assert_eq!(query_param("/audio?", "id"), None);
    }

    #[test]
    fn query_parameters_do_not_match_by_prefix() {
        let url = "/audio?xid=nope&id=yes";
        assert_eq!(query_param(url, "id").as_deref(), Some("yes"));
    }

    #[test]
    fn normalises_the_go_style_listen_address() {
        assert_eq!(normalise_addr(":8080"), "0.0.0.0:8080");
        assert_eq!(normalise_addr("127.0.0.1:9000"), "127.0.0.1:9000");
        assert_eq!(normalise_addr("0.0.0.0:8080"), "0.0.0.0:8080");
    }
}
