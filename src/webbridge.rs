//! `seance web` — the browser bridge: a tiny HTTP + websocket server that
//! proxies the daemon's JSON-lines control socket to the web client.
//!
//! # Shape
//!
//! - **One process, no async runtime.** Blocking `TcpListener`, a named thread
//!   per connection. The bridge is a two-socket pump; a runtime would buy
//!   nothing and drag gpui's executor into a headless binary.
//! - **Token auth on every attach.** The token is daemon-state-dir material
//!   (`web-token`, 0600), minted on first need; format + constant-time compare
//!   are [`seance_core::auth`]'s job so every bridge verifies identically.
//! - **Rejection is a websocket close, not an HTTP 401.** Browsers surface a
//!   failed upgrade as an indistinguishable `onerror`/`onclose(1006)`, so a
//!   pre-upgrade 401 is invisible to the client. We therefore accept the
//!   upgrade unconditionally and, on a bad token, send `Close(4401, "bad
//!   token")` — the one signal that arrives intact cross-browser.
//! - **Headers are peeked, not consumed.** Routing needs the request line
//!   before we know whether tungstenite should own the stream, and tungstenite
//!   re-parses the handshake from byte zero. `TcpStream::peek` leaves the bytes
//!   in the kernel buffer for it. Constraint: a request whose headers exceed
//!   the socket receive buffer can't be peeked whole — capped at
//!   [`MAX_HEADER_BYTES`], which is far above any real client request. A body
//!   (only `POST /replay/publish` has one) is read *after* the peeked headers
//!   are consumed, `Content-Length`-delimited and capped.
//! - **Replay endpoints are query-token'd.** `/replay/*` verifies the same
//!   token as `/ws` via [`seance_core::auth`] — but as a plain HTTP 401, which
//!   `fetch()` (unlike a websocket upgrade) surfaces fine. They route before
//!   the static handler and never appear in a log line with their token.
//! - **The ws is owned by exactly one thread.** `WebSocket<TcpStream>` is
//!   `Send` but not `Sync` and cannot be split, so the connection thread owns
//!   it and polls `read()` against a short TCP read timeout (tungstenite keeps
//!   partial-frame state across `WouldBlock`, the same contract non-blocking
//!   callers rely on); the unix→ws direction arrives over an mpsc channel fed
//!   by a second thread that owns the `UnixStream` read half.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context as _, Result};
use rand::RngCore as _;
use tungstenite::protocol::frame::coding::CloseCode;
use tungstenite::protocol::frame::CloseFrame;
use tungstenite::Message;

use crate::control::socket_path;
use crate::replayexport;

/// Default bind — loopback only. Remote access is tailscale's job, not ours.
const DEFAULT_BIND: &str = "127.0.0.1:9666";
/// Token file name inside the daemon state dir.
const TOKEN_FILE: &str = "web-token";
/// Upper bound on a peeked request header block.
const MAX_HEADER_BYTES: usize = 16 * 1024;
/// How long to wait for a complete header block to land.
const HEADER_TIMEOUT: Duration = Duration::from_secs(5);
/// ws read poll interval — bounds outbound latency when the client is silent
/// (daemon→client frames queue until the ws-owning thread wakes from read()).
/// 2ms keeps echo frames under a rAF frame; the wakeup cost (~500/s/conn,
/// two syscalls each) is noise. Measured: 50ms here put echo p50 at ~51ms.
const WS_POLL: Duration = Duration::from_millis(2);
/// Close code for "you presented no/!valid token". Client contract.
const CLOSE_BAD_TOKEN: u16 = 4401;

// ---------------------------------------------------------------------------
// entry point
// ---------------------------------------------------------------------------

/// Run the bridge. `args` is everything after `seance web`.
pub fn run(args: &[String]) -> Result<()> {
    let opts = Options::parse(args)?;

    if opts.regen_token {
        let tok = mint_token()?;
        eprintln!(
            "seance web: regenerated token at {}",
            token_path()?.display()
        );
        if opts.print_token {
            print_token_and_url(&tok, &opts.bind);
            return Ok(());
        }
        drop(tok);
    }

    let token = load_or_mint_token()?;

    if opts.print_token {
        print_token_and_url(&token, &opts.bind);
        return Ok(());
    }

    let dist = resolve_dist(opts.dist.clone())?;
    let listener =
        TcpListener::bind(&opts.bind).with_context(|| format!("binding {}", opts.bind))?;
    let local = listener
        .local_addr()
        .map(|a| a.to_string())
        .unwrap_or_else(|_| opts.bind.clone());

    let sock = socket_path();
    let daemon_up = UnixStream::connect(&sock).is_ok();
    eprintln!("seance web: listening on http://{local}");
    eprintln!("seance web: dist {}", dist.display());
    if daemon_up {
        eprintln!("seance web: daemon socket {} reachable", sock.display());
    } else {
        eprintln!(
            "seance web: WARNING daemon socket {} not reachable — serving anyway \
             (attaches fail until the daemon is up)",
            sock.display()
        );
    }
    eprintln!(
        "seance web: token in {} (`seance web --print-token` to read it)",
        token_path()?.display()
    );

    let mut n: u64 = 0;
    for stream in listener.incoming() {
        let stream = match stream {
            Ok(s) => s,
            Err(e) => {
                eprintln!("seance web: accept failed: {e}");
                continue;
            }
        };
        n += 1;
        let token = token.clone();
        let dist = dist.clone();
        let spawn = thread::Builder::new()
            .name(format!("seance-web-{n}"))
            .spawn(move || {
                let peer = stream
                    .peer_addr()
                    .map(|a| a.to_string())
                    .unwrap_or_else(|_| "?".into());
                if let Err(e) = serve_conn(stream, &peer, &token, &dist) {
                    eprintln!("seance web: [{peer}] connection ended: {e}");
                }
            });
        if let Err(e) = spawn {
            eprintln!("seance web: failed to spawn connection thread: {e}");
        }
    }
    Ok(())
}

fn print_token_and_url(token: &str, bind: &str) {
    let host = url_host(bind);
    println!("{token}");
    println!("http://{host}/?token={token}");
}

/// A bind of `0.0.0.0`/`[::]` is not something you can open — rewrite the
/// printed URL to loopback so the link is clickable.
fn url_host(bind: &str) -> String {
    match bind.rsplit_once(':') {
        Some((host, port)) => {
            let h = host.trim_matches(|c| c == '[' || c == ']');
            if h.is_empty() || h == "0.0.0.0" || h == "::" || h == "*" {
                format!("127.0.0.1:{port}")
            } else {
                bind.to_string()
            }
        }
        None => bind.to_string(),
    }
}

// ---------------------------------------------------------------------------
// options
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct Options {
    bind: String,
    dist: Option<PathBuf>,
    print_token: bool,
    regen_token: bool,
}

impl Options {
    fn parse(args: &[String]) -> Result<Self> {
        let mut o = Options {
            bind: DEFAULT_BIND.to_string(),
            dist: None,
            print_token: false,
            regen_token: false,
        };
        let mut i = 0;
        while i < args.len() {
            match args[i].as_str() {
                "--bind" => {
                    i += 1;
                    let v = args
                        .get(i)
                        .ok_or_else(|| anyhow!("--bind needs <addr:port>"))?;
                    o.bind = v.clone();
                }
                "--dist" => {
                    i += 1;
                    let v = args.get(i).ok_or_else(|| anyhow!("--dist needs <dir>"))?;
                    o.dist = Some(PathBuf::from(v));
                }
                "--print-token" => o.print_token = true,
                "--regen-token" => o.regen_token = true,
                "-h" | "--help" => {
                    println!(
                        "seance web [--bind <addr:port>] [--dist <dir>] \
                         [--print-token] [--regen-token]"
                    );
                    std::process::exit(0);
                }
                other => return Err(anyhow!("unknown flag for `seance web`: {other}")),
            }
            i += 1;
        }
        Ok(o)
    }
}

/// Static root: explicit flag → `$SEANCE_WEB_DIST` → `./crates/seance-web/dist`
/// (running from a source checkout) → `<exe>/../share/seance/web` (installed).
pub(crate) fn resolve_dist(explicit: Option<PathBuf>) -> Result<PathBuf> {
    if let Some(p) = explicit {
        return Ok(p);
    }
    if let Some(env) = std::env::var_os("SEANCE_WEB_DIST") {
        if !env.is_empty() {
            return Ok(PathBuf::from(env));
        }
    }
    let cwd_dist = PathBuf::from("crates/seance-web/dist");
    if cwd_dist.is_dir() {
        return Ok(cwd_dist);
    }
    let exe = std::env::current_exe().context("resolving current exe for dist fallback")?;
    let dir = exe.parent().unwrap_or_else(|| Path::new("."));
    Ok(dir.join("../share/seance/web"))
}

// ---------------------------------------------------------------------------
// token
// ---------------------------------------------------------------------------

fn token_path() -> Result<PathBuf> {
    Ok(crate::runtime::state_data_dir().join(TOKEN_FILE))
}

/// Read the existing token without minting one.
///
/// Minting here would be a lie: a fresh token wouldn't match the token a
/// *running* bridge already loaded, and callers (the replay editor URL) need
/// the one that works right now.
pub(crate) fn read_token() -> Result<String> {
    let path = token_path()?;
    let raw = std::fs::read_to_string(&path).with_context(|| {
        format!(
            "reading {} (start `seance web` once to mint it)",
            path.display()
        )
    })?;
    let t = raw.trim().to_string();
    if !seance_core::auth::token_well_formed(&t) {
        return Err(anyhow!(
            "{} is malformed — run `seance web --regen-token`",
            path.display()
        ));
    }
    Ok(t)
}

fn load_or_mint_token() -> Result<String> {
    let path = token_path()?;
    if let Ok(raw) = std::fs::read_to_string(&path) {
        let t = raw.trim().to_string();
        if seance_core::auth::token_well_formed(&t) {
            return Ok(t);
        }
        eprintln!("seance web: {} is malformed — reminting", path.display());
    }
    mint_token()
}

/// 32 bytes of OS entropy, hex — 64 chars, matching `auth::TOKEN_LEN`.
fn mint_token() -> Result<String> {
    let path = token_path()?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("creating state dir {}", dir.display()))?;
    }
    let mut bytes = [0u8; seance_core::auth::TOKEN_LEN / 2];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    let token = hex(&bytes);
    debug_assert!(seance_core::auth::token_well_formed(&token));

    // Write via a fresh 0600 file then rename: never leave a world-readable
    // window, never leave a half-written token behind.
    let tmp = path.with_extension("tmp");
    {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)
            .with_context(|| format!("creating {}", tmp.display()))?;
        f.write_all(token.as_bytes())
            .and_then(|_| f.write_all(b"\n"))
            .and_then(|_| f.flush())
            .with_context(|| format!("writing {}", tmp.display()))?;
    }
    std::fs::rename(&tmp, &path)
        .with_context(|| format!("renaming {} -> {}", tmp.display(), path.display()))?;
    Ok(token)
}

fn hex(bytes: &[u8]) -> String {
    const D: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(D[(b >> 4) as usize] as char);
        s.push(D[(b & 0x0f) as usize] as char);
    }
    s
}

// ---------------------------------------------------------------------------
// request parsing (pure — tested below)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
struct RequestLine {
    method: String,
    target: String,
}

impl RequestLine {
    /// Path with the query string stripped.
    fn path(&self) -> &str {
        match self.target.split_once('?') {
            Some((p, _)) => p,
            None => &self.target,
        }
    }

    /// Value of a query parameter, percent-decoded.
    fn query(&self, key: &str) -> Option<String> {
        let (_, q) = self.target.split_once('?')?;
        for pair in q.split('&') {
            let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
            if k == key {
                return Some(percent_decode(v));
            }
        }
        None
    }
}

fn parse_request_line(line: &str) -> Option<RequestLine> {
    let line = line.trim_end_matches(['\r', '\n']);
    let mut parts = line.split(' ');
    let method = parts.next()?;
    let target = parts.next()?;
    let version = parts.next()?;
    if method.is_empty() || !target.starts_with('/') || !version.starts_with("HTTP/") {
        return None;
    }
    Some(RequestLine {
        method: method.to_string(),
        target: target.to_string(),
    })
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'%' if i + 2 < b.len() => {
                let hi = (b[i + 1] as char).to_digit(16);
                let lo = (b[i + 2] as char).to_digit(16);
                match (hi, lo) {
                    (Some(h), Some(l)) => {
                        out.push((h * 16 + l) as u8);
                        i += 3;
                    }
                    _ => {
                        out.push(b[i]);
                        i += 1;
                    }
                }
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Case-insensitive header lookup over the raw header block (line 2 onward).
fn header<'a>(head: &'a str, name: &str) -> Option<&'a str> {
    for line in head.split("\r\n").skip(1) {
        if let Some((k, v)) = line.split_once(':') {
            if k.trim().eq_ignore_ascii_case(name) {
                return Some(v.trim());
            }
        }
    }
    None
}

fn is_websocket_upgrade(head: &str) -> bool {
    let up = header(head, "Upgrade")
        .map(|v| v.eq_ignore_ascii_case("websocket"))
        .unwrap_or(false);
    let conn = header(head, "Connection")
        .map(|v| {
            v.split(',')
                .any(|p| p.trim().eq_ignore_ascii_case("upgrade"))
        })
        .unwrap_or(false);
    up && conn
}

// ---------------------------------------------------------------------------
// static file serving (pure helpers — tested below)
// ---------------------------------------------------------------------------

/// Map a URL path to a relative filesystem path, or `None` if it escapes.
///
/// Rejects anything not rooted at `/`, any `..` component, and absolute or
/// backslash trickery; maps `/` to `index.html`.
fn sanitize_path(url_path: &str) -> Option<PathBuf> {
    if !url_path.starts_with('/') || url_path.contains('\\') || url_path.contains('\0') {
        return None;
    }
    let trimmed = url_path.trim_start_matches('/');
    let trimmed = if trimmed.is_empty() {
        "index.html"
    } else {
        trimmed
    };
    let mut out = PathBuf::new();
    for seg in trimmed.split('/') {
        if seg.is_empty() || seg == "." {
            continue;
        }
        if seg == ".." {
            return None;
        }
        out.push(seg);
    }
    if out.as_os_str().is_empty() {
        return None;
    }
    Some(out)
}

fn content_type(path: &Path) -> &'static str {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    match ext.as_str() {
        "html" | "htm" => "text/html; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "wasm" => "application/wasm",
        "json" => "application/json",
        "map" => "application/json",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "ico" => "image/x-icon",
        "txt" => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

/// `.html`/`.js` are the dev-iteration surfaces — never let a browser pin them.
fn no_cache(path: &Path) -> bool {
    matches!(
        path.extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_ascii_lowercase()
            .as_str(),
        "html" | "htm" | "js" | "mjs" | "wasm" | "css"
    )
}

// ---------------------------------------------------------------------------
// connection handling
// ---------------------------------------------------------------------------

fn serve_conn(stream: TcpStream, peer: &str, token: &str, dist: &Path) -> Result<()> {
    let _ = stream.set_nodelay(true);

    let (head, head_len) =
        peek_headers(&stream).with_context(|| format!("[{peer}] reading request headers"))?;
    let req = parse_request_line(head.lines().next().unwrap_or(""))
        .ok_or_else(|| anyhow!("[{peer}] malformed request line"))?;

    let ws_route = req.path() == "/ws" && is_websocket_upgrade(&head);
    if !ws_route {
        // Non-upgrade paths: consume the header bytes we peeked, then reply.
        consume(&stream, head_len)?;
    }

    match (req.method.as_str(), req.path()) {
        (_, "/ws") if ws_route => serve_ws(stream, peer, &req, token),
        ("GET", "/healthz") => {
            let mut s = stream;
            write_response(&mut s, 200, "OK", "text/plain; charset=utf-8", false, b"ok")
        }
        // Replay routes come before the static handler; each is token-gated.
        ("GET", "/replay/list") | ("GET", "/replay/manifest") | ("GET", "/replay/pane") => {
            let mut s = stream;
            serve_replay_get(&mut s, peer, &req, token)
        }
        ("POST", "/replay/publish") => {
            // The body must be drained whether or not auth passes, or the
            // client sees a reset instead of our response.
            let body = read_body(&stream, &head, replayexport::MAX_PUBLISH_BODY);
            let mut s = stream;
            serve_replay_publish(&mut s, peer, &req, token, body)
        }
        ("GET", _) => {
            let mut s = stream;
            serve_static(&mut s, peer, req.path(), dist)
        }
        _ => {
            let mut s = stream;
            write_response(
                &mut s,
                405,
                "Method Not Allowed",
                "text/plain; charset=utf-8",
                false,
                b"method not allowed",
            )
        }
    }
}

/// Peek the header block without consuming it (tungstenite re-reads the
/// handshake from byte zero). Returns the header text and its byte length.
fn peek_headers(stream: &TcpStream) -> Result<(String, usize)> {
    let deadline = Instant::now() + HEADER_TIMEOUT;
    let mut buf = vec![0u8; MAX_HEADER_BYTES];
    loop {
        let n = stream.peek(&mut buf).context("peeking request bytes")?;
        if n == 0 {
            return Err(anyhow!("client closed before sending a request"));
        }
        if let Some(pos) = find_header_end(&buf[..n]) {
            let head = String::from_utf8_lossy(&buf[..pos.saturating_sub(4)]).into_owned();
            return Ok((head, pos));
        }
        if n >= MAX_HEADER_BYTES {
            return Err(anyhow!("request headers exceed {MAX_HEADER_BYTES} bytes"));
        }
        if Instant::now() >= deadline {
            return Err(anyhow!("timed out waiting for complete request headers"));
        }
        thread::sleep(Duration::from_millis(5));
    }
}

fn find_header_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n").map(|p| p + 4)
}

fn consume(stream: &TcpStream, n: usize) -> Result<()> {
    let mut sink = vec![0u8; n];
    let mut s = stream;
    s.read_exact(&mut sink).context("consuming request headers")
}

fn write_response(
    stream: &mut TcpStream,
    code: u16,
    reason: &str,
    ctype: &str,
    no_store: bool,
    body: &[u8],
) -> Result<()> {
    let cache = if no_store {
        "Cache-Control: no-cache\r\n"
    } else {
        ""
    };
    let head = format!(
        "HTTP/1.1 {code} {reason}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\n\
         Connection: close\r\n{cache}\r\n",
        body.len()
    );
    stream
        .write_all(head.as_bytes())
        .context("writing response head")?;
    stream.write_all(body).context("writing response body")?;
    stream.flush().context("flushing response")
}

fn serve_static(stream: &mut TcpStream, peer: &str, url_path: &str, dist: &Path) -> Result<()> {
    let rel = match sanitize_path(url_path) {
        Some(p) => p,
        None => {
            eprintln!("seance web: [{peer}] rejected path {url_path}");
            return write_response(
                stream,
                400,
                "Bad Request",
                "text/plain; charset=utf-8",
                false,
                b"bad path",
            );
        }
    };
    let full = dist.join(&rel);
    match std::fs::read(&full) {
        Ok(body) => write_response(
            stream,
            200,
            "OK",
            content_type(&full),
            no_cache(&full),
            &body,
        ),
        Err(_) => write_response(
            stream,
            404,
            "Not Found",
            "text/plain; charset=utf-8",
            false,
            b"not found",
        ),
    }
}

// ---------------------------------------------------------------------------
// replay endpoints
// ---------------------------------------------------------------------------

/// Read a `Content-Length`-delimited body after the headers were consumed.
///
/// Over-long bodies are refused rather than truncated: a half-parsed publish
/// request is worse than a clear 413. Returns an empty body when the header is
/// absent (a bodyless POST is a client bug we report as a parse failure).
fn read_body(stream: &TcpStream, head: &str, cap: usize) -> Result<Vec<u8>> {
    let len: usize = match header(head, "Content-Length") {
        Some(v) => v.trim().parse().unwrap_or(0),
        None => 0,
    };
    if len > cap {
        return Err(anyhow!(
            "request body of {len} bytes exceeds the {cap}-byte cap"
        ));
    }
    let mut buf = vec![0u8; len];
    if len > 0 {
        let mut s = stream;
        s.read_exact(&mut buf).context("reading request body")?;
    }
    Ok(buf)
}

/// Same token as `/ws` — but over plain HTTP, where a 401 is visible to
/// `fetch()` (the websocket close-code dance exists only for upgrades).
fn replay_auth_ok(req: &RequestLine, token: &str) -> bool {
    let presented = req.query("token").unwrap_or_default();
    seance_core::auth::token_well_formed(&presented)
        && seance_core::auth::token_matches(token, &presented)
}

fn json_ok(stream: &mut TcpStream, body: &[u8]) -> Result<()> {
    write_response(stream, 200, "OK", "application/json", true, body)
}

fn json_err(stream: &mut TcpStream, code: u16, reason: &str, msg: &str) -> Result<()> {
    let body = serde_json::json!({ "error": msg }).to_string();
    write_response(
        stream,
        code,
        reason,
        "application/json",
        true,
        body.as_bytes(),
    )
}

/// Required unsigned query param.
fn q_u64(req: &RequestLine, key: &str) -> Result<u64> {
    req.query(key)
        .ok_or_else(|| anyhow!("missing {key}"))?
        .trim()
        .parse::<u64>()
        .map_err(|_| anyhow!("{key} must be unix milliseconds"))
}

fn q_panes(req: &RequestLine) -> Option<Vec<String>> {
    let raw = req.query("panes")?;
    let v: Vec<String> = raw
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    (!v.is_empty()).then_some(v)
}

fn serve_replay_get(
    stream: &mut TcpStream,
    peer: &str,
    req: &RequestLine,
    token: &str,
) -> Result<()> {
    if !replay_auth_ok(req, token) {
        // Never echo the presented token — not to the client, not to the log.
        eprintln!("seance web: [{peer}] replay auth failure on {}", req.path());
        return json_err(stream, 401, "Unauthorized", "bad token");
    }
    let root = replayexport::ring_root();
    match req.path() {
        "/replay/list" => match replayexport::coverage(&root) {
            Ok(cov) => json_ok(stream, &serde_json::to_vec(&cov)?),
            Err(e) => json_err(stream, 500, "Internal Server Error", &e.to_string()),
        },
        "/replay/manifest" => {
            let spec = match manifest_spec(req, &root) {
                Ok(s) => s,
                Err(e) => return json_err(stream, 400, "Bad Request", &e.to_string()),
            };
            match replayexport::build_manifest(&root, &spec) {
                Ok(m) => json_ok(stream, &serde_json::to_vec(&m)?),
                Err(e) => json_err(stream, 500, "Internal Server Error", &e.to_string()),
            }
        }
        "/replay/pane" => {
            let slug = match req.query("slug").filter(|s| !s.is_empty()) {
                Some(s) => s,
                None => return json_err(stream, 400, "Bad Request", "missing slug"),
            };
            let (from, to) = match (q_u64(req, "from_ms"), q_u64(req, "to_ms")) {
                (Ok(f), Ok(t)) => (f, t),
                (Err(e), _) | (_, Err(e)) => {
                    return json_err(stream, 400, "Bad Request", &e.to_string())
                }
            };
            match replayexport::slice_pane(&root, &slug, from, to)
                .and_then(|s| replayexport::gzip(&s))
            {
                Ok(gz) => write_response(stream, 200, "OK", "application/octet-stream", true, &gz),
                Err(e) => json_err(stream, 500, "Internal Server Error", &e.to_string()),
            }
        }
        other => json_err(
            stream,
            404,
            "Not Found",
            &format!("no replay route {other}"),
        ),
    }
}

/// `?workspace=&from_ms=&to_ms=[&panes=a,b]` → an [`ExportSpec`] with no
/// chapter override (the bridge extracts them; the editor overrides at publish).
fn manifest_spec(req: &RequestLine, root: &Path) -> Result<replayexport::ExportSpec> {
    let workspace = req
        .query("workspace")
        .filter(|w| !w.is_empty())
        .ok_or_else(|| anyhow!("missing workspace"))?;
    let from_ms = q_u64(req, "from_ms")?;
    let to_ms = q_u64(req, "to_ms")?;
    let panes = match q_panes(req) {
        Some(p) => p,
        // allow_all: the bridge is local and the caller already picked a range,
        // so a daemon-less lookup degrades to "everything recorded in it".
        None => replayexport::resolve_panes(root, &workspace, from_ms, to_ms, true)?,
    };
    Ok(replayexport::ExportSpec {
        workspace,
        panes,
        from_ms,
        to_ms,
        title: None,
        chapters_override: None,
    })
}

/// `POST /replay/publish` body — the editor's finished cut.
#[derive(serde::Deserialize)]
struct PublishReq {
    workspace: String,
    #[serde(default)]
    panes: Option<Vec<String>>,
    from_ms: u64,
    to_ms: u64,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    chapters: Option<Vec<seance_core::replay::Chapter>>,
}

fn serve_replay_publish(
    stream: &mut TcpStream,
    peer: &str,
    req: &RequestLine,
    token: &str,
    body: Result<Vec<u8>>,
) -> Result<()> {
    if !replay_auth_ok(req, token) {
        eprintln!("seance web: [{peer}] replay auth failure on {}", req.path());
        return json_err(stream, 401, "Unauthorized", "bad token");
    }
    let body = match body {
        Ok(b) => b,
        Err(e) => return json_err(stream, 413, "Payload Too Large", &e.to_string()),
    };
    let parsed: PublishReq = match serde_json::from_slice(&body) {
        Ok(p) => p,
        Err(e) => {
            return json_err(
                stream,
                400,
                "Bad Request",
                &format!("bad publish body: {e}"),
            )
        }
    };

    let root = replayexport::ring_root();
    let panes = match parsed.panes.filter(|p| !p.is_empty()) {
        Some(p) => p,
        None => match replayexport::resolve_panes(
            &root,
            &parsed.workspace,
            parsed.from_ms,
            parsed.to_ms,
            true,
        ) {
            Ok(p) => p,
            Err(e) => return json_err(stream, 400, "Bad Request", &e.to_string()),
        },
    };
    let spec = replayexport::ExportSpec {
        workspace: parsed.workspace,
        panes,
        from_ms: parsed.from_ms,
        to_ms: parsed.to_ms,
        title: parsed.title,
        chapters_override: parsed.chapters,
    };

    let out_dir = replayexport::out_root().join(replayexport::now_ms().to_string());
    let result = replayexport::load_publish_config().and_then(|cfg| {
        let dir = replayexport::export_bundle(&spec, &out_dir, cfg.assets_url.as_deref())?;
        replayexport::publish(&dir, &cfg)
    });
    match result {
        Ok(url) => {
            eprintln!("seance web: [{peer}] published replay → {url}");
            json_ok(
                stream,
                serde_json::json!({ "url": url }).to_string().as_bytes(),
            )
        }
        Err(e) => {
            eprintln!("seance web: [{peer}] replay publish failed: {e}");
            json_err(stream, 500, "Internal Server Error", &e.to_string())
        }
    }
}

// ---------------------------------------------------------------------------
// websocket ↔ unix socket proxy
// ---------------------------------------------------------------------------

fn serve_ws(stream: TcpStream, peer: &str, req: &RequestLine, token: &str) -> Result<()> {
    // Accept unconditionally: a pre-upgrade 401 is invisible to a browser, so
    // auth failure is reported as a websocket close (see module docs).
    let mut ws = tungstenite::accept(stream).map_err(|e| anyhow!("websocket handshake: {e}"))?;

    let presented = req.query("token").unwrap_or_default();
    let ok = seance_core::auth::token_well_formed(&presented)
        && seance_core::auth::token_matches(token, &presented);
    if !ok {
        eprintln!("seance web: [{peer}] ws auth failure — closing 4401");
        let _ = ws.close(Some(CloseFrame {
            code: CloseCode::Library(CLOSE_BAD_TOKEN),
            reason: "bad token".into(),
        }));
        // Drive the close frame out; ConnectionClosed is the expected terminal.
        let _ = ws.flush();
        for _ in 0..8 {
            if ws.read().is_err() {
                break;
            }
        }
        return Ok(());
    }

    let sock = socket_path();
    let unix = match UnixStream::connect(&sock) {
        Ok(u) => u,
        Err(e) => {
            eprintln!(
                "seance web: [{peer}] daemon socket {} unreachable: {e}",
                sock.display()
            );
            let _ = ws.close(Some(CloseFrame {
                code: CloseCode::Away,
                reason: "daemon unavailable".into(),
            }));
            let _ = ws.flush();
            return Ok(());
        }
    };
    eprintln!("seance web: [{peer}] ws open → {}", sock.display());

    let mut unix_w = unix
        .try_clone()
        .context("cloning daemon socket for write half")?;
    let unix_r = unix;

    // unix → ws: a dedicated thread owns the read half and ships whole lines
    // over a channel; the ws itself never leaves this thread (not Sync).
    let (tx, rx): (Sender<String>, Receiver<String>) = mpsc::channel();
    let peer_owned = peer.to_string();
    let reader = thread::Builder::new()
        .name(format!("seance-web-daemon-{peer}"))
        .spawn(move || {
            let br = BufReader::new(unix_r);
            for line in br.lines() {
                match line {
                    Ok(l) => {
                        if tx.send(l).is_err() {
                            return;
                        }
                    }
                    Err(e) => {
                        eprintln!("seance web: [{peer_owned}] daemon read ended: {e}");
                        return;
                    }
                }
            }
        })
        .context("spawning daemon reader thread")?;

    // Poll the ws against a short read timeout so the outbound channel gets
    // serviced even when the client is silent. tungstenite retains partial
    // frame state across WouldBlock, so timing out mid-frame is safe.
    let _ = ws.get_ref().set_read_timeout(Some(WS_POLL));

    let reason = proxy_loop(&mut ws, &mut unix_w, &rx, peer);
    eprintln!("seance web: [{peer}] ws close ({reason})");

    let _ = ws.close(None);
    let _ = ws.flush();
    let _ = unix_w.shutdown(std::net::Shutdown::Both);
    let _ = reader.join();
    Ok(())
}

/// Returns a short human reason for the teardown.
fn proxy_loop(
    ws: &mut tungstenite::WebSocket<TcpStream>,
    unix_w: &mut UnixStream,
    rx: &Receiver<String>,
    peer: &str,
) -> String {
    loop {
        // ws → unix. The first message is the client hello; it passes through
        // untouched — version skew is the daemon's call, not the bridge's.
        match ws.read() {
            Ok(Message::Text(t)) => {
                if let Err(e) = write_line(unix_w, t.as_bytes()) {
                    return format!("daemon write failed: {e}");
                }
            }
            Ok(Message::Binary(b)) => {
                if let Err(e) = write_line(unix_w, &b) {
                    return format!("daemon write failed: {e}");
                }
            }
            // Ping is auto-ponged by tungstenite on read; Pong needs nothing.
            Ok(Message::Ping(_)) | Ok(Message::Pong(_)) | Ok(Message::Frame(_)) => {}
            Ok(Message::Close(_)) => return "client closed".into(),
            Err(tungstenite::Error::Io(e))
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) => {}
            Err(tungstenite::Error::ConnectionClosed) | Err(tungstenite::Error::AlreadyClosed) => {
                return "client gone".into()
            }
            Err(e) => {
                eprintln!("seance web: [{peer}] ws read error: {e}");
                return "ws error".into();
            }
        }

        // unix → ws. Drain everything queued, then idle briefly if empty so the
        // loop doesn't spin when both sides are quiet.
        let mut idled = false;
        loop {
            let next = if idled {
                rx.try_recv().map_err(|e| match e {
                    mpsc::TryRecvError::Empty => RecvTimeoutError::Timeout,
                    mpsc::TryRecvError::Disconnected => RecvTimeoutError::Disconnected,
                })
            } else {
                rx.recv_timeout(Duration::from_millis(1))
            };
            idled = true;
            match next {
                Ok(line) => {
                    if let Err(e) = ws.send(Message::Text(line)) {
                        return format!("ws write failed: {e}");
                    }
                }
                Err(RecvTimeoutError::Timeout) => break,
                Err(RecvTimeoutError::Disconnected) => return "daemon disconnected".into(),
            }
        }
        if let Err(e) = ws.flush() {
            return format!("ws flush failed: {e}");
        }
    }
}

/// One JSON object per line — strip any trailing newline the client already
/// sent so we never emit a blank line the daemon would have to tolerate.
fn write_line(unix_w: &mut UnixStream, payload: &[u8]) -> std::io::Result<()> {
    let body = payload
        .strip_suffix(b"\n")
        .map(|b| b.strip_suffix(b"\r").unwrap_or(b))
        .unwrap_or(payload);
    unix_w.write_all(body)?;
    unix_w.write_all(b"\n")?;
    unix_w.flush()
}

// ---------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_normal_request_line() {
        let r = parse_request_line("GET /ws?token=abc HTTP/1.1\r\n").expect("parse");
        assert_eq!(r.method, "GET");
        assert_eq!(r.path(), "/ws");
        assert_eq!(r.query("token").as_deref(), Some("abc"));
        assert_eq!(r.query("nope"), None);
    }

    #[test]
    fn rejects_malformed_request_lines() {
        assert!(parse_request_line("").is_none());
        assert!(parse_request_line("GET").is_none());
        assert!(parse_request_line("GET /x").is_none());
        assert!(parse_request_line("GET x HTTP/1.1").is_none());
        assert!(parse_request_line("GET /x GARBAGE").is_none());
    }

    #[test]
    fn query_is_percent_decoded() {
        let r = parse_request_line("GET /a?x=a%2Fb&y=2 HTTP/1.1").expect("parse");
        assert_eq!(r.query("x").as_deref(), Some("a/b"));
        assert_eq!(r.query("y").as_deref(), Some("2"));
    }

    #[test]
    fn sanitize_maps_root_to_index() {
        assert_eq!(sanitize_path("/"), Some(PathBuf::from("index.html")));
        assert_eq!(sanitize_path("/app.js"), Some(PathBuf::from("app.js")));
        assert_eq!(
            sanitize_path("/a/b/c.wasm"),
            Some(PathBuf::from("a/b/c.wasm"))
        );
        assert_eq!(sanitize_path("/a//b.css"), Some(PathBuf::from("a/b.css")));
    }

    #[test]
    fn sanitize_rejects_escapes() {
        assert_eq!(sanitize_path("../etc/passwd"), None);
        assert_eq!(sanitize_path("/../etc/passwd"), None);
        assert_eq!(sanitize_path("/a/../../etc/passwd"), None);
        assert_eq!(sanitize_path("a.js"), None);
        assert_eq!(sanitize_path("/a\\b"), None);
    }

    #[test]
    fn content_types_cover_the_web_client() {
        assert_eq!(
            content_type(Path::new("i.html")),
            "text/html; charset=utf-8"
        );
        assert_eq!(content_type(Path::new("a.css")), "text/css; charset=utf-8");
        assert_eq!(
            content_type(Path::new("a.js")),
            "text/javascript; charset=utf-8"
        );
        assert_eq!(content_type(Path::new("a.wasm")), "application/wasm");
        assert_eq!(content_type(Path::new("a.js.map")), "application/json");
        assert_eq!(content_type(Path::new("a.svg")), "image/svg+xml");
        assert_eq!(content_type(Path::new("a.png")), "image/png");
        assert_eq!(content_type(Path::new("a.ico")), "image/x-icon");
        assert_eq!(
            content_type(Path::new("a.txt")),
            "text/plain; charset=utf-8"
        );
        assert_eq!(content_type(Path::new("a.bin")), "application/octet-stream");
        assert_eq!(content_type(Path::new("noext")), "application/octet-stream");
    }

    #[test]
    fn html_and_js_are_no_cache() {
        assert!(no_cache(Path::new("i.html")));
        assert!(no_cache(Path::new("a.js")));
        assert!(no_cache(Path::new("a.wasm"))); // stale wasm vs fresh js glue = silent ABI break
        assert!(!no_cache(Path::new("a.png")));
    }

    #[test]
    fn detects_websocket_upgrade() {
        let head =
            "GET /ws HTTP/1.1\r\nHost: x\r\nUpgrade: websocket\r\nConnection: keep-alive, Upgrade";
        assert!(is_websocket_upgrade(head));
        let plain = "GET / HTTP/1.1\r\nHost: x";
        assert!(!is_websocket_upgrade(plain));
    }

    #[test]
    fn header_lookup_is_case_insensitive() {
        let head = "GET / HTTP/1.1\r\nHost: example\r\nsec-websocket-version: 13";
        assert_eq!(header(head, "HOST"), Some("example"));
        assert_eq!(header(head, "Sec-WebSocket-Version"), Some("13"));
        assert_eq!(header(head, "missing"), None);
    }

    #[test]
    fn finds_header_terminator() {
        assert_eq!(find_header_end(b"GET / HTTP/1.1\r\n\r\n"), Some(18));
        assert_eq!(find_header_end(b"GET / HTTP/1.1\r\n"), None);
    }

    #[test]
    fn minted_tokens_are_well_formed_shape() {
        let t = hex(&[0u8; 32]);
        assert_eq!(t.len(), seance_core::auth::TOKEN_LEN);
        assert!(seance_core::auth::token_well_formed(&t));
        assert_eq!(hex(&[0xde, 0xad, 0xbe, 0xef]), "deadbeef");
    }

    #[test]
    fn replay_query_params_parse() {
        let r = parse_request_line(
            "GET /replay/manifest?workspace=lab&from_ms=10&to_ms=20&panes=a%2Cb HTTP/1.1",
        )
        .expect("parse");
        assert_eq!(r.path(), "/replay/manifest");
        assert_eq!(q_u64(&r, "from_ms").unwrap(), 10);
        assert_eq!(q_u64(&r, "to_ms").unwrap(), 20);
        assert!(q_u64(&r, "nope").is_err());
        assert_eq!(q_panes(&r), Some(vec!["a".into(), "b".into()]));

        let bare = parse_request_line("GET /replay/list HTTP/1.1").expect("parse");
        assert_eq!(q_panes(&bare), None);
        assert!(q_u64(&bare, "from_ms").is_err());
    }

    #[test]
    fn replay_auth_rejects_absent_and_malformed_tokens() {
        let good = hex(&[7u8; 32]);
        let with = |t: &str| {
            parse_request_line(&format!("GET /replay/list?token={t} HTTP/1.1")).expect("parse")
        };
        assert!(replay_auth_ok(&with(&good), &good));
        assert!(!replay_auth_ok(&with("short"), &good));
        assert!(!replay_auth_ok(&with(&hex(&[8u8; 32])), &good));
        assert!(!replay_auth_ok(
            &parse_request_line("GET /replay/list HTTP/1.1").unwrap(),
            &good
        ));
    }

    #[test]
    fn publish_body_shape_round_trips() {
        let raw = r#"{"workspace":"lab","from_ms":1,"to_ms":2,"title":"t",
                      "chapters":[{"t_ms":5,"pane":"w-1","text":"hi"}]}"#;
        let p: PublishReq = serde_json::from_str(raw).unwrap();
        assert_eq!(p.workspace, "lab");
        assert!(p.panes.is_none());
        assert_eq!(p.chapters.unwrap()[0].text, "hi");
        // panes optional, chapters optional
        let min: PublishReq =
            serde_json::from_str(r#"{"workspace":"lab","from_ms":1,"to_ms":2}"#).unwrap();
        assert!(min.title.is_none() && min.chapters.is_none());
    }

    #[test]
    fn wildcard_binds_print_a_clickable_url() {
        assert_eq!(url_host("0.0.0.0:9666"), "127.0.0.1:9666");
        assert_eq!(url_host("[::]:9666"), "127.0.0.1:9666");
        assert_eq!(url_host("127.0.0.1:9666"), "127.0.0.1:9666");
    }
}
