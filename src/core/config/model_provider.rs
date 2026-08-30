//! Provider identifiers and provider authorization policy.
//!
//! Mirrors `zig-reference/src/core/config/model_provider.zig`.

use crate::core::shared::types::CredentialSource;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProviderId {
    Gateway,
    Codex,
    Grok,
}

impl ProviderId {
    pub const ALL: [ProviderId; 3] = [ProviderId::Gateway, ProviderId::Codex, ProviderId::Grok];

    pub fn tag_name(self) -> &'static str {
        match self {
            Self::Gateway => "gateway",
            Self::Codex => "codex",
            Self::Grok => "grok",
        }
    }

    pub fn index(self) -> usize {
        match self {
            Self::Gateway => 0,
            Self::Codex => 1,
            Self::Grok => 2,
        }
    }

    pub fn from_index(index: usize) -> ProviderId {
        match index {
            0 => Self::Gateway,
            1 => Self::Codex,
            _ => Self::Grok,
        }
    }
}

pub struct ProviderSelection<'a> {
    pub provider: ProviderId,
    pub model: &'a str,
}

pub fn parse(value: &str) -> Option<ProviderId> {
    if value.eq_ignore_ascii_case("gateway") {
        return Some(ProviderId::Gateway);
    }
    if value.eq_ignore_ascii_case("codex") {
        return Some(ProviderId::Codex);
    }
    if value.eq_ignore_ascii_case("grok") {
        return Some(ProviderId::Grok);
    }
    None
}

pub fn authorizes_credential(provider: ProviderId, source: Option<CredentialSource>) -> bool {
    let selected = match source {
        Some(selected) => selected,
        None => return false,
    };
    match provider {
        ProviderId::Gateway => {
            selected != CredentialSource::ChatgptSubscription
                && selected != CredentialSource::GrokSubscription
        }
        ProviderId::Codex => selected == CredentialSource::ChatgptSubscription,
        ProviderId::Grok => selected == CredentialSource::GrokSubscription,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_providers_authorize_only_their_own_credential_origins() {
        assert!(authorizes_credential(
            ProviderId::Gateway,
            Some(CredentialSource::AiGatewayApiKey)
        ));
        assert!(authorizes_credential(
            ProviderId::Gateway,
            Some(CredentialSource::FxLogin)
        ));
        assert!(!authorizes_credential(
            ProviderId::Gateway,
            Some(CredentialSource::ChatgptSubscription)
        ));
        assert!(authorizes_credential(
            ProviderId::Codex,
            Some(CredentialSource::ChatgptSubscription)
        ));
        assert!(!authorizes_credential(
            ProviderId::Codex,
            Some(CredentialSource::AiGatewayApiKey)
        ));
        assert!(!authorizes_credential(ProviderId::Codex, None));
        assert!(authorizes_credential(
            ProviderId::Grok,
            Some(CredentialSource::GrokSubscription)
        ));
        assert!(!authorizes_credential(
            ProviderId::Grok,
            Some(CredentialSource::ChatgptSubscription)
        ));
        assert!(!authorizes_credential(
            ProviderId::Gateway,
            Some(CredentialSource::GrokSubscription)
        ));
    }

    #[test]
    fn provider_parsing_exposes_gateway_codex_and_grok() {
        assert_eq!(ProviderId::Gateway, parse("gateway").unwrap());
        assert_eq!(ProviderId::Codex, parse("CODEX").unwrap());
        assert_eq!(ProviderId::Grok, parse("GROK").unwrap());
        assert_eq!(None, parse("openai-codex"));
        assert_eq!(None, parse(""));
    }
}
