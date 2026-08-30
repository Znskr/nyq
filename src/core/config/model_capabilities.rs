//! Model capability declarations and provider-option resolution.
//!
//! Mirrors `zig-reference/src/core/config/model_capabilities.zig`.

use crate::core::shared::types::ReasoningEffort;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ResolvedProviderOptions {
    pub reasoning: Option<ReasoningEffort>,
    pub fast: bool,
    pub parallel_tool_calls: Option<bool>,
    pub prompt_caching: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReasoningEffortOptions {
    pub values: [ReasoningEffort; ReasoningEffort::MAX_OPTIONS],
    pub len: usize,
}

impl Default for ReasoningEffortOptions {
    fn default() -> Self {
        Self {
            values: std::array::from_fn(|_| ReasoningEffort::Auto),
            len: 0,
        }
    }
}

impl ReasoningEffortOptions {
    pub fn from_slice(source: &[ReasoningEffort]) -> Self {
        let mut result = Self::default();
        result.len = source.len().min(result.values.len());
        for (index, value) in source.iter().take(result.len).enumerate() {
            result.values[index] = value.clone();
        }
        result
    }

    pub fn slice(&self) -> &[ReasoningEffort] {
        &self.values[..self.len]
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GatewayMetadata {
    pub supports_reasoning: bool,
    pub reasoning_efforts: ReasoningEffortOptions,
    pub supports_fast_mode: bool,
    pub supports_tool_use: bool,
    pub supports_vision: bool,
    pub supports_file_input: bool,
    pub supports_web_search: bool,
    pub supports_explicit_caching: bool,
    pub supports_implicit_caching: bool,
    pub context_window: Option<u32>,
    pub max_output_tokens: Option<u32>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Capabilities {
    pub supports_reasoning: bool,
    pub reasoning_efforts: ReasoningEffortOptions,
    pub supports_fast_mode: bool,
    pub supports_tool_use: bool,
    pub supports_vision: bool,
    pub supports_file_input: bool,
    pub supports_web_search: bool,
    pub supports_explicit_caching: bool,
    pub supports_implicit_caching: bool,
    pub prompt_caching: bool,
    pub parallel_tool_calls: Option<bool>,
    pub context_window: Option<u32>,
    pub max_output_tokens: Option<u32>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResolveError {
    Cancelled,
}

/// Capability resolver.
///
/// The Zig original stores a context pointer plus a function pointer taking an
/// arena allocator. Rust models the same value as a boxed closure so the
/// context stays captured without `unsafe`; allocations use the global
/// allocator.
pub struct Resolver {
    resolve_fn: Box<dyn Fn(&str) -> Result<Capabilities, ResolveError> + Send + Sync>,
}

impl Resolver {
    pub fn new(
        resolve_fn: Box<dyn Fn(&str) -> Result<Capabilities, ResolveError> + Send + Sync>,
    ) -> Self {
        Self { resolve_fn }
    }

    pub fn resolve(&self, model: &str) -> Result<Capabilities, ResolveError> {
        (self.resolve_fn)(model)
    }
}

pub fn merge_capabilities(
    capabilities_value: Capabilities,
    gateway_metadata: Option<GatewayMetadata>,
) -> Capabilities {
    let mut capabilities = capabilities_value;
    if let Some(metadata) = gateway_metadata {
        capabilities.supports_reasoning =
            metadata.supports_reasoning || metadata.reasoning_efforts.len > 0;
        capabilities.reasoning_efforts = metadata.reasoning_efforts;
        capabilities.supports_fast_mode = metadata.supports_fast_mode;
        capabilities.supports_tool_use = metadata.supports_tool_use;
        capabilities.supports_vision = metadata.supports_vision;
        capabilities.supports_file_input = metadata.supports_file_input;
        capabilities.supports_web_search = metadata.supports_web_search;
        capabilities.supports_explicit_caching = metadata.supports_explicit_caching;
        capabilities.supports_implicit_caching = metadata.supports_implicit_caching;
        if let Some(window) = metadata.context_window {
            capabilities.context_window = Some(window);
        }
        if let Some(tokens) = metadata.max_output_tokens {
            capabilities.max_output_tokens = Some(tokens);
        }
    }
    capabilities
}

pub fn resolve_capabilities(
    _model: &str,
    gateway_metadata: Option<GatewayMetadata>,
) -> Capabilities {
    merge_capabilities(Capabilities::default(), gateway_metadata)
}

pub fn capabilities_for_model(model: &str) -> Capabilities {
    resolve_capabilities(model, None)
}

pub trait ModelCapabilitiesResolver {
    fn resolved_model_capabilities(&self, model: &str) -> Capabilities;
}

pub fn resolve_for_app<App: ModelCapabilitiesResolver + ?Sized>(
    app: &App,
    model: &str,
) -> Capabilities {
    app.resolved_model_capabilities(model)
}

pub fn reasoning_effort_supported(capabilities: &Capabilities, effort: &ReasoningEffort) -> bool {
    if effort.is_default() {
        return true;
    }
    capabilities
        .reasoning_efforts
        .slice()
        .iter()
        .any(|option| option == effort)
}

pub fn reasoning_effort_index(capabilities: &Capabilities, effort: &ReasoningEffort) -> usize {
    if effort.is_default() {
        return 0;
    }
    for (index, option) in capabilities.reasoning_efforts.slice().iter().enumerate() {
        if option == effort {
            return index + 1;
        }
    }
    0
}

pub fn reasoning_effort_at_index(capabilities: &Capabilities, index: usize) -> ReasoningEffort {
    if index == 0 || capabilities.reasoning_efforts.len == 0 {
        return ReasoningEffort::Auto;
    }
    capabilities.reasoning_efforts.values
        [(index - 1) % capabilities.reasoning_efforts.len]
        .clone()
}

pub fn reasoning_effort_label_at_index(capabilities: &Capabilities, index: usize) -> &[u8] {
    if index == 0 || capabilities.reasoning_efforts.len == 0 {
        return b"default";
    }
    capabilities.reasoning_efforts.values
        [(index - 1) % capabilities.reasoning_efforts.len]
        .display_label()
}

pub fn reasoning_effort_option_count(capabilities: &Capabilities) -> usize {
    if capabilities.reasoning_efforts.len == 0 {
        0
    } else {
        capabilities.reasoning_efforts.len + 1
    }
}

pub fn resolve_provider_options_for_capabilities(
    capabilities: &Capabilities,
    effort: &ReasoningEffort,
    fast_mode: bool,
) -> ResolvedProviderOptions {
    let mut resolved = ResolvedProviderOptions {
        parallel_tool_calls: capabilities.parallel_tool_calls,
        prompt_caching: capabilities.prompt_caching,
        ..ResolvedProviderOptions::default()
    };
    if !effort.is_default() && reasoning_effort_supported(capabilities, effort) {
        resolved.reasoning = Some(effort.clone());
    }
    resolved.fast = fast_mode && capabilities.supports_fast_mode;
    resolved
}

#[cfg(test)]
mod tests {
    use super::*;

    fn effort(raw: &[u8]) -> ReasoningEffort {
        ReasoningEffort::literal(raw)
    }

    #[test]
    fn capabilities_never_infer_reasoning_or_fast_controls_from_model_ids() {
        let models = [
            "openai/gpt-5.6-sol",
            "anthropic/claude-opus-4.8",
            "zai/glm-5.2",
            "zai/glm-5.2-fast",
        ];
        for model in models {
            let capabilities = capabilities_for_model(model);
            assert_eq!(0, capabilities.reasoning_efforts.len);
            assert!(!capabilities.supports_fast_mode);
        }
    }

    #[test]
    fn merge_capabilities_preserves_provider_controls_and_supplied_fallback_policy() {
        let efforts = [effort(b"future-tier"), effort(b"high")];
        let capabilities = merge_capabilities(
            Capabilities {
                prompt_caching: true,
                ..Capabilities::default()
            },
            Some(GatewayMetadata {
                reasoning_efforts: ReasoningEffortOptions::from_slice(&efforts),
                supports_fast_mode: true,
                supports_tool_use: true,
                supports_vision: true,
                supports_file_input: true,
                supports_web_search: true,
                supports_explicit_caching: true,
                supports_implicit_caching: true,
                context_window: Some(300_000),
                max_output_tokens: Some(32_000),
                ..GatewayMetadata::default()
            }),
        );

        assert!(capabilities.supports_reasoning);
        assert_eq!(2, capabilities.reasoning_efforts.len);
        assert_eq!(
            b"future-tier".as_slice(),
            capabilities.reasoning_efforts.values[0].label()
        );
        assert!(capabilities.supports_fast_mode);
        assert!(capabilities.supports_tool_use);
        assert!(capabilities.supports_vision);
        assert!(capabilities.supports_file_input);
        assert!(capabilities.supports_web_search);
        assert!(capabilities.supports_explicit_caching);
        assert!(capabilities.supports_implicit_caching);
        assert!(capabilities.prompt_caching);
        assert_eq!(Some(300_000), capabilities.context_window);
        assert_eq!(Some(32_000), capabilities.max_output_tokens);
    }

    #[test]
    fn reasoning_effort_picker_helpers_prepend_default_and_preserve_gateway_order() {
        let efforts = [effort(b"future-tier"), effort(b"high")];
        let capabilities = Capabilities {
            reasoning_efforts: ReasoningEffortOptions::from_slice(&efforts),
            ..Capabilities::default()
        };

        assert_eq!(3, reasoning_effort_option_count(&capabilities));
        assert!(reasoning_effort_at_index(&capabilities, 0).is_default());
        assert_eq!(
            b"future-tier".as_slice(),
            reasoning_effort_label_at_index(&capabilities, 1)
        );
        assert_eq!(
            b"high".as_slice(),
            reasoning_effort_label_at_index(&capabilities, 2)
        );
        assert_eq!(
            1,
            reasoning_effort_index(&capabilities, &effort(b"future-tier"))
        );
        assert_eq!(0, reasoning_effort_index(&capabilities, &effort(b"stale")));
    }

    #[test]
    fn request_controls_form_a_total_fail_closed_state_machine() {
        let declared_efforts = [effort(b"future-tier")];
        let selected = effort(b"future-tier");
        let stale = effort(b"stale");

        struct Case {
            capabilities: Capabilities,
            effort: ReasoningEffort,
            fast_mode: bool,
            expected_effort: Option<&'static [u8]>,
            expected_fast: bool,
        }

        let declared = || Capabilities {
            reasoning_efforts: ReasoningEffortOptions::from_slice(&declared_efforts),
            ..Capabilities::default()
        };

        let cases = [
            Case {
                capabilities: Capabilities::default(),
                effort: ReasoningEffort::Auto,
                fast_mode: false,
                expected_effort: None,
                expected_fast: false,
            },
            Case {
                capabilities: Capabilities::default(),
                effort: selected.clone(),
                fast_mode: true,
                expected_effort: None,
                expected_fast: false,
            },
            Case {
                capabilities: declared(),
                effort: ReasoningEffort::Auto,
                fast_mode: false,
                expected_effort: None,
                expected_fast: false,
            },
            Case {
                capabilities: declared(),
                effort: selected.clone(),
                fast_mode: false,
                expected_effort: Some(b"future-tier"),
                expected_fast: false,
            },
            Case {
                capabilities: declared(),
                effort: stale.clone(),
                fast_mode: false,
                expected_effort: None,
                expected_fast: false,
            },
            Case {
                capabilities: Capabilities {
                    supports_fast_mode: true,
                    ..Capabilities::default()
                },
                effort: ReasoningEffort::Auto,
                fast_mode: true,
                expected_effort: None,
                expected_fast: true,
            },
            Case {
                capabilities: Capabilities {
                    reasoning_efforts: ReasoningEffortOptions::from_slice(&declared_efforts),
                    supports_fast_mode: true,
                    ..Capabilities::default()
                },
                effort: selected,
                fast_mode: true,
                expected_effort: Some(b"future-tier"),
                expected_fast: true,
            },
        ];

        for case in cases {
            let resolved =
                resolve_provider_options_for_capabilities(&case.capabilities, &case.effort, case.fast_mode);
            assert_eq!(case.expected_fast, resolved.fast);
            if let Some(expected) = case.expected_effort {
                assert_eq!(expected, resolved.reasoning.as_ref().unwrap().label());
            } else {
                assert_eq!(None, resolved.reasoning);
            }
        }
    }

    #[test]
    fn request_controls_remain_safe_across_repeated_state_transitions() {
        let declared_efforts = [effort(b"future-tier"), effort(b"high")];

        struct State {
            capabilities: Capabilities,
            effort: ReasoningEffort,
            fast_mode: bool,
        }

        let states = [
            State {
                capabilities: Capabilities::default(),
                effort: ReasoningEffort::Auto,
                fast_mode: false,
            },
            State {
                capabilities: Capabilities::default(),
                effort: effort(b"future-tier"),
                fast_mode: true,
            },
            State {
                capabilities: Capabilities {
                    reasoning_efforts: ReasoningEffortOptions::from_slice(&declared_efforts),
                    ..Capabilities::default()
                },
                effort: effort(b"future-tier"),
                fast_mode: true,
            },
            State {
                capabilities: Capabilities {
                    reasoning_efforts: ReasoningEffortOptions::from_slice(&declared_efforts),
                    supports_fast_mode: true,
                    ..Capabilities::default()
                },
                effort: effort(b"high"),
                fast_mode: true,
            },
            State {
                capabilities: Capabilities {
                    supports_fast_mode: true,
                    ..Capabilities::default()
                },
                effort: effort(b"stale"),
                fast_mode: false,
            },
        ];

        for transition in 0..1000usize {
            let state = &states[transition % states.len()];
            let resolved =
                resolve_provider_options_for_capabilities(&state.capabilities, &state.effort, state.fast_mode);
            if let Some(effort) = &resolved.reasoning {
                assert!(reasoning_effort_supported(&state.capabilities, effort));
                assert_eq!(&state.effort, effort);
            }
            assert!(!resolved.fast || (state.fast_mode && state.capabilities.supports_fast_mode));
        }
    }

    #[test]
    fn generic_fallback_capabilities_contain_no_vendor_policy() {
        let fallback = capabilities_for_model("anthropic/claude-any");
        assert!(!fallback.prompt_caching);
        assert_eq!(None, fallback.context_window);
    }
}
