//! OAuth browser-callback listener.
//!
//! Ported 1:1 from `zig-reference/src/core/auth/browser_callback.zig`. Zig
//! drives `std.Io.net.Server`/`Stream` with `poll`; the Rust port uses
//! `std::net::{TcpListener, TcpStream}` with non-blocking accept plus a
//! 1 ms read-timeout poll loop. `SO_LINGER` reset probes become plain closes
//! because `std` exposes no safe linger API — both are observed identically by
//! the listener (EOF or reset). The Zig `await` function is renamed
//! `await_callback` because `await` is a reserved Rust keyword.

use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crate::core::shared::{debug_trace, io as io_mod};

const POLL_MS: u64 = 100;
const SOCKET_TIMEOUT_SECONDS: u64 = 30;
const SILENCE_MS: u64 = 250;
const MAX_ACCEPTS_PER_POLL: usize = 16;
const MAX_REQUEST_BYTES: usize = 16 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Response {
    Ok,
    Failed,
    Unrelated,
}

#[derive(Debug)]
pub enum ParseResult<Callback, E> {
    Accepted(Callback),
    Unrelated,
    Failed(E),
}

#[derive(Debug)]
pub struct Accepted<Callback> {
    pub stream: TcpStream,
    pub callback: Callback,
    pub cors_origin: Option<Vec<u8>>,
}

impl<Callback> Accepted<Callback> {
    pub fn deinit(&mut self) {
        let _ = self.stream.shutdown(Shutdown::Both);
    }

    pub fn respond(&mut self, outcome: Response) -> std::io::Result<()> {
        write_response(&mut self.stream, outcome, self.cors_origin.as_deref())
    }
}

/// Drains the listener's queued connections and returns the first callback the
/// provider accepts, or null when no callback is ready during this poll.
///
/// Browsers may open an idle speculative connection or request unrelated paths
/// before sending the redirect. Those connections must not close the listener
/// or hold the real callback in the accept queue. Provider-specific parsing and
/// error classification stay in the adapter passed by the caller.
#[derive(Debug)]
pub enum AwaitError<E> {
    Cancelled,
    ListenerFailed,
    InvalidRequest,
    RequestTooLarge,
    Io(std::io::Error),
    Parse(E),
}

pub fn await_callback<Callback, E>(
    parse: &dyn Fn(&[u8]) -> ParseResult<Callback, E>,
    listener: &TcpListener,
    cancel_flag: &AtomicBool,
    allowed_cors_origin: Option<&[u8]>,
) -> Result<Option<Accepted<Callback>>, AwaitError<E>> {
    listener.set_nonblocking(true).map_err(AwaitError::Io)?;
    let mut accepts = 0;
    while accepts < MAX_ACCEPTS_PER_POLL {
        accepts += 1;
        let mut stream = match accept_with_timeout(listener, cancel_flag)? {
            Some(stream) => stream,
            None => return Ok(None),
        };
        set_socket_timeouts(&stream);

        let request = match read_request(&mut stream, cancel_flag, allowed_cors_origin) {
            Err(AwaitError::Cancelled) => return Err(AwaitError::Cancelled),
            Err(AwaitError::InvalidRequest) | Err(AwaitError::RequestTooLarge) => {
                let _ = write_response(&mut stream, Response::Unrelated, None);
                continue;
            }
            Err(err) => return Err(err),
            Ok(None) => continue,
            Ok(Some(request)) => request,
        };

        match request.kind {
            RequestKind::Preflight => {
                if let Some(origin) = allowed_cors_origin {
                    let _ = write_preflight_response(&mut stream, origin);
                }
                continue;
            }
            RequestKind::Unrelated => {
                let _ = write_response(&mut stream, Response::Unrelated, None);
                continue;
            }
            RequestKind::Callback => {}
        }

        match parse(&request.target) {
            ParseResult::Accepted(callback) => {
                return Ok(Some(Accepted {
                    stream,
                    callback,
                    cors_origin: request.cors_origin,
                }));
            }
            ParseResult::Unrelated => {
                let _ = write_response(
                    &mut stream,
                    Response::Unrelated,
                    request.cors_origin.as_deref(),
                );
                continue;
            }
            ParseResult::Failed(err) => {
                let _ = write_response(&mut stream, Response::Failed, request.cors_origin.as_deref());
                return Err(AwaitError::Parse(err));
            }
        }
    }
    Ok(None)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RequestKind {
    Callback,
    Preflight,
    Unrelated,
}

struct Request {
    kind: RequestKind,
    target: Vec<u8>,
    cors_origin: Option<Vec<u8>>,
}

fn accept_with_timeout<E>(
    listener: &TcpListener,
    cancel_flag: &AtomicBool,
) -> Result<Option<TcpStream>, AwaitError<E>> {
    if cancel_flag.load(Ordering::SeqCst) {
        return Err(AwaitError::Cancelled);
    }
    let deadline = Instant::now() + Duration::from_millis(POLL_MS);
    loop {
        match listener.accept() {
            Ok((stream, _)) => return Ok(Some(stream)),
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                if cancel_flag.load(Ordering::SeqCst) {
                    return Err(AwaitError::Cancelled);
                }
                if Instant::now() >= deadline {
                    return Ok(None);
                }
                std::thread::sleep(Duration::from_millis(1));
            }
            Err(err) if err.kind() == std::io::ErrorKind::ConnectionAborted => {
                if cancel_flag.load(Ordering::SeqCst) {
                    return Err(AwaitError::Cancelled);
                }
                if Instant::now() >= deadline {
                    return Ok(None);
                }
            }
            Err(err) => return Err(AwaitError::Io(err)),
        }
    }
}

/// Reads one bounded HTTP request, or returns null when the connection remains
/// silent for the speculative-preconnect budget.
fn read_request<E>(
    stream: &mut TcpStream,
    cancel_flag: &AtomicBool,
    allowed_cors_origin: Option<&[u8]>,
) -> Result<Option<Request>, AwaitError<E>> {
    stream
        .set_read_timeout(Some(Duration::from_millis(1)))
        .map_err(AwaitError::Io)?;
    let deadline = Instant::now() + Duration::from_millis(SILENCE_MS);
    let mut request_bytes: Vec<u8> = Vec::with_capacity(256);
    let mut found_terminator = false;
    while request_bytes.len() < MAX_REQUEST_BYTES {
        if cancel_flag.load(Ordering::SeqCst) {
            return Err(AwaitError::Cancelled);
        }
        let mut byte = [0u8; 1];
        match stream.read(&mut byte) {
            Ok(0) => {
                return if request_bytes.is_empty() {
                    Ok(None)
                } else {
                    Err(AwaitError::InvalidRequest)
                };
            }
            Ok(_) => {
                request_bytes.push(byte[0]);
                if request_bytes.ends_with(b"\r\n\r\n") {
                    found_terminator = true;
                    break;
                }
            }
            Err(err)
                if err.kind() == std::io::ErrorKind::WouldBlock
                    || err.kind() == std::io::ErrorKind::TimedOut =>
            {
                if cancel_flag.load(Ordering::SeqCst) {
                    return Err(AwaitError::Cancelled);
                }
                if Instant::now() >= deadline {
                    return Ok(None);
                }
            }
            Err(err)
                if err.kind() == std::io::ErrorKind::ConnectionReset
                    || err.kind() == std::io::ErrorKind::ConnectionAborted
                    || err.kind() == std::io::ErrorKind::BrokenPipe =>
            {
                return Ok(None);
            }
            Err(err) => return Err(AwaitError::Io(err)),
        }
    }
    if !found_terminator {
        return Err(AwaitError::RequestTooLarge);
    }

    let line_end = request_bytes
        .windows(2)
        .position(|window| window == b"\r\n")
        .ok_or(AwaitError::InvalidRequest)?;
    let request_line = &request_bytes[..line_end];
    let method_end = request_line
        .iter()
        .position(|&b| b == b' ')
        .ok_or(AwaitError::InvalidRequest)?;
    let target_start = method_end + 1;
    let target_end = request_line[target_start..]
        .iter()
        .position(|&b| b == b' ')
        .map(|offset| target_start + offset)
        .ok_or(AwaitError::InvalidRequest)?;
    let method = &request_line[..method_end];
    let target = &request_line[target_start..target_end];
    let headers = &request_bytes[line_end + 2..];
    let origin = request_header_value(headers, b"origin");

    let cors_origin = match allowed_cors_origin {
        Some(value) => value,
        None => {
            if method != b"GET" {
                return Err(AwaitError::InvalidRequest);
            }
            return Ok(Some(Request {
                kind: RequestKind::Callback,
                target: target.to_vec(),
                cors_origin: None,
            }));
        }
    };
    let origin_allowed = match origin {
        Some(value) => value == cors_origin,
        None => false,
    };

    if method == b"OPTIONS" {
        let requested_method = request_header_value(headers, b"access-control-request-method");
        let callback_path = target == b"/callback" || target.starts_with(b"/callback?");
        let valid = origin_allowed
            && requested_method.is_some()
            && requested_method.unwrap().eq_ignore_ascii_case(b"GET")
            && callback_path;
        return Ok(Some(Request {
            kind: if valid {
                RequestKind::Preflight
            } else {
                RequestKind::Unrelated
            },
            target: target.to_vec(),
            cors_origin: None,
        }));
    }
    if method != b"GET" {
        return Err(AwaitError::InvalidRequest);
    }
    Ok(Some(Request {
        kind: if origin.is_none() || origin_allowed {
            RequestKind::Callback
        } else {
            RequestKind::Unrelated
        },
        target: target.to_vec(),
        cors_origin: if origin_allowed {
            Some(cors_origin.to_vec())
        } else {
            None
        },
    }))
}

fn request_header_value<'a>(headers: &'a [u8], name: &[u8]) -> Option<&'a [u8]> {
    for line in headers.split(|&b| b == b'\n') {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if line.is_empty() {
            break;
        }
        let colon = match line.iter().position(|&b| b == b':') {
            Some(colon) => colon,
            None => continue,
        };
        if !line[..colon].eq_ignore_ascii_case(name) {
            continue;
        }
        return Some(trim_ascii_ws(&line[colon + 1..]));
    }
    None
}

fn trim_ascii_ws(mut slice: &[u8]) -> &[u8] {
    while slice.first().is_some_and(|&b| b == b' ' || b == b'\t') {
        slice = &slice[1..];
    }
    while slice.last().is_some_and(|&b| b == b' ' || b == b'\t') {
        slice = &slice[..slice.len() - 1];
    }
    slice
}

const CALLBACK_CSS: &str = ":root{color-scheme:light dark}\
body{margin:0;min-height:100vh;display:flex;align-items:center;justify-content:center;\
background:#fff;color:#111;\
font:15px/1.6 ui-sans-serif,-apple-system,BlinkMacSystemFont,\"Segoe UI\",sans-serif}\
@media(prefers-color-scheme:dark){body{background:#0b0b0c;color:#f4f4f5}}\
main{text-align:center;padding:2rem;max-width:26rem}\
h1{margin:0 0 .5rem;font-size:1.125rem;font-weight:600;letter-spacing:-.01em}\
p{margin:0;font-size:.875rem;opacity:.62}";

fn callback_page(title: &str, detail: &str) -> String {
    format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\
         <title>fx</title><style>{}</style></head><body><main><h1>{}</h1><p>{}</p></main></body></html>",
        CALLBACK_CSS, title, detail
    )
}

pub fn write_response(
    stream: &mut TcpStream,
    outcome: Response,
    cors_origin: Option<&[u8]>,
) -> std::io::Result<()> {
    let (status, body): (&str, String) = match outcome {
        Response::Ok => (
            "200 OK",
            callback_page(
                "Authorization complete",
                "Returning you to fx. You can close this tab.",
            ),
        ),
        Response::Failed => (
            "400 Bad Request",
            callback_page("Authorization failed", "Return to fx for details."),
        ),
        Response::Unrelated => (
            "404 Not Found",
            "<!doctype html><title>Not found</title>Not found.".to_string(),
        ),
    };
    let mut out = String::new();
    out.push_str(&format!("HTTP/1.1 {}\r\n", status));
    if let Some(origin) = cors_origin {
        out.push_str(&format!(
            "Access-Control-Allow-Origin: {}\r\nVary: Origin\r\n",
            String::from_utf8_lossy(origin)
        ));
    }
    out.push_str(&format!(
        "Content-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    ));
    stream.write_all(out.as_bytes())
}

fn write_preflight_response(stream: &mut TcpStream, origin: &[u8]) -> std::io::Result<()> {
    let mut out = String::new();
    out.push_str(&format!(
        "HTTP/1.1 204 No Content\r\nAccess-Control-Allow-Origin: {}\r\nAccess-Control-Allow-Methods: GET\r\n",
        String::from_utf8_lossy(origin)
    ));
    out.push_str(
        "Access-Control-Allow-Private-Network: true\r\nVary: Origin, Access-Control-Request-Method, Access-Control-Request-Private-Network\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
    );
    stream.write_all(out.as_bytes())
}

fn set_socket_timeouts(stream: &TcpStream) {
    let timeout = Duration::from_secs(SOCKET_TIMEOUT_SECONDS);
    if let Err(err) = stream.set_read_timeout(Some(timeout)) {
        debug_trace::logf(
            "auth",
            format_args!(
                "OAuth callback receive timeout setup failed errno={:?}",
                err.kind()
            ),
        );
    }
    if let Err(err) = stream.set_write_timeout(Some(timeout)) {
        debug_trace::logf(
            "auth",
            format_args!(
                "OAuth callback send timeout setup failed errno={:?}",
                err.kind()
            ),
        );
    }
}

fn bind_test_listener() -> std::io::Result<TcpListener> {
    TcpListener::bind("127.0.0.1:0")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug)]
    struct TestCallback {
        code: &'static [u8],
    }

    fn parse_test_callback(target: &[u8]) -> ParseResult<TestCallback, ()> {
        if target == b"/callback?code=granted" {
            ParseResult::Accepted(TestCallback { code: b"granted" })
        } else {
            ParseResult::Unrelated
        }
    }

    struct CallbackProbe {
        port: u16,
        requests: Vec<Vec<u8>>,
    }

    impl CallbackProbe {
        fn run(&self) {
            for request in &self.requests {
                let address = format!("127.0.0.1:{}", self.port);
                let mut stream = match TcpStream::connect(&address) {
                    Ok(stream) => stream,
                    Err(_) => return,
                };
                if request.is_empty() {
                    continue;
                }
                if stream.write_all(request).is_err() || stream.flush().is_err() {
                    return;
                }
                let mut buffer = [0u8; 512];
                loop {
                    match stream.read(&mut buffer) {
                        Ok(0) | Err(_) => break,
                        Ok(_) => {}
                    }
                }
            }
        }
    }

    struct ResetPreconnectProbe {
        port: u16,
        request: Vec<u8>,
        hold_ms: u64,
        connected: AtomicBool,
        failed: AtomicBool,
    }

    impl ResetPreconnectProbe {
        fn run(&self) {
            let address = format!("127.0.0.1:{}", self.port);
            {
                let reset_stream = match TcpStream::connect(&address) {
                    Ok(stream) => stream,
                    Err(_) => return self.finish(true),
                };
                self.finish(false);
                io_mod::sleep(self.hold_ms * 1_000_000);
                drop(reset_stream);
            }
            let mut stream = match TcpStream::connect(&address) {
                Ok(stream) => stream,
                Err(_) => return,
            };
            let _ = stream.write_all(&self.request);
            let _ = stream.flush();
        }

        fn finish(&self, failed: bool) {
            self.failed.store(failed, Ordering::Release);
            self.connected.store(true, Ordering::Release);
        }

        fn wait_until_connected(&self) {
            while !self.connected.load(Ordering::Acquire) {
                io_mod::sleep(1_000_000);
            }
            assert!(!self.failed.load(Ordering::Acquire));
        }
    }

    struct HeldPreconnectProbe {
        port: u16,
        request: Vec<u8>,
        delivered: AtomicBool,
        release: AtomicBool,
        failed: AtomicBool,
    }

    impl HeldPreconnectProbe {
        fn run(&self) {
            let address = format!("127.0.0.1:{}", self.port);
            let idle = match TcpStream::connect(&address) {
                Ok(stream) => stream,
                Err(_) => return self.finish(true),
            };
            if self.request.is_empty() {
                self.finish(false);
                self.wait();
                drop(idle);
                return;
            }
            let mut stream = match TcpStream::connect(&address) {
                Ok(stream) => stream,
                Err(_) => return self.finish(true),
            };
            if stream.write_all(&self.request).is_err() || stream.flush().is_err() {
                return self.finish(true);
            }
            self.finish(false);
            self.wait();
            drop(stream);
            drop(idle);
        }

        fn finish(&self, failed: bool) {
            self.failed.store(failed, Ordering::Release);
            self.delivered.store(true, Ordering::Release);
        }

        fn wait(&self) {
            while !self.release.load(Ordering::Acquire) {
                io_mod::sleep(1_000_000);
            }
        }

        fn wait_until_delivered(&self) {
            while !self.delivered.load(Ordering::Acquire) {
                io_mod::sleep(1_000_000);
            }
            assert!(!self.failed.load(Ordering::Acquire));
        }
    }

    struct CorsCallbackProbe {
        port: u16,
        preflight_response: Vec<u8>,
        callback_response: Vec<u8>,
        failed: bool,
    }

    impl CorsCallbackProbe {
        fn run(&mut self) {
            let mut preflight = Vec::new();
            let mut callback = Vec::new();
            let ok = self
                .exchange(
                    b"OPTIONS /callback HTTP/1.1\r\nHost: 127.0.0.1\r\nOrigin: https://accounts.x.ai\r\nAccess-Control-Request-Method: GET\r\nAccess-Control-Request-Private-Network: true\r\n\r\n",
                    &mut preflight,
                )
                .is_some()
                && self
                    .exchange(
                        b"GET /callback?code=granted HTTP/1.1\r\nHost: 127.0.0.1\r\nOrigin: https://accounts.x.ai\r\n\r\n",
                        &mut callback,
                    )
                    .is_some();
            if !ok {
                self.failed = true;
                return;
            }
            self.preflight_response = preflight;
            self.callback_response = callback;
        }

        fn exchange(&self, request: &[u8], out: &mut Vec<u8>) -> Option<usize> {
            let address = format!("127.0.0.1:{}", self.port);
            let mut stream = TcpStream::connect(&address).ok()?;
            stream.write_all(request).ok()?;
            stream.flush().ok()?;
            let mut buffer = [0u8; 1024];
            let mut total = 0usize;
            loop {
                match stream.read(&mut buffer) {
                    Ok(0) | Err(_) => break,
                    Ok(read) => {
                        out.extend_from_slice(&buffer[..read]);
                        total += read;
                    }
                }
            }
            Some(total)
        }
    }

    #[test]
    fn browser_callback_outruns_an_idle_preconnect_held_open() {
        let listener = bind_test_listener().unwrap();
        let port = listener.local_addr().unwrap().port();
        let probe = HeldPreconnectProbe {
            port,
            request: b"GET /callback?code=granted HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n".to_vec(),
            delivered: AtomicBool::new(false),
            release: AtomicBool::new(false),
            failed: AtomicBool::new(false),
        };
        let cancel_flag = AtomicBool::new(false);
        let started_ms = io_mod::milli_timestamp();
        let code = std::thread::scope(|scope| {
            scope.spawn(|| probe.run());
            probe.wait_until_delivered();

            let mut accepted =
                await_callback::<TestCallback, ()>(&parse_test_callback, &listener, &cancel_flag, None)
                    .unwrap()
                    .expect("callback never arrived");
            let code = accepted.callback.code;
            accepted.deinit();
            probe.release.store(true, Ordering::Release);
            code
        });
        assert_eq!(b"granted", code);
        assert!(io_mod::milli_timestamp() - started_ms < 1_000);
    }

    #[test]
    fn browser_callback_cancels_while_an_idle_preconnect_is_open() {
        let listener = bind_test_listener().unwrap();
        let port = listener.local_addr().unwrap().port();
        let probe = HeldPreconnectProbe {
            port,
            request: Vec::new(),
            delivered: AtomicBool::new(false),
            release: AtomicBool::new(false),
            failed: AtomicBool::new(false),
        };
        let cancel_flag = AtomicBool::new(false);
        let started_ms = io_mod::milli_timestamp();
        let result = std::thread::scope(|scope| {
            scope.spawn(|| probe.run());
            probe.wait_until_delivered();
            scope.spawn(|| {
                io_mod::sleep(20 * 1_000_000);
                cancel_flag.store(true, Ordering::SeqCst);
            });

            let result =
                await_callback::<TestCallback, ()>(&parse_test_callback, &listener, &cancel_flag, None);
            probe.release.store(true, Ordering::Release);
            result
        });
        assert!(matches!(result, Err(AwaitError::Cancelled)));
        assert!(io_mod::milli_timestamp() - started_ms < 1_000);
    }

    #[test]
    fn browser_callback_survives_unrelated_requests_before_the_redirect() {
        let listener = bind_test_listener().unwrap();
        let port = listener.local_addr().unwrap().port();
        let probe = CallbackProbe {
            port,
            requests: vec![
                Vec::new(),
                b"GET /favicon.ico HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n".to_vec(),
                b"GET /unrelated?code=nope HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n".to_vec(),
                b"GET /callback?code=granted HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n".to_vec(),
            ],
        };
        let cancel_flag = AtomicBool::new(false);
        let code = std::thread::scope(|scope| {
            scope.spawn(|| probe.run());

            let mut accepted =
                await_callback::<TestCallback, ()>(&parse_test_callback, &listener, &cancel_flag, None)
                    .unwrap()
                    .expect("callback never arrived");
            accepted.respond(Response::Ok).expect("respond");
            let code = accepted.callback.code;
            accepted.deinit();
            code
        });
        assert_eq!(b"granted", code);
    }

    fn expect_reset_preconnect_survives(hold_ms: u64) {
        let listener = bind_test_listener().unwrap();
        let port = listener.local_addr().unwrap().port();
        let probe = ResetPreconnectProbe {
            port,
            request: b"GET /callback?code=granted HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n".to_vec(),
            hold_ms,
            connected: AtomicBool::new(false),
            failed: AtomicBool::new(false),
        };
        let cancel_flag = AtomicBool::new(false);
        let code = std::thread::scope(|scope| {
            scope.spawn(|| probe.run());
            probe.wait_until_connected();

            let mut accepted =
                await_callback::<TestCallback, ()>(&parse_test_callback, &listener, &cancel_flag, None)
                    .unwrap()
                    .expect("callback never arrived");
            let code = accepted.callback.code;
            accepted.deinit();
            code
        });
        assert_eq!(b"granted", code);
    }

    #[test]
    fn browser_callback_survives_a_reset_preconnect_before_the_redirect() {
        expect_reset_preconnect_survives(100);
    }

    #[test]
    fn browser_callback_survives_a_reset_queued_before_accept() {
        expect_reset_preconnect_survives(0);
    }

    #[test]
    fn browser_callback_permits_the_xai_cors_private_network_preflight() {
        let listener = bind_test_listener().unwrap();
        let port = listener.local_addr().unwrap().port();
        let mut probe = CorsCallbackProbe {
            port,
            preflight_response: Vec::new(),
            callback_response: Vec::new(),
            failed: false,
        };
        let cancel_flag = AtomicBool::new(false);
        std::thread::scope(|scope| {
            scope.spawn(|| probe.run());

            let mut accepted = await_callback::<TestCallback, ()>(
                &parse_test_callback,
                &listener,
                &cancel_flag,
                Some(b"https://accounts.x.ai"),
            )
            .unwrap()
            .expect("callback never arrived");
            accepted.respond(Response::Ok).expect("respond");
            accepted.deinit();
        });

        assert!(!probe.failed);
        assert!(probe
            .preflight_response
            .starts_with(b"HTTP/1.1 204 No Content\r\n"));
        assert!(probe.preflight_response.windows(b"Access-Control-Allow-Origin: https://accounts.x.ai\r\n".len()).any(|w| w == b"Access-Control-Allow-Origin: https://accounts.x.ai\r\n"));
        assert!(probe
            .preflight_response
            .windows(b"Access-Control-Allow-Methods: GET\r\n".len())
            .any(|w| w == b"Access-Control-Allow-Methods: GET\r\n"));
        assert!(probe
            .preflight_response
            .windows(b"Access-Control-Allow-Private-Network: true\r\n".len())
            .any(|w| w == b"Access-Control-Allow-Private-Network: true\r\n"));

        assert!(probe.callback_response.starts_with(b"HTTP/1.1 200 OK\r\n"));
        assert!(probe
            .callback_response
            .windows(b"Access-Control-Allow-Origin: https://accounts.x.ai\r\n".len())
            .any(|w| w == b"Access-Control-Allow-Origin: https://accounts.x.ai\r\n"));
        assert!(probe
            .callback_response
            .windows(b"Content-Type: text/html; charset=utf-8\r\n".len())
            .any(|w| w == b"Content-Type: text/html; charset=utf-8\r\n"));
        assert!(probe
            .callback_response
            .windows(b"<h1>Authorization complete</h1>".len())
            .any(|w| w == b"<h1>Authorization complete</h1>"));
        assert!(probe
            .callback_response
            .windows(b"prefers-color-scheme:dark".len())
            .any(|w| w == b"prefers-color-scheme:dark"));
    }
}
