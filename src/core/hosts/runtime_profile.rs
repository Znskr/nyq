//! Host runtime profile selection (ported 1:1 from
//! zig-reference/src/core/hosts/runtime_profile.zig).
//!
//! Zig selects a capability via `allows(App, capability)` where `App` may
//! declare a `host_profile` decl at comptime. Rust models the same decision as
//! `allows(host_profile: Option<Profile>, capability)` — `None` means "the app
//! declared no profile", which selects the native defaults.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Profile {
    pub cooperative_agent: bool,
    pub durable_sessions: bool,
    pub profile_usage: bool,
    pub native_auth: bool,
    pub file_index: bool,
    pub mcp: bool,
    pub subagents: bool,
    pub auto_upgrade: bool,
    pub skills: bool,
    pub clipboard: bool,
    pub url_opening: bool,
    pub web_search: bool,
    pub generation_usage: bool,
    pub tools: bool,
    pub js_host_config: bool,
    pub js_host_prompt_history: bool,
    pub js_host_sessions: bool,
    pub js_host_auth: bool,
    pub js_host_url_open: bool,
    pub js_host_workspace: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Capability {
    CooperativeAgent,
    DurableSessions,
    ProfileUsage,
    NativeAuth,
    FileIndex,
    Mcp,
    Subagents,
    AutoUpgrade,
    Skills,
    Clipboard,
    UrlOpening,
    WebSearch,
    GenerationUsage,
    Tools,
    JsHostConfig,
    JsHostPromptHistory,
    JsHostSessions,
    JsHostAuth,
    JsHostUrlOpen,
    JsHostWorkspace,
}

impl Profile {
    pub fn get(self, capability: Capability) -> bool {
        match capability {
            Capability::CooperativeAgent => self.cooperative_agent,
            Capability::DurableSessions => self.durable_sessions,
            Capability::ProfileUsage => self.profile_usage,
            Capability::NativeAuth => self.native_auth,
            Capability::FileIndex => self.file_index,
            Capability::Mcp => self.mcp,
            Capability::Subagents => self.subagents,
            Capability::AutoUpgrade => self.auto_upgrade,
            Capability::Skills => self.skills,
            Capability::Clipboard => self.clipboard,
            Capability::UrlOpening => self.url_opening,
            Capability::WebSearch => self.web_search,
            Capability::GenerationUsage => self.generation_usage,
            Capability::Tools => self.tools,
            Capability::JsHostConfig => self.js_host_config,
            Capability::JsHostPromptHistory => self.js_host_prompt_history,
            Capability::JsHostSessions => self.js_host_sessions,
            Capability::JsHostAuth => self.js_host_auth,
            Capability::JsHostUrlOpen => self.js_host_url_open,
            Capability::JsHostWorkspace => self.js_host_workspace,
        }
    }
}

/// `None` (app declares no `host_profile`) resolves to the native defaults,
/// exactly as the Zig `allows` falls back to `native` when `@hasDecl(App,
/// "host_profile")` is false.
pub fn allows(host_profile: Option<Profile>, capability: Capability) -> bool {
    host_profile.unwrap_or(native).get(capability)
}

#[allow(non_upper_case_globals)]
pub const native: Profile = Profile {
    cooperative_agent: false,
    durable_sessions: true,
    profile_usage: true,
    native_auth: true,
    file_index: true,
    mcp: true,
    subagents: true,
    auto_upgrade: true,
    skills: true,
    clipboard: true,
    url_opening: true,
    web_search: true,
    generation_usage: true,
    tools: true,
    js_host_config: false,
    js_host_prompt_history: false,
    js_host_sessions: false,
    js_host_auth: false,
    js_host_url_open: false,
    js_host_workspace: false,
};

#[allow(non_upper_case_globals)]
pub const wasm: Profile = Profile {
    cooperative_agent: true,
    durable_sessions: false,
    profile_usage: false,
    native_auth: false,
    file_index: false,
    mcp: false,
    subagents: false,
    auto_upgrade: false,
    skills: false,
    clipboard: false,
    url_opening: false,
    web_search: false,
    generation_usage: false,
    tools: false,
    js_host_config: true,
    js_host_prompt_history: true,
    js_host_sessions: true,
    js_host_auth: true,
    js_host_url_open: true,
    js_host_workspace: true,
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apps_without_a_host_profile_retain_native_capabilities() {
        assert!(allows(None, Capability::DurableSessions));
        assert!(!allows(None, Capability::JsHostSessions));
    }

    #[test]
    fn host_profile_capabilities_are_selected_by_field() {
        assert!(!allows(Some(wasm), Capability::DurableSessions));
        assert!(allows(Some(wasm), Capability::JsHostConfig));
        assert!(allows(Some(wasm), Capability::JsHostSessions));
    }

    #[test]
    fn native_and_wasm_profiles_select_distinct_auth_host_effects() {
        assert!(native.native_auth);
        assert!(native.url_opening);
        assert!(!native.js_host_auth);
        assert!(!native.js_host_url_open);

        assert!(!wasm.native_auth);
        assert!(!wasm.url_opening);
        assert!(wasm.js_host_auth);
        assert!(wasm.js_host_url_open);
    }

    #[test]
    fn workspace_host_capability_is_wasm_only_without_enabling_native_tools() {
        assert!(!native.js_host_workspace);
        assert!(wasm.js_host_workspace);
        assert!(!wasm.tools);
    }
}
