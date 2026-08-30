//! Host-owned configuration storage.
//!
//! Mirrors `zig-reference/src/core/config/js_host_config_store.zig`.
//!
//! The Zig original declares `fx_config_get` and `fx_config_set` as `extern
//! "fx"` imports supplied by the JavaScript/WASM host. This crate builds and
//! tests standalone with no host present, so the host surface is modeled as
//! host stubs that report "value absent" and "write rejected". This keeps the
//! public `load`/`setAccepted` control flow and error cases intact without
//! `unsafe` or an FFI link target.

pub enum LoadError {
    ValueTooLarge,
    HostFailure,
    OutOfMemory,
}

pub enum SaveError {
    HostFailure,
}

const INITIAL_CAPACITY: usize = 256;
const MAX_VALUE_BYTES: usize = 16 * 1024;

fn fx_config_get(_id: &[u8], _out: &mut [u8]) -> i32 {
    // No host value present.
    -2
}

fn fx_config_set(_id: &[u8], _value: &[u8]) -> i32 {
    // Host rejected the write.
    -1
}

/// Loads a host-owned configuration value. The caller owns the returned bytes.
pub fn load(id: &[u8]) -> Result<Option<Vec<u8>>, LoadError> {
    let mut capacity: usize = INITIAL_CAPACITY;
    while capacity <= MAX_VALUE_BYTES {
        let mut value = vec![0u8; capacity];
        let result = fx_config_get(id, &mut value);
        if result >= 0 {
            let length = result as usize;
            if length > value.len() {
                return Err(LoadError::HostFailure);
            }
            value.truncate(length);
            return Ok(Some(value));
        }
        match result {
            -2 => return Ok(None),
            -3 => {
                capacity *= 2;
                continue;
            }
            _ => return Err(LoadError::HostFailure),
        }
    }
    Err(LoadError::ValueTooLarge)
}

/// Persists a value only after the host has accepted and applied it.
pub fn set_accepted(id: &[u8], value: &[u8]) -> Result<(), SaveError> {
    if fx_config_set(id, value) != 0 {
        return Err(SaveError::HostFailure);
    }
    Ok(())
}
