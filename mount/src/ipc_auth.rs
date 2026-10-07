// IPC protocol 2: the authenticated handshake every connection runs before
// its first verb. Wire format, message layout and test vectors follow the
// engine's `docs/dev/specs/ipc-authentication.md`.
//
// The token and both proofs are secrets: none of them is ever logged or put
// into an error message.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use hmac::{Hmac, Mac};
use sha2::Sha256;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

type HmacSha256 = Hmac<Sha256>;

pub const PROTOCOL: u64 = 2;
pub const SCOPE: &str = "full";
pub const CLIENT_NAME: &str = "unidrive-mount-linux";

/// Error text prefixes of the terminal handshake failures. They travel as
/// `IpcError::ServerError` so the FUSE errno mapping stays unchanged (EIO);
/// `IpcError::is_auth_terminal` keys off them.
pub const AUTH_FAILED: &str = "auth_failed";
pub const AUTH_UNAVAILABLE: &str = "auth_unavailable";

/// How a connection authenticates. Cloned into every connect (initial and
/// reconnect); the token file is read fresh each time.
#[derive(Clone, Debug)]
pub struct IpcAuth {
    pub token_file: Option<PathBuf>,
    pub profile: String,
    fallback_warned: Arc<AtomicBool>,
}

impl IpcAuth {
    pub fn new(token_file: Option<PathBuf>, profile: impl Into<String>) -> Self {
        Self { token_file, profile: profile.into(), fallback_warned: Arc::new(AtomicBool::new(false)) }
    }

    /// Log the protocol-1 fallback warning once per `IpcAuth` (shared by all
    /// clones), not once per connection.
    pub(crate) fn warn_fallback_once(&self) {
        if !self.fallback_warned.swap(true, Ordering::SeqCst) {
            tracing::warn!(
                token_file = ?self.token_file,
                "IPC: no token file and the daemon reports protocol 1; continuing unauthenticated (temporary fallback)"
            );
        }
    }
}

pub enum TokenRead {
    Key(Vec<u8>),
    Absent,
}

/// Read the token: one line of base64url (no padding) holding 32 bytes.
/// A missing or unreadable file is `Absent`; a file that is there but does
/// not hold a valid token is an error (fail closed).
pub fn read_token(path: &Path) -> Result<TokenRead, String> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(_) => return Ok(TokenRead::Absent),
    };
    let key = URL_SAFE_NO_PAD
        .decode(text.trim())
        .map_err(|_| format!("{AUTH_UNAVAILABLE}: token file {} is not base64url", path.display()))?;
    if key.len() != 32 {
        return Err(format!("{AUTH_UNAVAILABLE}: token file {} does not hold 32 bytes", path.display()));
    }
    Ok(TokenRead::Key(key))
}

pub fn new_nonce() -> Result<String, String> {
    let mut b = [0u8; 16];
    getrandom::fill(&mut b).map_err(|e| format!("{AUTH_FAILED}: no OS randomness for the nonce: {e}"))?;
    Ok(URL_SAFE_NO_PAD.encode(b))
}

fn client_message(profile: &str, nonce: &str, snonce: &str) -> String {
    format!("unidrive-ipc-v2|client|{SCOPE}|{}:{profile}|{nonce}|{snonce}", profile.len())
}

fn server_message(profile: &str, nonce: &str, snonce: &str, client_proof: &str) -> String {
    format!(
        "unidrive-ipc-v2|server|{SCOPE}|{}:{profile}|{nonce}|{snonce}|{client_proof}",
        profile.len()
    )
}

fn mac(key: &[u8], msg: &str) -> HmacSha256 {
    let mut m = HmacSha256::new_from_slice(key).expect("HMAC accepts any key length");
    m.update(msg.as_bytes());
    m
}

pub fn client_proof(key: &[u8], profile: &str, nonce: &str, snonce: &str) -> String {
    URL_SAFE_NO_PAD.encode(mac(key, &client_message(profile, nonce, snonce)).finalize().into_bytes())
}

/// The daemon side of the proof (used by the fake daemon in tests).
pub fn server_proof(key: &[u8], profile: &str, nonce: &str, snonce: &str, client_proof: &str) -> String {
    URL_SAFE_NO_PAD.encode(mac(key, &server_message(profile, nonce, snonce, client_proof)).finalize().into_bytes())
}

/// Constant-time check of the daemon's proof.
pub fn verify_server_proof(
    key: &[u8],
    profile: &str,
    nonce: &str,
    snonce: &str,
    client_proof: &str,
    server_proof: &str,
) -> bool {
    let Ok(got) = URL_SAFE_NO_PAD.decode(server_proof) else {
        return false;
    };
    mac(key, &server_message(profile, nonce, snonce, client_proof)).verify_slice(&got).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seq(from: u8, n: u8) -> Vec<u8> {
        (from..from + n).collect()
    }

    // Invariant: the proofs match the spec's fixed vectors byte for byte
    // (profile `p1`, token 00..1f, nonce 00..0f, snonce 10..1f).
    #[test]
    fn handshake_proofs_match_spec_test_vectors() {
        let key = seq(0, 32);
        assert_eq!(URL_SAFE_NO_PAD.encode(&key), "AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8");
        let nonce = URL_SAFE_NO_PAD.encode(seq(0, 16));
        let snonce = URL_SAFE_NO_PAD.encode(seq(16, 16));
        assert_eq!(nonce, "AAECAwQFBgcICQoLDA0ODw");
        assert_eq!(snonce, "EBESExQVFhcYGRobHB0eHw");
        let cp = client_proof(&key, "p1", &nonce, &snonce);
        assert_eq!(cp, "2TL-rOrEbuDB9pZ34fpxlSRakSFB9X49f3Kn7_plriA");
        let expected_sp = "qwZ4elXK7shenllL3frkDuObSaAhaSuBDra9Q94EuNo";
        assert_eq!(server_proof(&key, "p1", &nonce, &snonce, &cp), expected_sp);
        assert!(verify_server_proof(&key, "p1", &nonce, &snonce, &cp, expected_sp));
    }

    // Invariant: the length prefix is the UTF-8 byte length, not the char
    // count (spec vector: profile "café", 5 bytes, scope full).
    #[test]
    fn multibyte_profile_uses_utf8_byte_length() {
        let key = seq(0, 32);
        let nonce = URL_SAFE_NO_PAD.encode(seq(0, 16));
        let snonce = URL_SAFE_NO_PAD.encode(seq(16, 16));
        assert!(client_message("caf\u{e9}", &nonce, &snonce).contains("|5:caf\u{e9}|"));
        assert_eq!(
            client_proof(&key, "caf\u{e9}", &nonce, &snonce),
            "oVzRRJv7YFxwABUzPVE56lJmegMwKj_bEsfdATEzT7A"
        );
    }

    // Invariant: a proof that differs (or is not even base64url) never verifies.
    #[test]
    fn wrong_server_proof_is_rejected() {
        let key = seq(0, 32);
        let cp = client_proof(&key, "p1", "n", "s");
        assert!(!verify_server_proof(&key, "p1", "n", "s", &cp, "qwZ4elXK7shenllL3frkDuObSaAhaSuBDra9Q94EuNo"));
        assert!(!verify_server_proof(&key, "p1", "n", "s", &cp, "!!not base64!!"));
    }

    // Invariant: a token file with surrounding whitespace is accepted; one
    // holding the wrong number of bytes fails closed; a missing one is Absent.
    #[test]
    fn token_file_parsing() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("ipc.token");
        assert!(matches!(read_token(&p), Ok(TokenRead::Absent)));
        std::fs::write(&p, "AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8\n").unwrap();
        assert!(matches!(read_token(&p), Ok(TokenRead::Key(k)) if k == seq(0, 32)));
        std::fs::write(&p, "AAECAwQFBgcICQoLDA0ODw").unwrap();
        assert!(read_token(&p).is_err());
    }
}
