//! MCP health snapshots, startup decisions, and terminal rendering.
//!
//! Ported 1:1 from `zig-reference/src/core/mcp/health.zig`.

use super::mcp_contract::{
    ConfigScope, ConfigSource, McpTransport, ProfileConfigDiagnostic, WorkspaceAdmission,
};
use crate::core::shared::text_utils;

const MAX_PENDING_SUMMARY_NAMES: usize = 4;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConnectionState {
    Disconnected,
    Disabled,
    Connecting,
    Ready,
    Failed,
}

impl ConnectionState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Disconnected => "disconnected",
            Self::Disabled => "disabled",
            Self::Connecting => "connecting",
            Self::Ready => "ready",
            Self::Failed => "failed",
        }
    }
}

pub fn observed_connection(
    published: ConnectionState,
    transport_running: Option<bool>,
) -> ConnectionState {
    if published == ConnectionState::Ready && transport_running == Some(false) {
        return ConnectionState::Failed;
    }
    published
}

pub fn retry_delay(retry_at_ms: Option<u64>, captured_at_ms: u64) -> Option<u64> {
    let retry_at = retry_at_ms?;
    Some(retry_at.saturating_sub(captured_at_ms))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AuthenticationState {
    None,
    Configured,
    Authenticated,
    Required,
}

impl AuthenticationState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Configured => "configured",
            Self::Authenticated => "authenticated",
            Self::Required => "required",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CacheFreshness {
    Unavailable,
    Fresh,
    Stale,
    Refreshing,
    FailedRefresh,
}

impl CacheFreshness {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Unavailable => "unavailable",
            Self::Fresh => "fresh",
            Self::Stale => "stale",
            Self::Refreshing => "refreshing",
            Self::FailedRefresh => "failed_refresh",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SubscriptionState {
    Unavailable,
    Starting,
    Active,
    Unsupported,
    Stopped,
}

impl SubscriptionState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Unavailable => "unavailable",
            Self::Starting => "starting",
            Self::Active => "active",
            Self::Unsupported => "unsupported",
            Self::Stopped => "stopped",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CapabilityCounts {
    pub tools: Option<usize>,
    pub resources: Option<usize>,
    pub resource_templates: Option<usize>,
    pub prompts: Option<usize>,
}

pub fn capability_count(advertised: bool, loaded: bool, count: usize) -> Option<usize> {
    if !advertised {
        return Some(0);
    }
    if loaded {
        Some(count)
    } else {
        None
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SubscriptionObservation {
    pub present: bool,
    pub unsupported: bool,
    pub finished: bool,
    pub requires_acknowledgement: bool,
    pub acknowledged: bool,
}

pub fn subscription_state_for(observation: SubscriptionObservation) -> SubscriptionState {
    if !observation.present {
        return SubscriptionState::Unavailable;
    }
    if observation.unsupported {
        return SubscriptionState::Unsupported;
    }
    if observation.finished {
        return SubscriptionState::Stopped;
    }
    if !observation.requires_acknowledgement || observation.acknowledged {
        return SubscriptionState::Active;
    }
    SubscriptionState::Starting
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ServerSnapshot {
    pub configured_name: String,
    pub negotiated_name: Option<String>,
    pub negotiated_version: Option<String>,
    pub source: ConfigSource,
    pub scope: ConfigScope,
    pub workspace_admission: Option<WorkspaceAdmission>,
    pub required: bool,
    pub transport: McpTransport,
    pub protocol_version: Option<String>,
    pub connection: ConnectionState,
    pub authentication: AuthenticationState,
    pub counts: CapabilityCounts,
    pub cache_freshness: CacheFreshness,
    pub subscription: SubscriptionState,
    pub runtime_generation: u64,
    pub catalog_generation: u64,
    pub retry_attempt: u8,
    pub retry_in_ms: Option<u64>,
    pub last_successful_discovery_ms: Option<u64>,
    pub failure: Option<String>,
}

impl ServerSnapshot {
    pub fn deinit(&mut self) {
        *self = empty_server_snapshot();
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConfigurationIssue {
    pub message: String,
}

impl ConfigurationIssue {
    pub fn deinit(&mut self) {
        self.message.clear();
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConfiguredServerSnapshot {
    pub configured_name: String,
    pub source: ConfigSource,
    pub scope: ConfigScope,
    pub workspace_admission: Option<WorkspaceAdmission>,
    pub required: bool,
    pub transport: McpTransport,
}

impl ConfiguredServerSnapshot {
    pub fn deinit(&mut self) {
        self.configured_name.clear();
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct LocalConfigSnapshot {
    pub servers: Vec<ConfiguredServerSnapshot>,
    pub configuration_issues: Vec<ConfigurationIssue>,
}

impl LocalConfigSnapshot {
    pub fn deinit(&mut self) {
        self.servers.clear();
        self.configuration_issues.clear();
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Snapshot {
    pub captured_at_ms: u64,
    pub servers: Vec<ServerSnapshot>,
    pub configuration_issues: Vec<ConfigurationIssue>,
}

impl Snapshot {
    pub fn deinit(&mut self) {
        self.servers.clear();
        self.configuration_issues.clear();
    }
}

pub struct LocalConfigInspection {
    pub profile_diagnostic: ProfileConfigDiagnostic,
    pub snapshot: LocalConfigSnapshot,
    pub inspection_error: Option<String>,
}

impl LocalConfigInspection {
    pub fn deinit(&mut self) {
        self.snapshot.deinit();
        self.inspection_error = None;
    }
}

pub type InspectLocalConfigFn = fn(&[u8]) -> Result<LocalConfigInspection, ()>;

pub fn inspect_local_config_unavailable(_workspace: &[u8]) -> Result<LocalConfigInspection, ()> {
    Ok(LocalConfigInspection {
        profile_diagnostic: ProfileConfigDiagnostic::Clear,
        snapshot: LocalConfigSnapshot {
            servers: Vec::new(),
            configuration_issues: Vec::new(),
        },
        inspection_error: None,
    })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StartupDecision {
    Ready,
    Degraded,
    Blocked,
}

pub fn startup_decision(servers: &[ServerSnapshot]) -> StartupDecision {
    let mut degraded = false;
    for server in servers {
        if server.connection == ConnectionState::Ready
            || (server.connection == ConnectionState::Disabled && !server.required)
        {
            continue;
        }
        if server.required {
            return StartupDecision::Blocked;
        }
        degraded = true;
    }
    if degraded {
        StartupDecision::Degraded
    } else {
        StartupDecision::Ready
    }
}

fn publish_candidate(servers: &[ServerSnapshot]) -> bool {
    startup_decision(servers) != StartupDecision::Blocked
}

pub fn publish_candidate_for_decision(decision: StartupDecision) -> bool {
    decision != StartupDecision::Blocked
}

fn write_optional_count(out: &mut String, count: Option<usize>) {
    match count {
        Some(value) => out.push_str(&value.to_string()),
        None => out.push_str("unknown"),
    }
}

pub fn render(snapshot: &Snapshot) -> String {
    if snapshot.servers.is_empty() && snapshot.configuration_issues.is_empty() {
        return "No MCP servers configured.\n".into();
    }
    let mut out = String::new();
    if !snapshot.servers.is_empty() {
        let noun = if snapshot.servers.len() == 1 {
            "server"
        } else {
            "servers"
        };
        out.push_str(&format!(
            "MCP health ({} {}):\n",
            snapshot.servers.len(),
            noun
        ));
    }
    for server in &snapshot.servers {
        let policy = if server.required {
            "required"
        } else {
            "optional"
        };
        out.push_str(&format!(
            "  {} source={} scope={} policy={} transport={} state={} auth={}\n",
            server.configured_name,
            server.source.as_str(),
            server.scope.as_str(),
            policy,
            server.transport.as_str(),
            server.connection.as_str(),
            server.authentication.as_str(),
        ));
        if let Some(admission) = server.workspace_admission {
            out.push_str(&format!("    admission={}\n", admission.as_str()));
        }
        out.push_str(&format!(
            "    negotiated_name={} negotiated_version={} protocol={}\n",
            server.negotiated_name.as_deref().unwrap_or("unavailable"),
            server
                .negotiated_version
                .as_deref()
                .unwrap_or("unavailable"),
            server.protocol_version.as_deref().unwrap_or("unavailable"),
        ));
        out.push_str("    tools=");
        write_optional_count(&mut out, server.counts.tools);
        out.push_str(" resources=");
        write_optional_count(&mut out, server.counts.resources);
        out.push_str(" templates=");
        write_optional_count(&mut out, server.counts.resource_templates);
        out.push_str(" prompts=");
        write_optional_count(&mut out, server.counts.prompts);
        out.push_str(&format!(
            " cache={} subscription={}\n",
            server.cache_freshness.as_str(),
            server.subscription.as_str(),
        ));
        out.push_str(&format!("    retry_attempt={} retry_in_ms=", server.retry_attempt));
        match server.retry_in_ms {
            Some(value) => out.push_str(&value.to_string()),
            None => out.push_str("none"),
        }
        out.push_str(&format!(
            " discovery={}",
            if server.last_successful_discovery_ms.is_none() {
                "pending"
            } else {
                "completed"
            }
        ));
        out.push('\n');
        if let Some(failure) = &server.failure {
            out.push_str(&format!("    failure={failure}\n"));
        }
    }
    if !snapshot.configuration_issues.is_empty() {
        out.push_str("Project MCP configuration errors:\n");
        for issue in &snapshot.configuration_issues {
            out.push_str(&format!("  {}\n", issue.message));
        }
    }
    out
}

pub fn render_summary(snapshot: &Snapshot) -> String {
    if snapshot.servers.is_empty() && snapshot.configuration_issues.is_empty() {
        return "MCP: no servers configured. Use /mcp add <name> <command> [args...].".into();
    }
    if snapshot.servers.is_empty() {
        let noun = if snapshot.configuration_issues.len() == 1 {
            "error"
        } else {
            "errors"
        };
        return format!(
            "MCP: {} project .mcp.json {}. Use /mcp list for details.",
            snapshot.configuration_issues.len(),
            noun
        );
    }
    let mut ready = 0usize;
    let mut connecting = 0usize;
    let mut auth_required = 0usize;
    let mut failed = 0usize;
    let mut first_auth_server: Option<&str> = None;
    let mut pending_names: [&str; MAX_PENDING_SUMMARY_NAMES] = [""; MAX_PENDING_SUMMARY_NAMES];
    let mut pending_count = 0usize;
    for server in &snapshot.servers {
        if server.source == ConfigSource::Workspace
            && server.workspace_admission == Some(WorkspaceAdmission::Pending)
        {
            if pending_count < pending_names.len() {
                pending_names[pending_count] = &server.configured_name;
            }
            pending_count += 1;
        }
        if server.authentication == AuthenticationState::Required {
            auth_required += 1;
            if first_auth_server.is_none() {
                first_auth_server = Some(&server.configured_name);
            }
            continue;
        }
        match server.connection {
            ConnectionState::Ready => ready += 1,
            ConnectionState::Connecting => connecting += 1,
            ConnectionState::Failed => failed += 1,
            ConnectionState::Disconnected | ConnectionState::Disabled => {}
        }
    }

    let noun = if snapshot.servers.len() == 1 {
        "server"
    } else {
        "servers"
    };
    let mut out = format!(
        "MCP: {} {} — {} ready, {} connecting, {} needs auth, {} failed.",
        snapshot.servers.len(),
        noun,
        ready,
        connecting,
        auth_required,
        failed,
    );
    if let Some(name) = first_auth_server {
        out.push_str(&format!(" Run /mcp auth {name} --open."));
    }
    if pending_count > 0 {
        out.push_str(" Pending approval: ");
        let rendered_count = pending_count.min(pending_names.len());
        for (index, name) in pending_names[..rendered_count].iter().enumerate() {
            if index > 0 {
                out.push_str(", ");
            }
            let encoded = text_utils::encode_terminal_safe(name.as_bytes(), 128);
            out.push_str(&String::from_utf8_lossy(&encoded.bytes));
        }
        if pending_count > rendered_count {
            out.push_str(&format!(", +{} more", pending_count - rendered_count));
        }
        out.push('.');
    }
    if !snapshot.configuration_issues.is_empty() {
        out.push_str(&format!(
            " Project .mcp.json errors: {}.",
            snapshot.configuration_issues.len()
        ));
    }
    out.push_str(" Use /mcp list for details.");
    out
}

fn empty_server_snapshot() -> ServerSnapshot {
    ServerSnapshot {
        configured_name: String::new(),
        negotiated_name: None,
        negotiated_version: None,
        source: ConfigSource::Profile,
        scope: ConfigScope::Profile,
        workspace_admission: None,
        required: false,
        transport: McpTransport::Stdio,
        protocol_version: None,
        connection: ConnectionState::Disconnected,
        authentication: AuthenticationState::None,
        counts: CapabilityCounts::default(),
        cache_freshness: CacheFreshness::Unavailable,
        subscription: SubscriptionState::Unavailable,
        runtime_generation: 0,
        catalog_generation: 0,
        retry_attempt: 0,
        retry_in_ms: None,
        last_successful_discovery_ms: None,
        failure: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn required_health_blocks_while_optional_health_degrades() {
        let mut snapshots = [empty_server_snapshot()];
        snapshots[0].connection = ConnectionState::Failed;
        assert_eq!(StartupDecision::Degraded, startup_decision(&snapshots));
        snapshots[0].required = true;
        assert_eq!(StartupDecision::Blocked, startup_decision(&snapshots));
        assert!(!publish_candidate(&snapshots));
    }

    #[test]
    fn health_decisions_distinguish_lazy_capabilities_and_listener_acknowledgement() {
        assert_eq!(None, capability_count(true, false, 0));
        assert_eq!(Some(0), capability_count(false, false, 0));
        assert_eq!(Some(7), capability_count(true, true, 7));

        let cases = [
            (
                SubscriptionObservation {
                    present: false,
                    ..SubscriptionObservation::default()
                },
                SubscriptionState::Unavailable,
            ),
            (
                SubscriptionObservation {
                    present: true,
                    unsupported: true,
                    ..SubscriptionObservation::default()
                },
                SubscriptionState::Unsupported,
            ),
            (
                SubscriptionObservation {
                    present: true,
                    finished: true,
                    ..SubscriptionObservation::default()
                },
                SubscriptionState::Stopped,
            ),
            (
                SubscriptionObservation {
                    present: true,
                    ..SubscriptionObservation::default()
                },
                SubscriptionState::Active,
            ),
            (
                SubscriptionObservation {
                    present: true,
                    requires_acknowledgement: true,
                    ..SubscriptionObservation::default()
                },
                SubscriptionState::Starting,
            ),
            (
                SubscriptionObservation {
                    present: true,
                    requires_acknowledgement: true,
                    acknowledged: true,
                    ..SubscriptionObservation::default()
                },
                SubscriptionState::Active,
            ),
        ];
        for (observation, expected) in cases {
            assert_eq!(expected, subscription_state_for(observation));
        }
    }

    #[test]
    fn health_derives_failed_connections_from_transport_state() {
        assert_eq!(
            ConnectionState::Failed,
            observed_connection(ConnectionState::Ready, Some(false)),
        );
        assert_eq!(
            ConnectionState::Ready,
            observed_connection(ConnectionState::Ready, Some(true)),
        );
        assert_eq!(
            ConnectionState::Ready,
            observed_connection(ConnectionState::Ready, None),
        );
        assert_eq!(Some(250), retry_delay(Some(1_250), 1_000));
        assert_eq!(Some(0), retry_delay(Some(900), 1_000));
        assert_eq!(None, retry_delay(None, 1_000));
    }

    #[test]
    fn health_rendering_includes_complete_typed_state_without_secret_bearing_configuration() {
        let mut server = empty_server_snapshot();
        server.configured_name = "server\\u001b[31m".into();
        server.negotiated_name = Some("fixture".into());
        server.negotiated_version = Some("1.2.3".into());
        server.protocol_version = Some("2026-07-28".into());
        server.source = ConfigSource::Profile;
        server.scope = ConfigScope::Profile;
        server.required = true;
        server.transport = McpTransport::Http;
        server.connection = ConnectionState::Failed;
        server.authentication = AuthenticationState::Required;
        server.counts = CapabilityCounts {
            tools: Some(2),
            resources: Some(3),
            resource_templates: Some(4),
            prompts: Some(5),
        };
        server.cache_freshness = CacheFreshness::FailedRefresh;
        server.subscription = SubscriptionState::Stopped;
        server.runtime_generation = 7;
        server.catalog_generation = 9;
        server.retry_attempt = 2;
        server.retry_in_ms = Some(123);
        server.last_successful_discovery_ms = Some(100);
        server.failure = Some(
            "Authentication is required; run /mcp auth <name> --open.".into(),
        );
        let snapshot = Snapshot {
            captured_at_ms: 200,
            servers: vec![server],
            configuration_issues: Vec::new(),
        };
        let output = render(&snapshot);
        for expected in [
            "source=profile",
            "scope=profile",
            "policy=required",
            "transport=http",
            "state=failed",
            "auth=required",
            "tools=2 resources=3 templates=4 prompts=5",
            "MCP health (1 server)",
            "retry_attempt=2 retry_in_ms=123 discovery=completed",
        ] {
            assert!(
                output.contains(expected),
                "missing {expected:?} in {output:?}"
            );
        }
        assert!(!output.contains("captured_at_ms"));
        assert!(!output.contains("runtime_generation"));
        assert!(!output.contains("catalog_generation"));
        assert!(!output.contains("Authorization"));
        assert!(!output.contains("https://"));
    }

    #[test]
    fn compact_health_summary_reports_actionable_aggregate_state() {
        let empty = Snapshot {
            captured_at_ms: 0,
            servers: Vec::new(),
            configuration_issues: Vec::new(),
        };
        assert_eq!(
            "MCP: no servers configured. Use /mcp add <name> <command> [args...].",
            render_summary(&empty)
        );

        let mut servers = [
            empty_server_snapshot(),
            empty_server_snapshot(),
            empty_server_snapshot(),
            empty_server_snapshot(),
        ];
        servers[0].configured_name = "ready".into();
        servers[0].connection = ConnectionState::Ready;
        servers[1].configured_name = "loading".into();
        servers[1].connection = ConnectionState::Connecting;
        servers[2].configured_name = "plain".into();
        servers[2].connection = ConnectionState::Failed;
        servers[2].authentication = AuthenticationState::Required;
        servers[3].configured_name = "broken".into();
        servers[3].connection = ConnectionState::Failed;
        let summary = render_summary(&Snapshot {
            captured_at_ms: 0,
            servers: servers.to_vec(),
            configuration_issues: Vec::new(),
        });
        assert_eq!(
            "MCP: 4 servers — 1 ready, 1 connecting, 1 needs auth, 1 failed. Run /mcp auth plain --open. Use /mcp list for details.",
            summary
        );
    }

    #[test]
    fn compact_health_summary_names_pending_workspace_servers() {
        let mut servers = [
            empty_server_snapshot(),
            empty_server_snapshot(),
            empty_server_snapshot(),
        ];
        servers[0].configured_name = "docs".into();
        servers[0].source = ConfigSource::Workspace;
        servers[0].scope = ConfigScope::Workspace;
        servers[0].workspace_admission = Some(WorkspaceAdmission::Pending);
        servers[1].configured_name = "db".into();
        servers[1].source = ConfigSource::Workspace;
        servers[1].scope = ConfigScope::Workspace;
        servers[1].workspace_admission = Some(WorkspaceAdmission::Pending);
        servers[2].configured_name = "approved".into();
        servers[2].source = ConfigSource::Workspace;
        servers[2].scope = ConfigScope::Workspace;
        servers[2].workspace_admission = Some(WorkspaceAdmission::Approved);

        let summary = render_summary(&Snapshot {
            captured_at_ms: 0,
            servers: servers.to_vec(),
            configuration_issues: Vec::new(),
        });
        assert!(summary.contains("Pending approval: docs, db."));
        assert!(!summary.contains("Pending approval: approved"));
    }
}
