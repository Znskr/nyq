//! Overwrite an owned secret before returning its allocation to the allocator.
//!
//! Ported 1:1 from `zig-reference/src/core/auth/secret.zig`. Zig uses
//! `std.crypto.secureZero` with `@volatileCast`; the Rust port keeps the
//! zero-dependency policy by writing zeroes through a `black_box` so the
//! compiler cannot treat the writes as dead stores.

/// Overwrites `value` with zeroes and then drops the allocation.
#[inline(never)]
pub fn zero_and_free(mut value: Vec<u8>) {
    secure_zero(&mut value);
}

#[inline(never)]
fn secure_zero(value: &mut [u8]) {
    for byte in value.iter_mut() {
        *byte = 0;
    }
    std::hint::black_box(value);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_and_free_overwrites_bytes_before_release() {
        let mut value = [1u8, 2, 3];
        secure_zero(&mut value);
        assert_eq!(value, [0u8, 0, 0]);
    }

    #[test]
    fn zero_and_free_releases_an_empty_owned_secret() {
        zero_and_free(Vec::new());
    }
}
