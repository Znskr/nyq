//! Pure decisions for provider switches, logout selection, and sign-in.
//!
//! Ported 1:1 from `zig-reference/src/core/auth/auth_transition.zig`.
//! `credentials.Source` is a type alias for `types.CredentialSource`, so the
//! port references that shared type directly. Zig's `std.EnumSet` becomes a
//! plain `Vec<CredentialSource>`.

use crate::core::config::model_provider::ProviderId;
use crate::core::shared::types::CredentialSource;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProviderSwitchDecision {
    NoChange,
    Busy,
    Prepare,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProviderSwitchIntent {
    Manual,
    PostOauth,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProviderSwitchFacts {
    pub current: ProviderId,
    pub target: ProviderId,
    pub target_credential_ready: bool,
    pub intent: ProviderSwitchIntent,
    pub stream_active: bool,
    pub queued_prompts: usize,
}

pub fn decide_provider_switch(facts: ProviderSwitchFacts) -> ProviderSwitchDecision {
    if facts.intent == ProviderSwitchIntent::Manual
        && facts.current == facts.target
        && facts.target_credential_ready
    {
        return ProviderSwitchDecision::NoChange;
    }
    if facts.stream_active || facts.queued_prompts > 0 {
        return ProviderSwitchDecision::Busy;
    }
    ProviderSwitchDecision::Prepare
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogoutFacts {
    pub requested: Option<ProviderId>,
    pub selected: ProviderId,
    pub active_source: Option<CredentialSource>,
    pub available_sources: Vec<CredentialSource>,
}

pub fn decide_logout_provider(facts: LogoutFacts) -> ProviderId {
    if let Some(provider) = facts.requested {
        return provider;
    }
    if facts.selected == ProviderId::Grok
        || facts.active_source == Some(CredentialSource::GrokSubscription)
    {
        return ProviderId::Grok;
    }
    if facts.selected == ProviderId::Codex
        || facts.active_source == Some(CredentialSource::ChatgptSubscription)
    {
        return ProviderId::Codex;
    }

    let only_grok_login = facts
        .available_sources
        .contains(&CredentialSource::GrokSubscription)
        && !facts.available_sources.contains(&CredentialSource::FxLogin)
        && !facts
            .available_sources
            .contains(&CredentialSource::ChatgptSubscription);
    if only_grok_login {
        return ProviderId::Grok;
    }
    let only_codex_login = facts
        .available_sources
        .contains(&CredentialSource::ChatgptSubscription)
        && !facts.available_sources.contains(&CredentialSource::FxLogin)
        && !facts
            .available_sources
            .contains(&CredentialSource::GrokSubscription);
    if only_codex_login {
        return ProviderId::Codex;
    }
    ProviderId::Gateway
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SignInCompletionAction {
    Vercel,
    SwitchProvider(ProviderId),
    ActivateSource(CredentialSource),
}

pub fn sign_in_completion(
    provider: ProviderId,
    provider_routing_supported: bool,
) -> SignInCompletionAction {
    match provider {
        ProviderId::Gateway => SignInCompletionAction::Vercel,
        ProviderId::Codex => {
            if provider_routing_supported {
                SignInCompletionAction::SwitchProvider(ProviderId::Codex)
            } else {
                SignInCompletionAction::ActivateSource(CredentialSource::ChatgptSubscription)
            }
        }
        ProviderId::Grok => {
            if provider_routing_supported {
                SignInCompletionAction::SwitchProvider(ProviderId::Grok)
            } else {
                SignInCompletionAction::ActivateSource(CredentialSource::GrokSubscription)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_switch_and_logout_decisions_are_pure_and_provider_keyed() {
        assert_eq!(
            ProviderSwitchDecision::NoChange,
            decide_provider_switch(ProviderSwitchFacts {
                current: ProviderId::Codex,
                target: ProviderId::Codex,
                target_credential_ready: true,
                intent: ProviderSwitchIntent::Manual,
                stream_active: false,
                queued_prompts: 0,
            })
        );
        assert_eq!(
            ProviderSwitchDecision::Busy,
            decide_provider_switch(ProviderSwitchFacts {
                current: ProviderId::Gateway,
                target: ProviderId::Grok,
                target_credential_ready: true,
                intent: ProviderSwitchIntent::Manual,
                stream_active: true,
                queued_prompts: 0,
            })
        );

        let inventory = vec![CredentialSource::ChatgptSubscription];
        assert_eq!(
            ProviderId::Codex,
            decide_logout_provider(LogoutFacts {
                requested: None,
                selected: ProviderId::Gateway,
                active_source: None,
                available_sources: inventory.clone(),
            })
        );
        assert_eq!(
            ProviderId::Grok,
            decide_logout_provider(LogoutFacts {
                requested: Some(ProviderId::Grok),
                selected: ProviderId::Codex,
                active_source: Some(CredentialSource::ChatgptSubscription),
                available_sources: inventory,
            })
        );
    }

    #[test]
    fn sign_in_completion_selects_routing_or_credential_activation_without_effects() {
        assert_eq!(
            SignInCompletionAction::SwitchProvider(ProviderId::Codex),
            sign_in_completion(ProviderId::Codex, true)
        );
        assert_eq!(
            SignInCompletionAction::ActivateSource(CredentialSource::GrokSubscription),
            sign_in_completion(ProviderId::Grok, false)
        );
        assert_eq!(
            SignInCompletionAction::Vercel,
            sign_in_completion(ProviderId::Gateway, true)
        );
    }
}
