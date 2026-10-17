//! HTTP clients. rustls runs on the ring crypto provider (much smaller than
//! aws-lc), installed once before the first client is built.

use std::sync::Once;

/// Install the process-wide rustls provider. Clients built by dependencies
/// (rmcp's OAuth and transports) rely on it too, so it runs before any of them.
pub fn init() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

pub fn builder() -> reqwest::ClientBuilder {
    init();
    reqwest::Client::builder()
}

pub fn client() -> reqwest::Client {
    init();
    reqwest::Client::new()
}
