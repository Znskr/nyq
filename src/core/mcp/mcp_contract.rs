//! MCP configuration and transport contracts.
//!
//! Ported 1:1 from `zig-reference/src/core/mcp/mcp_contract.zig`.

use crate::core::mcp::json_value::Value;

#[allow(non_upper_case_globals)]
pub const default_startup_timeout_ms: u32 = 30_000;
#[allow(non_upper_case_globals)]
pub const default_operation_timeout_ms: u32 = 60_000;
#[allow(non_upper_case_globals)]
pub const default_elicitation_timeout_ms: u32 = 30 * 60_000;
#[allow(non_upper_case_globals)]
pub const default_restart_limit: u8 = 1;

#[allow(non_upper_case_globals)]
pub const max_profile_config_warning_key_bytes: usize = 128;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProfileConfigWarningCause {
    IgnoredMcpServersAlias,
    SuspiciousServerKey,
    SuspiciousKeyScanIndeterminate,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProfileConfigWarning {
    pub cause: ProfileConfigWarningCause,
    key_bytes: [u8; max_profile_config_warning_key_bytes],
    key_len: u16,
    pub additional_matches: usize,
}

impl ProfileConfigWarning {
    pub fn init(
        cause: ProfileConfigWarningCause,
        key_value: Option<&[u8]>,
        additional_matches: usize,
    ) -> Self {
        let mut result = Self {
            cause,
            key_bytes: [0; max_profile_config_warning_key_bytes],
            key_len: 0,
            additional_matches,
        };
        if let Some(value) = key_value {
            let len = value.len().min(max_profile_config_warning_key_bytes);
            result.key_bytes[..len].copy_from_slice(&value[..len]);
            result.key_len = len as u16;
        }
        result
    }

    pub fn key(&self) -> Option<&[u8]> {
        if self.key_len == 0 {
            None
        } else {
            Some(&self.key_bytes[..self.key_len as usize])
        }
    }
}

#[derive(Clone, Debug)]
pub enum ProfileConfigDiagnostic {
    Clear,
    Warning(ProfileConfigWarning),
    Failed(String),
}

pub type InspectProfileConfigFn = fn() -> Result<ProfileConfigDiagnostic, ()>;

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct McpEnvVar {
    pub key: String,
    pub value: String,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct McpHttpHeader {
    pub name: String,
    pub value: String,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct McpHttpHeaderEnv {
    pub name: String,
    pub env: String,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct McpAuthConfig {
    pub resource: Option<String>,
    pub issuer: Option<String>,
    pub client_id: Option<String>,
    pub client_secret_env: Option<String>,
    pub client_metadata_url: Option<String>,
    pub scopes: Vec<String>,
}

impl McpAuthConfig {
    pub fn deinit(&mut self) {
        *self = Self::default();
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Progress {
    pub progress: f64,
    pub total: Option<f64>,
    pub message: Option<String>,
}

pub struct ProgressSink<'a> {
    pub callback: &'a mut dyn FnMut(Progress),
}

/// Receives a borrowed, validated JSON-RPC notification object.
pub struct NotificationSink<'a> {
    pub callback: &'a mut dyn FnMut(&Value),
}

/// Observes one borrowed, validated JSON-RPC response before the transport
/// publishes it to the requesting thread.
pub struct ResponseObserver<'a> {
    pub callback: &'a mut dyn FnMut(&Value) -> Result<(), Box<dyn std::error::Error + Send + Sync>>,
}

/// Receives one owned-lifetime JSON-RPC server request frame.
pub struct ServerRequestSink<'a> {
    pub prepare_callback: Option<&'a mut dyn FnMut(&[u8]) -> Result<(), Box<dyn std::error::Error + Send + Sync>>>,
    pub callback: &'a mut dyn FnMut(&[u8]) -> Result<(), Box<dyn std::error::Error + Send + Sync>>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PrecommitError;

/// Acquires the final authorization guard immediately before a transport
/// commits request bytes, then releases it as soon as commitment completes.
pub struct TransportPrecommit<'a> {
    pub acquire_callback: &'a mut dyn FnMut() -> Result<(), PrecommitError>,
    pub release_callback: &'a mut dyn FnMut(),
    pub acquired: bool,
}

impl TransportPrecommit<'_> {
    pub fn acquire(&mut self) -> Result<(), PrecommitError> {
        if self.acquired {
            return Ok(());
        }
        (self.acquire_callback)()?;
        self.acquired = true;
        Ok(())
    }

    pub fn release(&mut self) {
        if !self.acquired {
            return;
        }
        self.acquired = false;
        (self.release_callback)();
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum McpTransport {
    Stdio,
    Http,
    Sse,
}

impl McpTransport {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Stdio => "stdio",
            Self::Http => "http",
            Self::Sse => "sse",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConfigSource {
    Profile,
    Acp,
    Workspace,
}

impl ConfigSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Profile => "profile",
            Self::Acp => "acp",
            Self::Workspace => "workspace",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConfigScope {
    Profile,
    AcpSession,
    Workspace,
}

impl ConfigScope {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Profile => "profile",
            Self::AcpSession => "acp_session",
            Self::Workspace => "workspace",
        }
    }
}

pub fn source_allows_scope(source: ConfigSource, scope: ConfigScope) -> bool {
    match source {
        ConfigSource::Profile => scope == ConfigScope::Profile,
        ConfigSource::Acp => scope == ConfigScope::AcpSession,
        ConfigSource::Workspace => scope == ConfigScope::Workspace,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorkspaceAdmission {
    Pending,
    Approved,
    Rejected,
}

impl WorkspaceAdmission {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Approved => "approved",
            Self::Rejected => "rejected",
        }
    }
}

pub fn source_allows_workspace_admission(
    source: ConfigSource,
    admission: Option<WorkspaceAdmission>,
) -> bool {
    match source {
        ConfigSource::Workspace => admission.is_some(),
        ConfigSource::Profile | ConfigSource::Acp => admission.is_none(),
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EnvelopeError {
    McpInvalidJson,
}

pub fn validate_json_rpc_response_envelope(value: &Value) -> Result<(), EnvelopeError> {
    let object = value.as_object().ok_or(EnvelopeError::McpInvalidJson)?;
    let jsonrpc = object.get("jsonrpc").ok_or(EnvelopeError::McpInvalidJson)?;
    match jsonrpc.as_str() {
        Some("2.0") => {}
        _ => return Err(EnvelopeError::McpInvalidJson),
    }
    if object.contains("result") == object.contains("error") {
        return Err(EnvelopeError::McpInvalidJson);
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ServerConfigError {
    McpInvalidServerConfig,
}

/// One configured MCP server. Owns every field.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct McpServerConfig {
    pub name: String,
    pub source: ConfigSource,
    pub scope: ConfigScope,
    pub required: bool,
    pub transport: McpTransport,
    pub command: Option<String>,
    pub args: Vec<String>,
    pub url: Option<String>,
    pub env: Vec<McpEnvVar>,
    pub headers: Vec<McpHttpHeader>,
    pub header_env: Vec<McpHttpHeaderEnv>,
    pub bearer_token_env: Option<String>,
    pub auth: Option<McpAuthConfig>,
    pub allow_stored_credentials: bool,
    pub enabled: bool,
    pub workspace_admission: Option<WorkspaceAdmission>,
    pub startup_timeout_ms: u32,
    pub operation_timeout_ms: u32,
    pub restart_limit: u8,
}

impl McpServerConfig {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            source: ConfigSource::Profile,
            scope: ConfigScope::Profile,
            required: false,
            transport: McpTransport::Stdio,
            command: None,
            args: Vec::new(),
            url: None,
            env: Vec::new(),
            headers: Vec::new(),
            header_env: Vec::new(),
            bearer_token_env: None,
            auth: None,
            allow_stored_credentials: false,
            enabled: true,
            workspace_admission: None,
            startup_timeout_ms: default_startup_timeout_ms,
            operation_timeout_ms: default_operation_timeout_ms,
            restart_limit: default_restart_limit,
        }
    }

    pub fn stdio_command(&self) -> Result<&str, ServerConfigError> {
        if self.transport != McpTransport::Stdio {
            return Err(ServerConfigError::McpInvalidServerConfig);
        }
        let command = self
            .command
            .as_deref()
            .ok_or(ServerConfigError::McpInvalidServerConfig)?;
        if command.is_empty() {
            return Err(ServerConfigError::McpInvalidServerConfig);
        }
        Ok(command)
    }

    pub fn remote_url(&self) -> Result<&str, ServerConfigError> {
        if self.transport == McpTransport::Stdio {
            return Err(ServerConfigError::McpInvalidServerConfig);
        }
        let url = self
            .url
            .as_deref()
            .ok_or(ServerConfigError::McpInvalidServerConfig)?;
        if url.is_empty() {
            return Err(ServerConfigError::McpInvalidServerConfig);
        }
        Ok(url)
    }

    pub fn deinit(&mut self) {
        *self = Self::new(String::new());
    }
}

pub fn free_owned_strings(_values: Vec<String>) {}

pub fn free_http_header_env(_headers: Vec<McpHttpHeaderEnv>) {}

pub fn free_env_vars(_vars: Vec<McpEnvVar>) {}

pub fn free_http_headers(_headers: Vec<McpHttpHeader>) {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::mcp::json_value::parse_json;

    #[test]
    fn mcp_default_startup_timeout_allows_thirty_second_cold_starts() {
        assert_eq!(30_000, default_startup_timeout_ms);
    }

    #[test]
    fn transport_precommit_acquires_and_releases_exactly_once() {
        use std::cell::Cell;
        let acquisitions = Cell::new(0usize);
        let releases = Cell::new(0usize);
        let mut acquire = || {
            acquisitions.set(acquisitions.get() + 1);
            Ok(())
        };
        let mut release = || {
            releases.set(releases.get() + 1);
        };
        let mut precommit = TransportPrecommit {
            acquire_callback: &mut acquire,
            release_callback: &mut release,
            acquired: false,
        };
        precommit.acquire().unwrap();
        precommit.acquire().unwrap();
        precommit.release();
        precommit.release();
        assert_eq!(1, acquisitions.get());
        assert_eq!(1, releases.get());
    }

    #[test]
    fn mcp_server_configuration_exposes_only_the_active_transport_target() {
        let mut stdio = McpServerConfig::new("stdio");
        stdio.command = Some("node".into());
        assert_eq!("node", stdio.stdio_command().unwrap());
        assert_eq!(
            Err(ServerConfigError::McpInvalidServerConfig),
            stdio.remote_url()
        );

        let mut remote = McpServerConfig::new("remote");
        remote.transport = McpTransport::Http;
        remote.url = Some("https://example.test/mcp".into());
        assert_eq!(
            "https://example.test/mcp",
            remote.remote_url().unwrap()
        );
        assert_eq!(
            Err(ServerConfigError::McpInvalidServerConfig),
            remote.stdio_command()
        );

        let missing_stdio = McpServerConfig::new("missing");
        assert_eq!(
            Err(ServerConfigError::McpInvalidServerConfig),
            missing_stdio.stdio_command()
        );

        let mut empty_remote = McpServerConfig::new("empty");
        empty_remote.transport = McpTransport::Sse;
        empty_remote.url = Some(String::new());
        assert_eq!(
            Err(ServerConfigError::McpInvalidServerConfig),
            empty_remote.remote_url()
        );
    }

    #[test]
    fn profile_config_warning_owns_a_bounded_key_inline() {
        let warning = ProfileConfigWarning::init(
            ProfileConfigWarningCause::SuspiciousServerKey,
            Some(b"MCP-Servers"),
            2,
        );
        assert_eq!(b"MCP-Servers", warning.key().unwrap());
        assert_eq!(2, warning.additional_matches);
        assert!(ProfileConfigWarning::init(
            ProfileConfigWarningCause::SuspiciousKeyScanIndeterminate,
            None,
            0,
        )
        .key()
        .is_none());
    }

    #[test]
    fn mcp_server_configuration_deinit_owns_present_empty_targets() {
        let mut config = McpServerConfig::new("empty-owned-targets");
        config.command = Some(String::new());
        config.url = Some(String::new());
        config.deinit();
    }

    #[test]
    fn mcp_configuration_sources_admit_only_their_product_scope() {
        for source in [
            ConfigSource::Profile,
            ConfigSource::Acp,
            ConfigSource::Workspace,
        ] {
            for scope in [
                ConfigScope::Profile,
                ConfigScope::AcpSession,
                ConfigScope::Workspace,
            ] {
                let expected = match source {
                    ConfigSource::Profile => scope == ConfigScope::Profile,
                    ConfigSource::Acp => scope == ConfigScope::AcpSession,
                    ConfigSource::Workspace => scope == ConfigScope::Workspace,
                };
                assert_eq!(expected, source_allows_scope(source, scope));
            }
        }
    }

    #[test]
    fn workspace_admission_is_present_exactly_for_workspace_source() {
        for source in [
            ConfigSource::Profile,
            ConfigSource::Acp,
            ConfigSource::Workspace,
        ] {
            for admission in [
                None,
                Some(WorkspaceAdmission::Pending),
                Some(WorkspaceAdmission::Approved),
                Some(WorkspaceAdmission::Rejected),
            ] {
                let expected = if source == ConfigSource::Workspace {
                    admission.is_some()
                } else {
                    admission.is_none()
                };
                assert_eq!(
                    expected,
                    source_allows_workspace_admission(source, admission)
                );
            }
        }
    }

    #[test]
    fn json_rpc_response_envelopes_require_version_2_0_and_one_payload_member() {
        let cases: &[(&str, bool)] = &[
            (r#"{"jsonrpc":"2.0","id":1,"result":null}"#, true),
            (r#"{"jsonrpc":"2.0","id":1,"error":null}"#, true),
            (r#"{"id":1,"result":null}"#, false),
            (r#"{"jsonrpc":"1.0","id":1,"result":null}"#, false),
            (r#"{"jsonrpc":"2.0","id":1}"#, false),
            (
                r#"{"jsonrpc":"2.0","id":1,"result":null,"error":null}"#,
                false,
            ),
        ];
        for &(json, valid) in cases {
            let parsed = parse_json(json).unwrap();
            if valid {
                validate_json_rpc_response_envelope(&parsed).unwrap();
            } else {
                assert_eq!(
                    Err(EnvelopeError::McpInvalidJson),
                    validate_json_rpc_response_envelope(&parsed)
                );
            }
        }
    }
}
