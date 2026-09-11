//! Shell command admission fingerprints and execution authority shapes.
//!
//! Ported from `zig-reference/src/core/permissions/command_admission.zig`.

use crate::core::execution::command_environment::Environment;
use crate::core::shell_command::command_effect::{self, ApprovalReason, TargetOs};
use crate::core::shared::types::PermissionMode;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommandContext<'a> {
    pub command: &'a [u8],
    pub resolved_cwd: &'a [u8],
    pub background: bool,
    pub target_os: TargetOs,
    pub environment: Environment<'a>,
}

impl<'a> CommandContext<'a> {
    pub fn new(
        command: &'a [u8],
        resolved_cwd: &'a [u8],
        background: bool,
        target_os: TargetOs,
    ) -> Self {
        Self {
            command,
            resolved_cwd,
            background,
            target_os,
            environment: Environment::Legacy,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AdmissionFingerprint {
    pub command: Vec<u8>,
    pub resolved_cwd: Vec<u8>,
    pub background: bool,
    pub target_os: TargetOs,
    pub environment: OwnedEnvironment,
}

/// Owned form of `command_environment::Environment` so a fingerprint can be
/// stored without borrowing the admission context.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OwnedEnvironment {
    Legacy,
    Clean(Vec<u8>),
    User(Vec<u8>),
    WorkspaceClean,
}

impl OwnedEnvironment {
    pub fn from_borrowed(env: Environment<'_>) -> Self {
        match env {
            Environment::Legacy => Self::Legacy,
            Environment::Clean(path) => Self::Clean(path.to_vec()),
            Environment::User(path) => Self::User(path.to_vec()),
            Environment::WorkspaceClean => Self::WorkspaceClean,
        }
    }

    pub fn borrowed(&self) -> Environment<'_> {
        match self {
            Self::Legacy => Environment::Legacy,
            Self::Clean(path) => Environment::Clean(path),
            Self::User(path) => Environment::User(path),
            Self::WorkspaceClean => Environment::WorkspaceClean,
        }
    }

    pub fn eql(&self, other: Environment<'_>) -> bool {
        self.borrowed().eql(other)
    }
}

impl AdmissionFingerprint {
    pub fn init(ctx: &CommandContext<'_>) -> Self {
        Self {
            command: ctx.command.to_vec(),
            resolved_cwd: ctx.resolved_cwd.to_vec(),
            background: ctx.background,
            target_os: ctx.target_os,
            environment: OwnedEnvironment::from_borrowed(ctx.environment),
        }
    }

    pub fn matches(&self, ctx: &CommandContext<'_>) -> bool {
        self.command == ctx.command
            && self.resolved_cwd == ctx.resolved_cwd
            && self.background == ctx.background
            && self.target_os == ctx.target_os
            && self.environment.eql(ctx.environment)
    }

    pub fn eql(&self, other: &AdmissionFingerprint) -> bool {
        self == other
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShellAuthorizationSource {
    ConfiguredRule,
    SessionState,
    SessionGrant,
    InteractiveOnce,
    InteractiveAlways,
    AutoMode,
    AutoClassifier,
    Yolo,
    JsHost,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CommandExecutionAuthority {
    DirectOnly(AdmissionFingerprint),
    ShellAllowed {
        fingerprint: AdmissionFingerprint,
        source: ShellAuthorizationSource,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VisionPathExecutionTarget {
    pub canonical_path: Vec<u8>,
    pub identity: crate::core::tooling::file_mutation_contract::FileIdentity,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VisionPathExecutionAuthority {
    /// The admission layer owns this list and every canonical path.
    pub targets: Vec<VisionPathExecutionTarget>,
}

pub enum ToolExecutionAuthority {
    Ordinary,
    RunCommand(CommandExecutionAuthority),
    /// Kept opaque because the authorization carries non-comparable prepared
    /// mutation payloads; matches! on the variant is the supported surface.
    FileMutation(std::sync::Arc<crate::core::tooling::file_mutation_contract::FileExecutionAuthorization>),
    VisionPaths(VisionPathExecutionAuthority),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HumanApprovalProvenance {
    None,
    Once,
    Always,
}

/// The classifier result stays classifier-owned; transports never clone the
/// review payload, so the Rust port keeps it out of the outcome value.
pub struct PermissionOutcome {
    pub decision: crate::core::shared::types::ToolPermissionDecision,
    pub execution_authority: Option<ToolExecutionAuthority>,
    pub human_approval: HumanApprovalProvenance,
    pub denial_reason: Option<crate::core::shared::types::ToolPermissionDenialReason>,
    pub requirement: Option<PermissionRequirement>,
    pub tool_failure: Option<Vec<u8>>,
    pub feedback: Option<Vec<u8>>,
}

impl Default for PermissionOutcome {
    fn default() -> Self {
        Self {
            decision: crate::core::shared::types::ToolPermissionDecision::PolicyDenied,
            execution_authority: None,
            human_approval: HumanApprovalProvenance::None,
            denial_reason: None,
            requirement: None,
            tool_failure: None,
            feedback: None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PermissionRequirement {
    ConfiguredRule,
    ApprovalRequired,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DefaultApproval {
    DirectOnly(AdmissionFingerprint),
    ApprovalRequired(ApprovalReason),
}

pub fn default_for_run_command(
    command_ctx: &CommandContext<'_>,
    permission_mode: PermissionMode,
) -> DefaultApproval {
    let requires_shell_authority = match command_ctx.environment {
        Environment::User(_) => true,
        Environment::Clean(_) => permission_mode != PermissionMode::Auto,
        Environment::Legacy | Environment::WorkspaceClean => false,
    };
    if requires_shell_authority {
        return DefaultApproval::ApprovalRequired(ApprovalReason::DynamicShell);
    }
    let admission = command_effect::plan(
        command_ctx.command,
        command_ctx.resolved_cwd,
        command_ctx.background,
        command_ctx.target_os,
    );
    match admission {
        command_effect::Admission::DirectReadOnly(_) => {
            DefaultApproval::DirectOnly(AdmissionFingerprint::init(&command_ctx))
        }
        command_effect::Admission::ApprovalRequired(reason) => {
            DefaultApproval::ApprovalRequired(reason)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalized_default_emits_direct_only_only_for_a_direct_plan() {
        let direct_ctx = CommandContext {
            command: b"pwd",
            resolved_cwd: b"/workspace",
            background: false,
            target_os: TargetOs::Macos,
            environment: Environment::Legacy,
        };
        let direct = default_for_run_command(&direct_ctx, PermissionMode::Ask);
        match direct {
            DefaultApproval::DirectOnly(fingerprint) => {
                assert!(fingerprint.matches(&direct_ctx))
            }
            _ => panic!("expected direct only"),
        }

        let write_ctx = CommandContext {
            command: b"touch created.txt",
            resolved_cwd: b"/workspace",
            background: false,
            target_os: TargetOs::Macos,
            environment: Environment::Legacy,
        };
        assert_eq!(
            default_for_run_command(&write_ctx, PermissionMode::Ask),
            DefaultApproval::ApprovalRequired(ApprovalReason::FilesystemWrite)
        );
    }

    #[test]
    fn explicit_user_environment_always_requires_shell_authority() {
        let user_ctx = CommandContext {
            command: b"pwd",
            resolved_cwd: b"/workspace",
            background: false,
            target_os: TargetOs::Macos,
            environment: Environment::User(b"/bin/zsh"),
        };
        for permission_mode in [PermissionMode::Auto, PermissionMode::Ask] {
            assert_eq!(
                default_for_run_command(&user_ctx, permission_mode),
                DefaultApproval::ApprovalRequired(ApprovalReason::DynamicShell)
            );
        }
    }

    #[test]
    fn explicit_clean_environment_is_direct_only_in_automatic_mode() {
        let clean_ctx = CommandContext {
            command: b"pwd",
            resolved_cwd: b"/workspace",
            background: false,
            target_os: TargetOs::Macos,
            environment: Environment::Clean(b"/bin/zsh"),
        };
        let automatic = default_for_run_command(&clean_ctx, PermissionMode::Auto);
        match automatic {
            DefaultApproval::DirectOnly(fingerprint) => {
                assert!(fingerprint.matches(&clean_ctx))
            }
            _ => panic!("expected direct only"),
        }
        assert_eq!(
            default_for_run_command(&clean_ctx, PermissionMode::Ask),
            DefaultApproval::ApprovalRequired(ApprovalReason::DynamicShell)
        );
        let write_ctx = CommandContext {
            command: b"touch created.txt",
            ..clean_ctx
        };
        assert_eq!(
            default_for_run_command(&write_ctx, PermissionMode::Auto),
            DefaultApproval::ApprovalRequired(ApprovalReason::FilesystemWrite)
        );
    }

    #[test]
    fn fingerprint_eql_compares_owned_fields() {
        let ctx = CommandContext {
            command: b"ls",
            resolved_cwd: b"/w",
            background: false,
            target_os: TargetOs::Linux,
            environment: Environment::Clean(b"/bin/sh"),
        };
        let a = AdmissionFingerprint::init(&ctx);
        let b = AdmissionFingerprint::init(&ctx);
        assert!(a.eql(&b));
        let mut c = AdmissionFingerprint::init(&ctx);
        c.background = true;
        assert!(!a.eql(&c));
        assert!(!a.matches(&CommandContext {
            background: true,
            ..ctx
        }));
    }
}
