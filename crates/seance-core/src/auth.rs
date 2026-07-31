//! Web-attach token: format + constant-time verification.
//!
//! Transport is tailscale-only for now, but auth is a protocol feature from
//! byte one: the `seance web` bridge requires a daemon-minted bearer token on
//! every websocket attach. Minting/persisting the token (files, RNG) is the
//! native side's job; this module owns the format and the comparison so every
//! bridge implementation verifies identically.

/// Length of a seance web token in characters (hex, 32 bytes of entropy).
pub const TOKEN_LEN: usize = 64;

/// Shape check: 64 lowercase-hex chars. Cheap pre-filter before compare.
pub fn token_well_formed(t: &str) -> bool {
    t.len() == TOKEN_LEN && t.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}

/// Constant-time equality over the full token length. Returns false on any
/// length mismatch without early exit inside the comparison loop.
pub fn token_matches(expected: &str, presented: &str) -> bool {
    let a = expected.as_bytes();
    let b = presented.as_bytes();
    if a.len() != b.len() || a.is_empty() {
        return false;
    }
    let mut diff: u8 = 0;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn well_formed_accepts_hex64() {
        let t = "a".repeat(64);
        assert!(token_well_formed(&t));
        assert!(!token_well_formed(&"A".repeat(64)));
        assert!(!token_well_formed("abc"));
    }

    #[test]
    fn matches_exact_only() {
        let t = "0f".repeat(32);
        assert!(token_matches(&t, &t.clone()));
        let mut wrong = t.clone();
        wrong.replace_range(0..1, "1");
        assert!(!token_matches(&t, &wrong));
        assert!(!token_matches(&t, ""));
    }
}
