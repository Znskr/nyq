//! Derives a persistable, non-secret identity. Gateway sources use the
//! selected credential slot; provider subscriptions require a stable
//! account ID.
//!
//! Ported 1:1 from `zig-reference/src/core/auth/credential_authority.zig`.
//! Zig uses `std.crypto.hash.sha2.Sha256`; the Rust port keeps the
//! zero-dependency policy with a self-contained SHA-256 implementation.

use crate::core::shared::types::CredentialSource;

pub const DIGEST_LENGTH: usize = 32;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Identity {
    pub bytes: [u8; DIGEST_LENGTH],
}

impl Identity {
    pub fn eql(&self, other: &Identity) -> bool {
        self.bytes == other.bytes
    }
}

/// Public SHA-256 helper for modules that need the same digest primitive
/// Zig's `std.crypto.hash.sha2.Sha256` provides.
pub fn sha256(data: &[u8]) -> [u8; DIGEST_LENGTH] {
    let mut hash = Sha256::new();
    hash.update(data);
    hash.finalize()
}

/// Derives a persistable, non-secret identity. Gateway sources use the
/// selected credential slot; provider subscriptions require a stable
/// account ID.
pub fn derive(source: CredentialSource, account_id: Option<&[u8]>) -> Option<Identity> {
    let mut hash = Sha256::new();
    hash.update(b"fx-credential-authority-v1\x00");
    // Zig hashes `@tagName(source)`; the ported tag names match
    // `persisted_name()`.
    hash.update(source.persisted_name().as_bytes());
    match source {
        CredentialSource::VercelOidcToken
        | CredentialSource::AiGatewayApiKey
        | CredentialSource::FxLogin
        | CredentialSource::StoredKey => {
            hash.update(b"\x00slot\x00");
        }
        CredentialSource::ChatgptSubscription | CredentialSource::GrokSubscription => {
            let account = account_id?;
            if account.is_empty() {
                return None;
            }
            hash.update(b"\x00account\x00");
            hash.update(account);
        }
    }
    Some(Identity {
        bytes: hash.finalize(),
    })
}

/// Minimal FIPS 180-4 SHA-256 (zero-dependency port policy).
struct Sha256 {
    state: [u32; 8],
    buffer: [u8; 64],
    buffer_len: usize,
    total_len: u64,
}

const K: [u32; 64] = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
    0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
    0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
    0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
    0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
    0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
    0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
    0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
    0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
    0xc67178f2,
];

impl Sha256 {
    fn new() -> Self {
        Sha256 {
            state: [
                0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c,
                0x1f83d9ab, 0x5be0cd19,
            ],
            buffer: [0; 64],
            buffer_len: 0,
            total_len: 0,
        }
    }

    fn update(&mut self, mut data: &[u8]) {
        self.total_len = self.total_len.wrapping_add(data.len() as u64);
        if self.buffer_len > 0 {
            let take = usize::min(64 - self.buffer_len, data.len());
            self.buffer[self.buffer_len..self.buffer_len + take].copy_from_slice(&data[..take]);
            self.buffer_len += take;
            data = &data[take..];
            if self.buffer_len == 64 {
                let block = self.buffer;
                self.compress(&block);
                self.buffer_len = 0;
            }
        }
        while data.len() >= 64 {
            let mut block = [0u8; 64];
            block.copy_from_slice(&data[..64]);
            self.compress(&block);
            data = &data[64..];
        }
        if !data.is_empty() {
            self.buffer[..data.len()].copy_from_slice(data);
            self.buffer_len = data.len();
        }
    }

    fn finalize(mut self) -> [u8; DIGEST_LENGTH] {
        let bit_len = self.total_len.wrapping_mul(8);
        self.update(&[0x80]);
        while self.buffer_len != 56 {
            self.update(&[0]);
        }
        // Length is appended directly so `total_len` bookkeeping stays unused.
        self.buffer[56..64].copy_from_slice(&bit_len.to_be_bytes());
        let block = self.buffer;
        self.compress(&block);
        let mut out = [0u8; DIGEST_LENGTH];
        for (i, word) in self.state.iter().enumerate() {
            out[i * 4..i * 4 + 4].copy_from_slice(&word.to_be_bytes());
        }
        out
    }

    fn compress(&mut self, block: &[u8; 64]) {
        let mut w = [0u32; 64];
        for (i, chunk) in block.chunks_exact(4).enumerate() {
            w[i] = u32::from_be_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = self.state;
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let temp1 = h
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let temp2 = s0.wrapping_add(maj);
            h = g;
            g = f;
            f = e;
            e = d.wrapping_add(temp1);
            d = c;
            c = b;
            b = a;
            a = temp1.wrapping_add(temp2);
        }
        self.state[0] = self.state[0].wrapping_add(a);
        self.state[1] = self.state[1].wrapping_add(b);
        self.state[2] = self.state[2].wrapping_add(c);
        self.state[3] = self.state[3].wrapping_add(d);
        self.state[4] = self.state[4].wrapping_add(e);
        self.state[5] = self.state[5].wrapping_add(f);
        self.state[6] = self.state[6].wrapping_add(g);
        self.state[7] = self.state[7].wrapping_add(h);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credential_authority_uses_account_identity_for_provider_subscriptions() {
        let first = derive(CredentialSource::ChatgptSubscription, Some(b"acct_1")).unwrap();
        let refreshed =
            derive(CredentialSource::ChatgptSubscription, Some(b"acct_1")).unwrap();
        let other = derive(CredentialSource::ChatgptSubscription, Some(b"acct_2")).unwrap();
        assert!(first.eql(&refreshed));
        assert!(!first.eql(&other));
        assert!(derive(CredentialSource::ChatgptSubscription, None).is_none());
        assert!(derive(CredentialSource::GrokSubscription, Some(b"")).is_none());
        assert_eq!(std::mem::size_of::<Identity>(), 32);
    }

    #[test]
    fn credential_authority_uses_non_secret_gateway_credential_slots() {
        let api_key = derive(CredentialSource::AiGatewayApiKey, None).unwrap();
        let same_slot =
            derive(CredentialSource::AiGatewayApiKey, Some(b"ignored-account")).unwrap();
        let stored_key = derive(CredentialSource::StoredKey, None).unwrap();
        assert!(api_key.eql(&same_slot));
        assert!(!api_key.eql(&stored_key));
        assert!(derive(CredentialSource::VercelOidcToken, None).is_some());
        assert!(derive(CredentialSource::FxLogin, None).is_some());
    }

    #[test]
    fn sha256_matches_known_vectors() {
        let mut hash = Sha256::new();
        hash.update(b"abc");
        let digest = hash.finalize();
        let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(
            hex,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
