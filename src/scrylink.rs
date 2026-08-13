//! Hand links we *own* to scry instead of the default browser.
//!
//! Scry (`~/work/scry`) is bio-zack's browser for pages we control — vita
//! reports, workshop pages, spoke sites. Those are exactly the links that show
//! up in seance panes, so a middle-click on `localhost:33801/reports/…` landing
//! in a general-purpose browser is the wrong end of the routing decision.
//!
//! The wire is scry's control socket (`~/.local/share/scry/control.sock`,
//! JSON lines, `docs/CONTROL.md` in that repo) rather than shelling
//! `scry ctl`: the CLI needs `run.sh` for `LD_LIBRARY_PATH=…libcef.so`, which
//! would mean hardcoding a clone path from someone's home directory into this
//! app. The socket is a stable location and needs no binary at all.
//!
//! **Everything here fails toward the default browser.** No socket, a wedged
//! scry, an unparseable reply, a host we don't own — all of them return
//! `false` and the caller opens the link the old way. A link that goes
//! nowhere would be worse than a link that opens in the wrong browser.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::time::Duration;

/// The workspace scry puts these tabs in. Bio-zack's decision, not a default
/// worth configuring yet: `general` is the one workspace scry refuses to let
/// anyone rename, so it's the one name that can't go stale.
const WORKSPACE: &str = "general";

/// How long to wait for scry to answer. Generous on purpose — this runs off
/// the UI thread, and giving up early means the link opens in *both* browsers
/// when scry was merely busy (an index rebuild blocks its main thread).
const REPLY_TIMEOUT: Duration = Duration::from_secs(5);

/// Try to open `url` in scry. `true` means scry has it and the caller should
/// do nothing else.
pub fn open(url: &str) -> bool {
    let Some(host) = host_of(url) else {
        return false;
    };
    if !is_ours(&host) {
        return false;
    }
    ask_scry(url).unwrap_or(false)
}

/// One request/response over the control socket. `Ok(false)` = scry answered
/// but declined; `Err` = we never got an answer.
fn ask_scry(url: &str) -> std::io::Result<bool> {
    let stream = UnixStream::connect(socket_path())?;
    stream.set_read_timeout(Some(REPLY_TIMEOUT))?;
    stream.set_write_timeout(Some(REPLY_TIMEOUT))?;
    let req = serde_json::json!({
        "cmd": "open",
        "url": url,
        "workspace": WORKSPACE,
        // Reuse a tab already showing this url rather than stacking duplicates
        // — clicking the same report link twice is a re-read, not a second
        // copy. `activate` raises the window, which is what "open" means.
        "new_tab": false,
        "activate": true,
    });
    let mut w = &stream;
    writeln!(w, "{req}")?;
    w.flush()?;
    let mut line = String::new();
    BufReader::new(&stream).read_line(&mut line)?;
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) else {
        return Ok(false);
    };
    Ok(v.get("reply").and_then(|r| r.as_str()) == Some("opened"))
}

/// `$XDG_DATA_HOME/scry/control.sock`, or the default data dir under `$HOME`.
///
/// Deliberately the XDG path only. Scry is a Linux app; a macOS thin client
/// (docs/REMOTE.md) simply finds nothing here and opens links the normal way,
/// which is the correct answer on a machine with no scry.
fn socket_path() -> std::path::PathBuf {
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(std::path::PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| {
            std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".local/share"))
        })
        .unwrap_or_else(|| std::path::PathBuf::from("/tmp"));
    base.join("scry").join("control.sock")
}

/// Hosts whose pages we publish, and so want in scry: `localhost` and
/// anything under it, `ham.xyz` and any sub-domain.
///
/// Loopback *addresses* (`127.0.0.1`, `::1`) deliberately aren't here even
/// though scry itself blesses them — they'd quietly redirect things nobody
/// asked to move, like the `seance replay edit` url.
fn is_ours(host: &str) -> bool {
    is_domain_or_subdomain(host, "localhost") || is_domain_or_subdomain(host, "ham.xyz")
}

/// `host == domain`, or `host` ends in `.domain`. The dot boundary is the
/// whole point: `ham.xyz.evil.com` is not a sub-domain of `ham.xyz`.
fn is_domain_or_subdomain(host: &str, domain: &str) -> bool {
    if host == domain {
        return true;
    }
    host.len() > domain.len()
        && host.ends_with(domain)
        && host.as_bytes()[host.len() - domain.len() - 1] == b'.'
}

/// The host of a url, lowercased, port and userinfo stripped.
///
/// A port of scry's `policy.rs::host_of` — the two must agree about what a url
/// points at, or seance would hand scry a link scry then refuses. Its traps
/// are the interesting part: a backslash is a path separator in the authority
/// (so `https://evil.com\@ham.xyz` is **evil.com**), userinfo is whatever
/// precedes the *last* `@`, and a `:` inside `[…]` is part of an IPv6 literal
/// rather than a port.
fn host_of(url: &str) -> Option<String> {
    let rest = url.split_once(':').map(|(_, r)| r)?;
    let rest = rest.strip_prefix("//")?;
    let rest = rest.trim_start_matches(['/', '\\']);
    let end = rest
        .find(|c| c == '/' || c == '\\' || c == '?' || c == '#')
        .unwrap_or(rest.len());
    let authority = &rest[..end];
    let hostport = match authority.rfind('@') {
        Some(i) => &authority[i + 1..],
        None => authority,
    };
    let host = if let Some(rest) = hostport.strip_prefix('[') {
        &rest[..rest.find(']')?]
    } else {
        match hostport.rsplit_once(':') {
            // A port is digits, or empty (`http://x:/`). Anything else means
            // the colon wasn't a port separator at all.
            Some((h, port)) if port.bytes().all(|b| b.is_ascii_digit()) => h,
            _ => hostport,
        }
    };
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    if host.is_empty() {
        None
    } else {
        Some(host)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn routed(url: &str) -> bool {
        host_of(url).is_some_and(|h| is_ours(&h))
    }

    #[test]
    fn our_pages_go_to_scry() {
        assert!(routed("http://localhost:33801/reports/2026-08-10-x"));
        assert!(routed("http://localhost"));
        assert!(routed("http://vita.localhost/"));
        assert!(routed("http://a.b.localhost:1234/x?y#z"));
        assert!(routed("https://ham.xyz"));
        assert!(routed("https://vita-reports.ham.xyz/r/abc"));
        assert!(routed("https://foo.spoke.ham.xyz/"));
        // Trailing root dot is the same host.
        assert!(routed("https://ham.xyz./x"));
        // Scheme and host case are not part of the identity.
        assert!(routed("HTTPS://VITA-REPORTS.HAM.XYZ/"));
    }

    #[test]
    fn everything_else_keeps_the_default_browser() {
        assert!(!routed("https://github.com/zackham/seance/pull/1"));
        assert!(!routed("https://news.ycombinator.com"));
        // Loopback addresses are deliberately not routed — see `is_ours`.
        assert!(!routed("http://127.0.0.1:9666/#replay-edit"));
        assert!(!routed("file:///home/zack/notes.md"));
        assert!(!routed("not a url"));
        assert!(!routed(""));
    }

    #[test]
    fn a_lookalike_host_is_not_our_host() {
        // The dot boundary: suffix matching on the string would send these to
        // scry, which is the whole reason it matches labels instead.
        assert!(!routed("https://ham.xyz.evil.com/"));
        assert!(!routed("https://notham.xyz/"));
        assert!(!routed("https://evil-localhost/"));
    }

    #[test]
    fn userinfo_does_not_get_to_name_the_host() {
        assert!(!routed("https://ham.xyz@evil.com/"));
        assert!(routed("https://user@ham.xyz/"));
        // WHATWG: a backslash in the authority ends it, so the host is
        // evil.com and `@ham.xyz` is the path.
        assert!(!routed("https://evil.com\\@ham.xyz"));
    }

    #[test]
    fn ports_and_ipv6_brackets_are_stripped_correctly() {
        assert_eq!(
            host_of("http://localhost:33801/x").as_deref(),
            Some("localhost")
        );
        assert_eq!(host_of("http://localhost:/x").as_deref(), Some("localhost"));
        assert_eq!(host_of("http://[::1]:8080/x").as_deref(), Some("::1"));
        // Not a port — the whole thing is the host.
        assert_eq!(
            host_of("http://host:notaport/x").as_deref(),
            Some("host:notaport")
        );
    }

    #[test]
    fn the_socket_lives_under_the_data_dir() {
        let p = socket_path();
        assert!(p.ends_with("scry/control.sock"), "unexpected path: {p:?}");
    }
}
