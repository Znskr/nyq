//! API-key validation dispatch.
//!
//! Ported 1:1 from `zig-reference/src/core/auth/api_key_validator.zig`. The Zig
//! `Provider` stores a context pointer plus a function pointer taking an
//! allocator; the Rust port models the same value as a boxed closure so the
//! context stays captured without `unsafe`. The `alloc` parameter disappears
//! because Rust uses the global allocator.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Result {
    Accepted,
    Refused,
    Unavailable,
}

pub type ValidateFn = Box<dyn Fn(&[u8]) -> Result>;

pub struct Provider {
    validate_fn: ValidateFn,
}

impl Provider {
    pub fn new(validate_fn: ValidateFn) -> Self {
        Self { validate_fn }
    }

    pub fn validate(&self, api_key: &[u8]) -> Result {
        (self.validate_fn)(api_key)
    }
}

fn validate_unavailable(_: &[u8]) -> Result {
    Result::Unavailable
}

pub fn unavailable_provider() -> Provider {
    Provider::new(Box::new(validate_unavailable))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::rc::Rc;

    #[test]
    fn api_key_validator_dispatches_through_the_injected_provider() {
        let calls = Rc::new(Cell::new(0usize));
        let captured = Rc::clone(&calls);
        let provider = Provider::new(Box::new(move |api_key| {
            captured.set(captured.get() + 1);
            if api_key == b"accepted-key" {
                Result::Accepted
            } else {
                Result::Refused
            }
        }));

        assert_eq!(Result::Accepted, provider.validate(b"accepted-key"));
        assert_eq!(Result::Refused, provider.validate(b"other-key"));
        assert_eq!(2, calls.get());
    }

    #[test]
    fn unavailable_provider_reports_unavailable() {
        let provider = unavailable_provider();
        assert_eq!(Result::Unavailable, provider.validate(b"anything"));
    }
}
