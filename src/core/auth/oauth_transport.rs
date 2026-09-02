//! HTTP transport abstraction for OAuth flows.
//!
//! Ported 1:1 from `zig-reference/src/core/auth/oauth_transport.zig`. The Zig
//! `Provider` stores a context pointer plus a function pointer; the Rust port
//! models the same value as a boxed closure so the context stays captured
//! without `unsafe`. The `alloc` parameter disappears because Rust uses the
//! global allocator.

use crate::core::auth::secret;
use std::sync::atomic::AtomicBool;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Method {
    Get,
    PostForm,
    PostJson,
}

/// One outbound request. All borrowed fields live only for the duration of the
/// `Provider::execute` call.
#[derive(Clone, Copy, Debug)]
pub struct Request<'a> {
    pub method: Method,
    pub url: &'a [u8],
    pub payload: Option<&'a [u8]>,
    /// Optional preformatted Authorization value, borrowed for this request.
    pub authorization: Option<&'a [u8]>,
    pub cancel_flag: Option<&'a AtomicBool>,
    pub deadline: Option<i64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Disposition {
    Accepted,
    Rejected,
}

#[derive(Debug, PartialEq)]
pub struct Response {
    pub disposition: Disposition,
    /// Owned bytes returned by the transport.
    pub body: Vec<u8>,
}

impl Response {
    pub fn deinit(&mut self) {
        secret::zero_and_free(std::mem::take(&mut self.body));
        self.disposition = Disposition::Rejected;
    }

    /// Transfers ownership of `body` to the caller.
    pub fn take_body(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.body)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransportError {
    OAuthTransportUnavailable,
}

pub type ExecuteFn = Box<dyn for<'a> Fn(Request<'a>) -> Result<Response, TransportError>>;

pub struct Provider {
    execute_fn: ExecuteFn,
}

impl Provider {
    pub fn new(execute_fn: ExecuteFn) -> Self {
        Self { execute_fn }
    }

    pub fn execute(&self, request: Request<'_>) -> Result<Response, TransportError> {
        (self.execute_fn)(request)
    }
}

fn unavailable(_: Request<'_>) -> Result<Response, TransportError> {
    Err(TransportError::OAuthTransportUnavailable)
}

/// A transport that always reports the backend as unavailable. Returns a fresh
/// provider on each call because the boxed closure is not `const`-constructible.
pub fn unavailable_provider() -> Provider {
    Provider::new(Box::new(unavailable))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn response_transfers_owned_bytes_exactly_once() {
        let mut response = Response {
            disposition: Disposition::Accepted,
            body: b"secret".to_vec(),
        };
        let body = response.take_body();
        response.deinit();

        assert_eq!(0, response.body.len());
        assert_eq!(Disposition::Rejected, response.disposition);
        assert_eq!(b"secret", body.as_slice());
        secret::zero_and_free(body);
    }

    #[test]
    fn unavailable_provider_rejects_every_request() {
        let provider = unavailable_provider();
        let result = provider.execute(Request {
            method: Method::Get,
            url: b"https://example.test",
            payload: None,
            authorization: None,
            cancel_flag: None,
            deadline: None,
        });
        assert_eq!(Err(TransportError::OAuthTransportUnavailable), result);
    }
}
