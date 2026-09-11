//! Process instance identity tokens.
//!
//! Ported from `zig-reference/src/core/background/process_supervisor.zig`
//! (ProcessInstanceToken slice; the supervisor itself is a later wave).

use crate::core::shared::types::StableBackgroundRecordId;

pub const MAX_TOKEN_BYTES: usize = 128;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TokenError {
    InvalidProcessInstanceToken,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProcessInstanceToken {
    bytes: [u8; MAX_TOKEN_BYTES],
    len: u8,
}

impl Default for ProcessInstanceToken {
    fn default() -> Self {
        Self {
            bytes: [0u8; MAX_TOKEN_BYTES],
            len: 0,
        }
    }
}

impl ProcessInstanceToken {
    pub fn parse(text: &[u8]) -> Result<Self, TokenError> {
        if text.is_empty() || text.len() > MAX_TOKEN_BYTES {
            return Err(TokenError::InvalidProcessInstanceToken);
        }
        for &byte in text {
            if !byte.is_ascii()
                || byte.is_ascii_uppercase()
                || byte.is_ascii_whitespace()
                || byte.is_ascii_control()
            {
                return Err(TokenError::InvalidProcessInstanceToken);
            }
        }
        let mut parts = text.split(|&b| b == b':');
        let platform = parts.next().ok_or(TokenError::InvalidProcessInstanceToken)?;
        let boot_id = parts.next().ok_or(TokenError::InvalidProcessInstanceToken)?;
        if !is_lower_hex(boot_id, 32) {
            return Err(TokenError::InvalidProcessInstanceToken);
        }
        if platform == b"linux" {
            let start_ticks = parts.next().ok_or(TokenError::InvalidProcessInstanceToken)?;
            if parts.next().is_some() || !is_canonical_decimal(start_ticks) {
                return Err(TokenError::InvalidProcessInstanceToken);
            }
        } else if platform == b"macos" {
            let start_sec = parts.next().ok_or(TokenError::InvalidProcessInstanceToken)?;
            let start_usec = parts.next().ok_or(TokenError::InvalidProcessInstanceToken)?;
            if parts.next().is_some()
                || !is_canonical_decimal(start_sec)
                || !is_canonical_decimal(start_usec)
            {
                return Err(TokenError::InvalidProcessInstanceToken);
            }
        } else {
            return Err(TokenError::InvalidProcessInstanceToken);
        }
        let mut token = ProcessInstanceToken::default();
        token.bytes[..text.len()].copy_from_slice(text);
        token.len = text.len() as u8;
        Ok(token)
    }

    pub fn view(&self) -> &[u8] {
        &self.bytes[..self.len as usize]
    }

    pub fn eql(&self, other: &ProcessInstanceToken) -> bool {
        self.view() == other.view()
    }
}

pub type StableId = StableBackgroundRecordId;

fn is_lower_hex(value: &[u8], expected_len: usize) -> bool {
    if value.len() != expected_len {
        return false;
    }
    value
        .iter()
        .all(|&byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn is_canonical_decimal(value: &[u8]) -> bool {
    if value.is_empty() {
        return false;
    }
    if value.len() > 1 && value[0] == b'0' {
        return false;
    }
    if !value.iter().all(|&byte| byte.is_ascii_digit()) {
        return false;
    }
    std::str::from_utf8(value)
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .is_some()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TokenMatch {
    Matched,
    Missing,
    Mismatched,
    Unavailable,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_accepts_linux_and_macos_shapes() {
        let boot_id = "0123456789abcdef0123456789abcdef";
        let linux = format!("linux:{boot_id}:12345");
        let token = ProcessInstanceToken::parse(linux.as_bytes()).unwrap();
        assert_eq!(token.view(), linux.as_bytes());
        assert!(token.eql(&ProcessInstanceToken::parse(linux.as_bytes()).unwrap()));

        let macos = format!("macos:{boot_id}:123:456");
        assert!(ProcessInstanceToken::parse(macos.as_bytes()).is_ok());
    }

    #[test]
    fn parse_rejects_invalid_shapes() {
        assert!(ProcessInstanceToken::parse(b"").is_err());
        assert!(ProcessInstanceToken::parse(&[b'a'; 129]).is_err());
        assert!(ProcessInstanceToken::parse(b"LINUX:0123456789abcdef0123456789abcdef:1").is_err());
        assert!(ProcessInstanceToken::parse(b"linux:0123456789ABCDEF0123456789abcdef:1").is_err());
        assert!(ProcessInstanceToken::parse(b"linux:0123456789abcdef0123456789abcde:1").is_err());
        assert!(ProcessInstanceToken::parse(b"linux:0123456789abcdef0123456789abcdef:01").is_err());
        assert!(ProcessInstanceToken::parse(b"windows:0123456789abcdef0123456789abcdef:1").is_err());
        assert!(ProcessInstanceToken::parse(b"macos:0123456789abcdef0123456789abcdef:1").is_err());
        assert!(ProcessInstanceToken::parse(b"linux:0123456789abcdef0123456789abcdef:1:2").is_err());
        assert!(ProcessInstanceToken::parse(b"linux:0123456789abcdef0123456789abcdef").is_err());
    }
}
