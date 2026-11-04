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
