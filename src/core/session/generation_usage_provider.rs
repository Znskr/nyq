//! Injectable lookup providers for finalized generation usage.
//!
//! Mirrors `zig-reference/src/core/session/generation_usage_provider.zig`.

use std::any::Any;
use std::sync::atomic::AtomicBool;

use crate::core::config::model_provider::ProviderId;

pub struct LookupInput<'a> {
    pub credential: &'a [u8],
    pub tenant: Option<&'a [u8]>,
    pub origin: &'a [u8],
    pub generation_id: &'a [u8],
    pub cancel_flag: &'a AtomicBool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Record {
    pub id: Vec<u8>,
    pub created_at_ms: i64,
    pub model: Vec<u8>,
    pub total_cost: f64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub reasoning_tokens: Option<u64>,
    pub billable_web_search_calls: u64,
}

#[derive(Clone, Debug, PartialEq)]
pub enum LookupOutcome {
    Found(Record),
    Retry,
    PreservePending,
    Reject,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LookupError {
    Cancelled,
    Unavailable,
    OutOfMemory,
}

pub type LookupFn = for<'a> fn(
    context: Option<&dyn Any>,
    input: LookupInput<'a>,
) -> Result<LookupOutcome, LookupError>;

#[derive(Clone, Copy)]
pub struct Provider<'a> {
    /// The context must remain valid until every in-flight lookup returns.
    pub context: Option<&'a dyn Any>,
    pub lookup_fn: LookupFn,
}

impl Provider<'_> {
    pub fn lookup(&self, input: LookupInput<'_>) -> Result<LookupOutcome, LookupError> {
        (self.lookup_fn)(self.context, input)
    }
}

fn lookup_unavailable(
    _: Option<&dyn Any>,
    _: LookupInput<'_>,
) -> Result<LookupOutcome, LookupError> {
    Err(LookupError::Unavailable)
}

#[allow(non_upper_case_globals)]
pub const unavailable_provider: Provider<'static> = Provider {
    context: None,
    lookup_fn: lookup_unavailable,
};

#[derive(Clone, Copy, Default)]
pub struct Set<'a> {
    pub gateway: Option<Provider<'a>>,
    pub codex: Option<Provider<'a>>,
    pub grok: Option<Provider<'a>>,
}

impl<'a> Set<'a> {
    #[allow(non_snake_case)]
    pub fn gatewayOnly(provider: Provider<'a>) -> Set<'a> {
        Set {
            gateway: Some(provider),
            codex: None,
            grok: None,
        }
    }

    pub fn select(self, provider: ProviderId) -> Option<Provider<'a>> {
        match provider {
            ProviderId::Gateway => self.gateway,
            ProviderId::Codex => self.codex,
            ProviderId::Grok => self.grok,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    struct Fake {
        calls: AtomicUsize,
        saw_expected_input: AtomicBool,
    }

    fn fake_lookup(
        context: Option<&dyn Any>,
        input: LookupInput<'_>,
    ) -> Result<LookupOutcome, LookupError> {
        let fake = context.unwrap().downcast_ref::<Fake>().unwrap();
        fake.calls.fetch_add(1, Ordering::Relaxed);
        fake.saw_expected_input.store(
            input.credential == b"credential" && input.generation_id == b"generation",
            Ordering::Relaxed,
        );
        Ok(LookupOutcome::Found(Record {
            id: input.generation_id.to_vec(),
            created_at_ms: 0,
            model: b"provider/model".to_vec(),
            total_cost: 1.0,
            input_tokens: 2,
            output_tokens: 3,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            reasoning_tokens: None,
            billable_web_search_calls: 0,
        }))
    }

    #[test]
    fn generation_usage_lookup_dispatches_through_the_injected_provider() {
        let fake = Fake {
            calls: AtomicUsize::new(0),
            saw_expected_input: AtomicBool::new(false),
        };
        let provider = Provider {
            context: Some(&fake),
            lookup_fn: fake_lookup,
        };
        let cancel = AtomicBool::new(false);
        let outcome = provider
            .lookup(LookupInput {
                credential: b"credential",
                tenant: None,
                origin: b"https://provider.example",
                generation_id: b"generation",
                cancel_flag: &cancel,
            })
            .unwrap();

        assert_eq!(1, fake.calls.load(Ordering::Relaxed));
        assert!(fake.saw_expected_input.load(Ordering::Relaxed));
        match outcome {
            LookupOutcome::Found(record) => assert_eq!(b"provider/model", &record.model[..]),
            _ => panic!("expected found outcome"),
        }
    }

    #[test]
    fn generation_usage_providers_are_selected_by_provider_identity() {
        let routes = Set::gatewayOnly(unavailable_provider);
        assert!(routes.select(ProviderId::Gateway).is_some());
        assert!(routes.select(ProviderId::Codex).is_none());
        assert!(routes.select(ProviderId::Grok).is_none());
    }
}
