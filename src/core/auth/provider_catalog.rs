//! Auth provider catalog mapping slugs and aliases to provider identities.
//!
//! Ported 1:1 from `zig-reference/src/core/auth/provider_catalog.zig`.

use crate::core::config::model_provider::ProviderId;

pub struct Entry {
    pub id: ProviderId,
    pub slug: &'static str,
    pub aliases: &'static [&'static str],
    pub name: &'static str,
    pub route_name: &'static str,
    pub description: &'static str,
    pub subscription: bool,
}

#[allow(non_upper_case_globals)]
pub const entries: [Entry; 3] = [
    Entry {
        id: ProviderId::Gateway,
        slug: "vercel",
        aliases: &["gateway", "ai-gateway"],
        name: "Vercel AI Gateway",
        route_name: "Vercel AI Gateway",
        description: "Vercel account or AI Gateway billing",
        subscription: false,
    },
    Entry {
        id: ProviderId::Codex,
        slug: "codex",
        aliases: &[],
        name: "Codex",
        route_name: "Codex subscription",
        description: "ChatGPT Plus, Pro, Business, Enterprise, or Edu subscription",
        subscription: true,
    },
    Entry {
        id: ProviderId::Grok,
        slug: "grok",
        aliases: &[],
        name: "Grok",
        route_name: "Grok subscription",
        description: "SuperGrok or X Premium subscription",
        subscription: true,
    },
];

pub fn parse(value: &str) -> Option<ProviderId> {
    for entry in &entries {
        if value.eq_ignore_ascii_case(entry.slug) {
            return Some(entry.id);
        }
        for alias in entry.aliases {
            if value.eq_ignore_ascii_case(alias) {
                return Some(entry.id);
            }
        }
    }
    None
}

pub fn find(id: ProviderId) -> &'static Entry {
    for entry in &entries {
        if entry.id == id {
            return entry;
        }
    }
    unreachable!("auth provider catalog must contain every provider id");
}

pub fn label(id: ProviderId) -> &'static str {
    find(id).route_name
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auth_provider_catalog_uses_the_model_provider_identity_and_explicit_aliases() {
        assert_eq!(Some(ProviderId::Gateway), parse("vercel"));
        assert_eq!(Some(ProviderId::Gateway), parse("gateway"));
        assert_eq!(Some(ProviderId::Codex), parse("codex"));
        assert_eq!(Some(ProviderId::Grok), parse("grok"));
        assert_eq!(None, parse("openai-codex"));
        assert_eq!(None, parse("chatgpt"));
        assert_eq!(None, parse("unknown"));
        assert!(find(ProviderId::Codex).subscription);
        assert!(find(ProviderId::Grok).subscription);
    }
}
