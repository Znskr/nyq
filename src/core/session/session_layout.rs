//! Session-id layout rules shared by every session store.
//!
//! Mirrors `zig-reference/src/core/session/session_layout.zig`.

use crate::core::shared::io;

/// Number of random bytes folded into each generated session id.
const SESSION_ID_RANDOM_BYTES: usize = 8;

/// Error returned when a session id violates the layout rules (empty, too
/// long, path-unsafe, or containing an unsupported byte).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InvalidSessionId;

/// Builds the absolute directory path for a session. The id is validated
/// before any path is produced.
pub fn session_dir_path(sessions_dir: &str, session_id: &[u8]) -> Result<String, InvalidSessionId> {
    validate_session_id(session_id)?;
    let id = std::str::from_utf8(session_id).expect("validated session ids are ASCII");
    Ok(format!("{}/{}", sessions_dir, id))
}

/// Validates a session id against the shared layout rules: 1..=255 bytes,
/// not `.` or `..`, and only ASCII alphanumerics plus `.`, `_`, and `-`.
pub fn validate_session_id(session_id: &[u8]) -> Result<(), InvalidSessionId> {
    if session_id.is_empty()
        || session_id.len() > 255
        || session_id == b"."
        || session_id == b".."
    {
        return Err(InvalidSessionId);
    }
    for &byte in session_id {
        if !byte.is_ascii_alphanumeric() && byte != b'.' && byte != b'_' && byte != b'-' {
            return Err(InvalidSessionId);
        }
    }
    Ok(())
}

/// Generates a fresh, layout-valid session id.
pub fn generate_session_id() -> String {
    let random_hex = hex_lower(&random_bytes());
    format!(
        "{}-{}-{}",
        io::milli_timestamp(),
        io::nano_timestamp(),
        random_hex
    )
}

fn random_bytes() -> [u8; SESSION_ID_RANDOM_BYTES] {
    // Seeded from wall-clock nanoseconds; the generated id only needs to be a
    // layout-valid random-looking suffix, not cryptographically secure.
    let mut seed = io::nano_timestamp() as u64 ^ 0x9e37_79b9_7f4a_7c15;
    let mut out = [0u8; SESSION_ID_RANDOM_BYTES];
    for byte in out.iter_mut() {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        *byte = (seed >> 24) as u8;
    }
    out
}

fn hex_lower(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

#[cfg(test)]
fn is_lower_hex(text: &str) -> bool {
    text.bytes()
        .all(|c| (b'0'..=b'9').contains(&c) || (b'a'..=b'f').contains(&c))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_session_id_includes_random_hex_suffix() {
        let id = generate_session_id();
        let mut segments = id.split('-');
        let millis = segments.next().unwrap();
        let nanos = segments.next().unwrap();
        let suffix = segments.next().unwrap();
        assert!(segments.next().is_none());
        assert!(!millis.is_empty());
        assert!(!nanos.is_empty());
        assert!(millis.parse::<i64>().is_ok());
        assert!(nanos.parse::<i128>().is_ok());
        assert_eq!(16, suffix.len());
        assert!(is_lower_hex(suffix));
    }

    #[test]
    fn session_directory_path_rejects_unsafe_ids() {
        for id in ["", ".", "..", "../outside", "/tmp/outside", "nested/session", "nested\\session"] {
            assert_eq!(
                Err(InvalidSessionId),
                session_dir_path("/tmp/sessions", id.as_bytes())
            );
        }

        assert_eq!(
            Err(InvalidSessionId),
            session_dir_path("/tmp/sessions", &[0xff, b'a'])
        );

        let too_long = vec![b'a'; 256];
        assert_eq!(
            Err(InvalidSessionId),
            session_dir_path("/tmp/sessions", &too_long)
        );

        let dotted = session_dir_path("/tmp/sessions", b"session.v3").unwrap();
        assert_eq!("/tmp/sessions/session.v3", dotted);

        for id in [".hidden-session", "session..branch", "last", "resume"] {
            let valid = session_dir_path("/tmp/sessions", id.as_bytes()).unwrap();
            assert!(valid.ends_with(id));
        }

        assert_eq!(
            Err(InvalidSessionId),
            session_dir_path("/tmp/sessions", &[b'a', 0, b'b'])
        );

        let max_id = vec![b'a'; 255];
        let maximum = session_dir_path("/tmp/sessions", &max_id).unwrap();
        assert!(maximum.as_bytes().ends_with(&max_id));
    }
}
