//! HTTP clients.
//!
//! With `tls-rustls` (the default) rustls runs on the ring crypto provider,
//! installed once before the first client is built. With `tls-native` the
//! platform TLS stack is used and there is no process-wide provider to install.

#[cfg(feature = "tls-rustls")]
use std::sync::Once;

/// Install the process-wide rustls provider. Clients built by dependencies
/// (rmcp's OAuth and transports) rely on it too, so it runs before any of them.
/// A no-op with `tls-native`.
#[cfg(feature = "tls-rustls")]
pub fn init() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

#[cfg(not(feature = "tls-rustls"))]
pub fn init() {}

pub fn builder() -> reqwest::ClientBuilder {
    init();
    reqwest::Client::builder()
}

pub fn client() -> reqwest::Client {
    init();
    reqwest::Client::new()
}

/// Whether `url` may carry credentials: `https://`, or plain `http://` only
/// to a loopback host (local development). Anything else would send
/// credentials in cleartext or to a non-URL.
pub fn secure_url(url: &str) -> bool {
    let Ok(parsed) = reqwest::Url::parse(url) else {
        return false;
    };
    let Some(host) = parsed.host_str().filter(|h| !h.is_empty()) else {
        return false;
    };
    match parsed.scheme() {
        "https" => true,
        "http" => {
            let bare = host.trim_start_matches('[').trim_end_matches(']');
            host.eq_ignore_ascii_case("localhost")
                || bare
                    .parse::<std::net::IpAddr>()
                    .is_ok_and(|ip| ip.is_loopback())
        }
        _ => false,
    }
}

/// A URL override from environment variable `name`; a value that is not a
/// [`secure_url`] is ignored with a warning so the caller falls back to its
/// default.
pub fn secure_env(name: &str) -> Option<String> {
    let value = std::env::var(name)
        .ok()
        .map(|v| v.trim().to_owned())
        .filter(|v| !v.is_empty())?;
    if secure_url(&value) {
        Some(value)
    } else {
        eprintln!("nyq: ignoring {name}: not a secure URL (https, or http to a loopback host)");
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secure_urls_only() {
        for ok in [
            "https://api.zalthar.com",
            "https://api.zalthar.com/v1",
            "http://127.0.0.1:8080",
            "http://localhost:3000/v1",
            "http://[::1]:9000",
        ] {
            assert!(secure_url(ok), "{ok}");
        }
        for bad in [
            "http://evil.example",
            "http://10.0.0.1",
            "http://localhost.evil.example",
            "ftp://api.zalthar.com",
            "javascript:alert(1)",
            "api.zalthar.com",
            "",
        ] {
            assert!(!secure_url(bad), "{bad}");
        }
    }

    #[test]
    fn secure_env_requires_a_secure_url() {
        const VAR: &str = "NYQ_TEST_SECURE_ENV_URL";
        std::env::remove_var(VAR);
        assert_eq!(secure_env(VAR), None);
        for bad in ["http://evil.example", "not a url", "  "] {
            std::env::set_var(VAR, bad);
            assert_eq!(secure_env(VAR), None, "{bad}");
        }
        std::env::set_var(VAR, "https://api.example/v1");
        assert_eq!(secure_env(VAR).as_deref(), Some("https://api.example/v1"));
        std::env::set_var(VAR, " http://localhost:3000 ");
        assert_eq!(secure_env(VAR).as_deref(), Some("http://localhost:3000"));
