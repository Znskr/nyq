//! Per-provider model preferences.
//!
//! Mirrors `zig-reference/src/core/config/model_preferences.zig`.

use super::model_provider::ProviderId;

pub const PROVIDER_COUNT: usize = ProviderId::ALL.len();

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Preferences {
    pub values: [Option<Vec<u8>>; PROVIDER_COUNT],
}

impl Default for Preferences {
    fn default() -> Self {
        Preferences {
            values: std::array::from_fn(|_| None),
        }
    }
}

impl Preferences {
    pub fn get(&self, provider: ProviderId) -> Option<&[u8]> {
        self.values[provider.index()].as_deref()
    }

    pub fn put_copy(&mut self, provider: ProviderId, model: &[u8]) {
        let owned = model.to_vec();
        self.put_owned(provider, owned);
    }

    pub fn put_owned(&mut self, provider: ProviderId, model: Vec<u8>) {
        let index = provider.index();
        self.values[index] = Some(model);
    }

    pub fn take(&mut self, provider: ProviderId) -> Option<Vec<u8>> {
        let index = provider.index();
        self.values[index].take()
    }

    pub fn merge_owned_from(&mut self, incoming: &mut Preferences) {
        for provider in ProviderId::ALL {
            if let Some(model) = incoming.take(provider) {
                self.put_owned(provider, model);
            }
        }
    }

    pub fn count(&self) -> usize {
        self.values.iter().filter(|value| value.is_some()).count()
    }

    pub fn is_empty(&self) -> bool {
        self.count() == 0
    }

    pub fn deinit(&mut self) {
        self.values = std::array::from_fn(|_| None);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_preferences_are_bounded_and_provider_keyed() {
        let mut preferences = Preferences::default();

        preferences.put_copy(ProviderId::Gateway, b"gateway/model");
        preferences.put_copy(ProviderId::Codex, b"gpt-5.4");
        preferences.put_copy(ProviderId::Codex, b"gpt-5.6");

        assert_eq!(
            Some(b"gateway/model".as_slice()),
            preferences.get(ProviderId::Gateway)
        );
        assert_eq!(Some(b"gpt-5.6".as_slice()), preferences.get(ProviderId::Codex));
        assert_eq!(None, preferences.get(ProviderId::Grok));
        assert_eq!(2, preferences.count());

        preferences.deinit();
        assert!(preferences.is_empty());
    }
}
