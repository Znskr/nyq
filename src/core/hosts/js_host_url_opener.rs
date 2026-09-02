//! JS host URL opener behind the `host.UrlOpener` contract
//! (ported 1:1 from zig-reference/src/core/hosts/js_host_url_opener.zig).

use super::host;

/// Host import stub modeled like `js_host_config_store`: the Zig
/// `extern "fx" fn fx_open_url` is supplied by the JavaScript/WASM host. This
/// crate builds and tests standalone with no host present, so the stub reports
/// "not handled" (0).
fn fx_open_url(_url: &[u8]) -> i32 {
    0
}

#[allow(non_upper_case_globals)]
pub const opener: host::UrlOpener = host::UrlOpener {
    context: 0,
    open_fn: open,
};

fn open(_: usize, url: &[u8]) -> Result<bool, host::UrlOpenError> {
    Ok(open_with(fx_open_url, url))
}

fn open_with(call: fn(&[u8]) -> i32, url: &[u8]) -> bool {
    call(url) == 1
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no_handler(_: &[u8]) -> i32 {
        0
    }

    #[test]
    fn js_host_url_opener_keeps_the_manual_fallback_without_a_handler() {
        assert!(!open_with(no_handler, b"https://vercel.test/login"));
    }
}
