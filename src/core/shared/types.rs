use super::text_utils;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Layout {
    pub rows: u16,
    pub cols: u16,
    pub content_bottom: u16,
    pub divider_top_row: u16,
    pub input_row: u16,
    pub divider_bottom_row: u16,
    pub hint_row: u16,
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Metrics {
    pub ansi_bytes: usize,
    pub full_redraws: usize,
    pub debounced_resizes: usize,
    pub footer_line_updates: usize,
    pub stream_chunks: usize,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NoticeTone {
    Information,
    Success,
    Warning,
    Error,
    Cancelled,
    Neutral,
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum NoticeVisibility {
    #[default]
    CompactAndFull,
    FullOnly,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SemanticNotice {
    pub topic: Vec<u8>,
    pub tone: NoticeTone,
    pub body: Vec<u8>,
    pub visibility: NoticeVisibility,
}
pub fn dupe_semantic_notice(n: &SemanticNotice) -> SemanticNotice {
    n.clone()
}
pub fn free_semantic_notice(_: SemanticNotice) {}
pub fn render_context_notice_body(text: &[u8]) -> Vec<u8> {
    text.split(|&b| b == b'\n')
        .enumerate()
        .fold(Vec::new(), |mut o, (i, line)| {
            if i > 0 {
                o.push(b'\n')
            }
            if line != b"[context]" {
                o.extend_from_slice(line.strip_prefix(b"[context] ").unwrap_or(line))
            }
            o
        })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CredentialSource {
    VercelOidcToken,
    AiGatewayApiKey,
    FxLogin,
    StoredKey,
    ChatgptSubscription,
    GrokSubscription,
}
impl CredentialSource {
    pub fn persisted_name(self) -> &'static str {
        match self {
            Self::VercelOidcToken => "vercel_oidc_token",
            Self::AiGatewayApiKey => "ai_gateway_api_key",
            Self::FxLogin => "fx_login",
            Self::StoredKey => "stored_key",
            Self::ChatgptSubscription => "chatgpt_subscription",
            Self::GrokSubscription => "grok_subscription",
        }
    }
}
pub fn parse_credential_source(s: &str) -> Option<CredentialSource> {
    [
        CredentialSource::VercelOidcToken,
        CredentialSource::AiGatewayApiKey,
        CredentialSource::FxLogin,
        CredentialSource::StoredKey,
        CredentialSource::ChatgptSubscription,
        CredentialSource::GrokSubscription,
    ]
    .into_iter()
    .find(|x| x.persisted_name() == s)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TurnPresentationOutcome {
    Completed,
    Interrupted,
    Failed,
    Paused,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TurnFinished {
    pub turn_id: u64,
    pub outcome: TurnPresentationOutcome,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToolLifecycleId {
    pub turn_id: u64,
    pub call_id: Vec<u8>,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ToolPresentationGroupId {
    pub turn_id: u64,
    pub anchor_step_id: u64,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ToolOutcomeKind {
    Completed,
    Denied,
    Cancelled,
    Failed,
    Deferred,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToolOutcome {
    pub kind: ToolOutcomeKind,
    pub summary: Vec<u8>,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ToolLifecycleEvent {
    Provisional {
        id: ToolLifecycleId,
        presentation_group_id: Option<ToolPresentationGroupId>,
        tool_name: Option<Vec<u8>>,
        activity_kind: ToolActivityKind,
    },
    AuthoritativeStarted {
        id: ToolLifecycleId,
        presentation_group_id: Option<ToolPresentationGroupId>,
        reconciles_provisional_call_id: Option<Vec<u8>>,
        tool_name: Vec<u8>,
        activity_kind: ToolActivityKind,
        arguments_json: Option<Vec<u8>>,
        place_after_current_transcript: bool,
    },
    Progress {
        id: ToolLifecycleId,
        text: Vec<u8>,
    },
    Terminal {
        id: ToolLifecycleId,
        outcome: ToolOutcome,
        result: Option<Vec<u8>>,
        result_memory: Option<ToolResultMemory>,
        command_artifact_handle: Option<Vec<u8>>,
    },
    TurnFinished(TurnFinished),
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum TurnPhase {
    #[default]
    Thinking,
    Generating,
    Running,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TurnPhaseUpdate {
    pub turn_id: u64,
    pub step_id: u64,
    pub phase: TurnPhase,
}
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct StreamState {
    pub active: bool,
    pub phase: TurnPhase,
    pub phase_step_id: u64,
    pub chunks: usize,
    pub read_count: usize,
    pub list_count: usize,
    pub write_count: usize,
    pub edit_count: usize,
    pub open_count: usize,
    pub command_count: usize,
    pub subagent_count: usize,
    pub token_progress: TurnTokenProgress,
    pub last_activity_kind: Option<ToolActivityKind>,
    pub turn_started_ms: i64,
    pub waiting_since_ms: i64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RouteRecoveryUnsafeReason {
    AssistantOutput,
    ToolStart,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RouteRecoveryStatusTone {
    Warning,
    Success,
    Danger,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ModelRecoveryCause {
    NetworkInterrupted,
    ResponseInterrupted,
    ProviderUnavailable,
    RateLimited,
    SystemResumed,
    Authentication,
    RequestLimitReached,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ModelRecoveryAction {
    RetryingRequest,
    ContinuingResponse,
    RegeneratingTool,
    ContinuingAfterTool,
    ReconcilingTool,
    WaitingForConnectivity,
    Paused,
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ModelRecoveryRequiredAction {
    #[default]
    None,
    ContinueLater,
    InspectUncertainTool,
    ChangeRequest,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ModelFailureDiagnostic {
    bytes: [u8; 256],
    len: u16,
}
impl ModelFailureDiagnostic {
    pub const MAX_BYTES: usize = 256;
    pub fn default_text_for_cause(c: ModelRecoveryCause) -> &'static [u8] {
        match c {
            ModelRecoveryCause::NetworkInterrupted => b"NetworkInterrupted",
            ModelRecoveryCause::ResponseInterrupted => b"StreamInterrupted",
            ModelRecoveryCause::ProviderUnavailable => b"provider_error",
            ModelRecoveryCause::RateLimited => b"HTTP 429",
            ModelRecoveryCause::SystemResumed => b"SystemResumed",
            ModelRecoveryCause::Authentication => b"AuthenticationExpired",
            ModelRecoveryCause::RequestLimitReached => b"ProviderRequestLimitReached",
        }
    }
    pub fn for_cause(c: ModelRecoveryCause) -> Self {
        Self::new(Self::default_text_for_cause(c))
    }
    pub fn new(text: &[u8]) -> Self {
        let mut d = Self {
            bytes: [0; 256],
            len: 0,
        };
        let value = if text.len() <= 256 {
            text.to_vec()
        } else {
            let mut p = text_utils::utf8_prefix_by_bytes(text, 253).to_vec();
            p.extend_from_slice(b"...");
            p
        };
        d.bytes[..value.len()].copy_from_slice(&value);
        d.len = value.len() as u16;
        d
    }
    pub fn view(&self) -> &[u8] {
        &self.bytes[..self.len as usize]
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RouteRecoveryKind {
    AutoRetry,
    AutoRecovered,
    ManualRetryWithoutFast,
    ManualRecoveredWithoutFast,
    TerminalProviderError,
    UnsafeAssistantOutput,
    UnsafeToolStart,
    ContentFilter,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RouteRecoveryStatus {
    pub kind: RouteRecoveryKind,
    pub failed_attempt: usize,
    pub succeeded_attempt: usize,
    pub attempt_limit: usize,
    pub cause: Option<ModelRecoveryCause>,
    pub action: Option<ModelRecoveryAction>,
    pub required_action: ModelRecoveryRequiredAction,
    pub delay_seconds: u64,
    pub retry_deadline: Option<i128>,
    pub diagnostic: Option<ModelFailureDiagnostic>,
}
impl RouteRecoveryStatus {
    pub const LABEL_MAX_BYTES: usize = 512;
    pub fn tone(self) -> RouteRecoveryStatusTone {
        match self.kind {
            RouteRecoveryKind::AutoRecovered | RouteRecoveryKind::ManualRecoveredWithoutFast => {
                RouteRecoveryStatusTone::Success
            }
            RouteRecoveryKind::AutoRetry | RouteRecoveryKind::ManualRetryWithoutFast => {
                RouteRecoveryStatusTone::Warning
            }
            _ => RouteRecoveryStatusTone::Danger,
        }
    }
    pub fn is_recovered(self) -> bool {
        matches!(
            self.kind,
            RouteRecoveryKind::AutoRecovered | RouteRecoveryKind::ManualRecoveredWithoutFast
        )
    }
    pub fn reported_attempt(self) -> usize {
        if self.is_recovered() && self.succeeded_attempt != 0 {
            self.succeeded_attempt
        } else {
            self.failed_attempt
        }
    }
    pub fn label(self) -> String {
        let diag = self
            .diagnostic
            .map(|d| String::from_utf8_lossy(d.view()).into_owned());
        match self.kind {
            RouteRecoveryKind::AutoRecovered => format!(
                "✓ recovered · succeeded on attempt {}/{}",
                self.succeeded_attempt, self.attempt_limit
            ),
            RouteRecoveryKind::ManualRecoveredWithoutFast => "✓ recovered · Fast disabled".into(),
            RouteRecoveryKind::ManualRetryWithoutFast => diag.map_or_else(
                || "⚠ Provider route unavailable · continuing without Fast".into(),
                |d| format!("⚠ Provider route unavailable · {d} · continuing without Fast"),
            ),
            RouteRecoveryKind::AutoRetry => {
                let cause = match self
                    .cause
                    .unwrap_or(ModelRecoveryCause::ProviderUnavailable)
                {
                    ModelRecoveryCause::NetworkInterrupted => "Network interrupted",
                    ModelRecoveryCause::ResponseInterrupted => "Response ended early",
                    ModelRecoveryCause::ProviderUnavailable => "Provider unavailable",
                    ModelRecoveryCause::RateLimited => "Rate limited",
                    ModelRecoveryCause::SystemResumed => "Mac woke from sleep",
                    ModelRecoveryCause::Authentication => "Authentication refreshed",
                    ModelRecoveryCause::RequestLimitReached => "Provider request limit reached",
                };
                let action = match self.action.unwrap_or(ModelRecoveryAction::RetryingRequest) {
                    ModelRecoveryAction::RetryingRequest => "retrying request",
                    ModelRecoveryAction::ContinuingResponse => "continuing response",
                    ModelRecoveryAction::RegeneratingTool => "regenerating unstarted tool",
                    ModelRecoveryAction::ContinuingAfterTool => "continuing after confirmed tool",
                    ModelRecoveryAction::ReconcilingTool => "checking uncertain tool state",
                    ModelRecoveryAction::WaitingForConnectivity => "waiting for connection",
                    ModelRecoveryAction::Paused => "recovery paused",
                };
                let delay = if self.delay_seconds > 0 {
                    format!(" in {}s", self.delay_seconds)
                } else {
                    String::new()
                };
                format!(
                    "⚠ {cause}{} · {action}{delay} · attempt {}/{}",
                    diag.map(|d| format!(" · {d}")).unwrap_or_default(),
                    self.failed_attempt,
                    self.attempt_limit
                )
            }
            RouteRecoveryKind::UnsafeAssistantOutput => {
                "⚠ Response interrupted · partial output preserved".into()
            }
            RouteRecoveryKind::UnsafeToolStart => {
                "⚠ Tool state needs review · context preserved".into()
            }
            RouteRecoveryKind::ContentFilter => "⚠ blocked · content filter".into(),
            RouteRecoveryKind::TerminalProviderError => {
                "⚠ Provider unavailable · recovery paused".into()
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ChatRole {
    System,
    User,
    Assistant,
    Tool,
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum FinalToolIdentity {
    #[default]
    Valid,
    Absent,
    Empty,
    WrongType,
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ToolExecutionProvenance {
    #[default]
    FxLocal,
    ProviderExecuted,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProviderResultIdentityFailure {
    Absent,
    Empty,
    WrongType,
    Unmatched,
    Ambiguous,
    MissingResult,
    IncompleteStreamedInput,
    InvalidToolInput,
    ConflictingToolName,
    MalformedProviderExecuted,
    ProvenanceContradiction,
    MalformedPreliminary,
    DuplicateResult,
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ToolArgumentIntegrity {
    #[default]
    Valid,
    MalformedJson,
}
impl ToolArgumentIntegrity {
    pub fn classify_serialized(s: &[u8]) -> Self {
        if valid_json(s) {
            Self::Valid
        } else {
            Self::MalformedJson
        }
    }
}
fn valid_json(s: &[u8]) -> bool {
    struct P<'a> {
        s: &'a [u8],
        i: usize,
    }
    impl P<'_> {
        fn ws(&mut self) {
            while self.i < self.s.len() && self.s[self.i].is_ascii_whitespace() {
                self.i += 1
            }
        }
        fn value(&mut self) -> bool {
            self.ws();
            if self.i >= self.s.len() {
                return false;
            }
            match self.s[self.i] {
                b'"' => self.string().is_some(),
                b'{' => self.object(),
                b'[' => self.array(),
                b't' => self.literal(b"true"),
                b'f' => self.literal(b"false"),
                b'n' => self.literal(b"null"),
                b'-' | b'0'..=b'9' => self.number(),
                _ => false,
            }
        }
        fn string(&mut self) -> Option<Vec<u8>> {
            self.i += 1;
            let mut o = Vec::new();
            while self.i < self.s.len() {
                let b = self.s[self.i];
                self.i += 1;
                if b == b'"' {
                    return Some(o);
                }
                if b < 0x20 {
                    return None;
                }
                if b == b'\\' {
                    if self.i >= self.s.len() {
                        return None;
                    }
                    let e = self.s[self.i];
                    self.i += 1;
                    if e == b'u' {
                        for _ in 0..4 {
                            if self.i >= self.s.len() || !self.s[self.i].is_ascii_hexdigit() {
                                return None;
                            }
                            self.i += 1
                        }
                    } else if !b"\"\\/bfnrt".contains(&e) {
                        return None;
                    }
                }
                o.push(b)
            }
            None
        }
        fn object(&mut self) -> bool {
            self.i += 1;
            self.ws();
            let mut keys: Vec<Vec<u8>> = Vec::new();
            if self.s.get(self.i) == Some(&b'}') {
                self.i += 1;
                return true;
            }
            loop {
                self.ws();
                let Some(k) = self.string() else { return false };
                if keys.contains(&k) {
                    return false;
                }
                keys.push(k);
                self.ws();
                if self.s.get(self.i) != Some(&b':') {
                    return false;
                }
                self.i += 1;
                if !self.value() {
                    return false;
                }
                self.ws();
                match self.s.get(self.i) {
                    Some(b',') => self.i += 1,
                    Some(b'}') => {
                        self.i += 1;
                        return true;
                    }
                    _ => return false,
                }
            }
        }
        fn array(&mut self) -> bool {
            self.i += 1;
            self.ws();
            if self.s.get(self.i) == Some(&b']') {
                self.i += 1;
                return true;
            }
            loop {
                if !self.value() {
                    return false;
                }
                self.ws();
                match self.s.get(self.i) {
                    Some(b',') => self.i += 1,
                    Some(b']') => {
                        self.i += 1;
                        return true;
                    }
                    _ => return false,
                }
            }
        }
        fn literal(&mut self, l: &[u8]) -> bool {
            if self.s[self.i..].starts_with(l) {
                self.i += l.len();
                true
            } else {
                false
            }
        }
        fn number(&mut self) -> bool {
            let start = self.i;
            if self.s[self.i] == b'-' {
                self.i += 1
            }
            if self.i >= self.s.len() {
                return false;
            }
            if self.s[self.i] == b'0' {
                self.i += 1
            } else if self.s[self.i].is_ascii_digit() {
                while self.i < self.s.len() && self.s[self.i].is_ascii_digit() {
                    self.i += 1
                }
            } else {
                return false;
            }
            if self.s.get(self.i) == Some(&b'.') {
                self.i += 1;
                let p = self.i;
                while self.i < self.s.len() && self.s[self.i].is_ascii_digit() {
                    self.i += 1
                }
                if p == self.i {
                    return false;
                }
            }
            if matches!(self.s.get(self.i), Some(b'e' | b'E')) {
                self.i += 1;
                if matches!(self.s.get(self.i), Some(b'+' | b'-')) {
                    self.i += 1
                }
                let p = self.i;
                while self.i < self.s.len() && self.s[self.i].is_ascii_digit() {
                    self.i += 1
                }
                if p == self.i {
                    return false;
                }
            }
            self.i > start
        }
    }
    let mut p = P { s, i: 0 };
    let ok = p.value();
    p.ws();
    ok && p.i == s.len()
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToolCall {
    pub id: Vec<u8>,
    pub name: Vec<u8>,
    pub arguments_json: Vec<u8>,
    pub argument_integrity: ToolArgumentIntegrity,
    pub provisional_id: Option<Vec<u8>>,
    pub provider_result: Option<Vec<u8>>,
    pub final_identity: FinalToolIdentity,
    pub provenance: ToolExecutionProvenance,
}
impl ToolCall {
    pub fn new(
        id: impl Into<Vec<u8>>,
        name: impl Into<Vec<u8>>,
        arguments_json: impl Into<Vec<u8>>,
    ) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            arguments_json: arguments_json.into(),
            argument_integrity: ToolArgumentIntegrity::Valid,
            provisional_id: None,
            provider_result: None,
            final_identity: FinalToolIdentity::Valid,
            provenance: ToolExecutionProvenance::FxLocal,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WebSearchProgress {
    QueryStarted(Vec<u8>),
    ResultsReceived { query: Vec<u8>, result_count: usize },
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WebSearchCompletion {
    pub searches: u32,
    pub duration_ms: u64,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WebFetchProgress {
    Fetching(Vec<u8>),
    Converting(Vec<u8>),
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum WebFetchArtifactState {
    #[default]
    None,
    Stored,
    Unavailable,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WebFetchCompletion {
    url_buf: [u8; 192],
    url_len: u8,
    pub bytes: u64,
    pub status: u16,
    pub duration_ms: u64,
    pub cache_hit: bool,
    pub artifact_state: WebFetchArtifactState,
}
impl Default for WebFetchCompletion {
    fn default() -> Self {
        Self {
            url_buf: [0; 192],
            url_len: 0,
            bytes: 0,
            status: 0,
            duration_ms: 0,
            cache_hit: false,
            artifact_state: WebFetchArtifactState::None,
        }
    }
}
impl WebFetchCompletion {
    pub const MAX_URL_LEN: usize = 192;
    pub fn set_url(&mut self, url: &[u8]) {
        let n = url.len().min(192);
        self.url_buf[..n].copy_from_slice(&url[..n]);
        self.url_len = n as u8
    }
    pub fn url(&self) -> &[u8] {
        &self.url_buf[..self.url_len as usize]
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PersistedToolStatus {
    Success,
    Failure,
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum FileEvidenceAction {
    Read,
    Write,
    Edit,
    Delete,
    Rename,
    Copy,
    Search,
    List,
    #[default]
    Unknown,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CommittedFilePresentationKind {
    Added,
    Edited,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CommittedFilePresentationLineKind {
    Context,
    Addition,
    Deletion,
    Elision,
    Notice,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommittedFilePresentationLine {
    pub kind: CommittedFilePresentationLineKind,
    pub old_line: Option<u32>,
    pub new_line: Option<u32>,
    pub text: Vec<u8>,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommittedFilePresentation {
    pub path: Vec<u8>,
    pub kind: CommittedFilePresentationKind,
    pub lines: Vec<CommittedFilePresentationLine>,
    pub additions: usize,
    pub deletions: usize,
    pub truncated: bool,
    pub previous_content: Option<Vec<u8>>,
    pub after_content: Option<Vec<u8>>,
    pub lifecycle_id: Option<ToolLifecycleId>,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommandOutputReplayDescriptor {
    pub handle: Vec<u8>,
    pub framed_bytes: usize,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CommandOutputReplay {
    Available(CommandOutputReplayDescriptor),
    Unavailable,
}
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct CancelledCommandPresentation {
    pub output_replay: Option<CommandOutputReplay>,
    pub command_artifact_handle: Option<Vec<u8>>,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CommandProcessPresentation {
    ExitCode(i64),
    Signal(u32),
    TimedOut,
    OutputCaptureFailed,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TerminalReturnPresentation {
    Started,
    ConditionMet,
    SafetyCeiling,
    Cancelled,
    Exited(i32),
    Signal(u32),
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TerminalFailurePresentation {
    InvalidRequest,
    PathOutsideWorkspace,
    UnsupportedHost,
    ShellUnavailable,
    PtyUnavailable,
    StartupFailed,
    ProcessIdentityUnavailable,
    SessionLost,
    SessionNotFound,
    InvalidLifecycle,
    AuthorityDenied,
    AuthorityRetired,
    LeaseConflict,
    CursorGap,
    ScreenUnavailable,
    MonitorUnavailable,
    ProtocolIncompatible,
    CapacityExceeded,
    Cancelled,
}
impl TerminalFailurePresentation {
    pub fn detail(self) -> &'static str {
        match self {
            Self::InvalidRequest => "invalid request",
            Self::PathOutsideWorkspace => "path is outside the workspace",
            Self::UnsupportedHost => "terminal host is unavailable",
            Self::ShellUnavailable => "terminal shell is unavailable",
            Self::PtyUnavailable => "terminal PTY is unavailable",
            Self::StartupFailed => "terminal startup failed",
            Self::ProcessIdentityUnavailable => "terminal process identity is unavailable",
            Self::SessionLost => "terminal session was lost",
            Self::SessionNotFound => "terminal session not found",
            Self::InvalidLifecycle => "terminal session is in an invalid lifecycle state",
            Self::AuthorityDenied => "terminal authority denied",
            Self::AuthorityRetired => {
                "saved terminal authority is from an older fx version; start a new terminal"
            }
            Self::LeaseConflict => "terminal control lease conflict",
            Self::CursorGap => "terminal output cursor gap",
            Self::ScreenUnavailable => "terminal screen is unavailable",
            Self::MonitorUnavailable => "terminal monitor is unavailable",
            Self::ProtocolIncompatible => "terminal protocol is incompatible",
            Self::CapacityExceeded => "terminal capacity exceeded",
            Self::Cancelled => "terminal action was cancelled",
        }
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TerminalActionPresentation {
    Returned(TerminalReturnPresentation),
    Failed(TerminalFailurePresentation),
}
impl TerminalActionPresentation {
    pub fn outcome_kind(self) -> ToolOutcomeKind {
        match self {
            Self::Returned(
                TerminalReturnPresentation::Started
                | TerminalReturnPresentation::ConditionMet
                | TerminalReturnPresentation::SafetyCeiling
                | TerminalReturnPresentation::Exited(0),
            ) => ToolOutcomeKind::Completed,
            Self::Returned(TerminalReturnPresentation::Cancelled)
            | Self::Failed(TerminalFailurePresentation::Cancelled) => ToolOutcomeKind::Cancelled,
            _ => ToolOutcomeKind::Failed,
        }
    }
}
pub const DEFERRED_TOOL_RESULT_OUTPUT: &[u8] = b"Not executed";
pub const CONTEXT_DEFERRED_TOOL_RESULT_OUTPUT:&[u8]=b"Scoped project instructions were added before execution. Review them and reissue this tool call if it is still appropriate.";
pub const CONTEXT_DEFERRED_TOOL_STATUS_LABEL: &[u8] = b"Context updated";
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PersistedToolResult {
    pub tool_call_id: Vec<u8>,
    pub tool_name: Vec<u8>,
    pub status: PersistedToolStatus,
    pub output: Vec<u8>,
    pub output_handle: Option<Vec<u8>>,
    pub preview: Option<Vec<u8>>,
    pub output_bytes: usize,
    pub stored_output_bytes: usize,
    pub truncated: bool,
    pub provider_native: bool,
    pub created_at_ms: i64,
    pub permission_feedback: Vec<Vec<u8>>,
    pub committed_file_presentation: Option<CommittedFilePresentation>,
    pub command_output_replay: Option<CommandOutputReplay>,
    pub command_process_presentation: Option<CommandProcessPresentation>,
    pub terminal_action_presentation: Option<TerminalActionPresentation>,
}
pub fn is_context_deferred_tool_result(r: &PersistedToolResult) -> bool {
    r.status == PersistedToolStatus::Failure && r.output == CONTEXT_DEFERRED_TOOL_RESULT_OUTPUT
}
pub fn is_deferred_tool_result(r: &PersistedToolResult) -> bool {
    r.status == PersistedToolStatus::Failure
        && (r.output == DEFERRED_TOOL_RESULT_OUTPUT || is_context_deferred_tool_result(r))
}
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ToolResultMemory {
    pub output_handle: Option<Vec<u8>>,
    pub preview: Option<Vec<u8>>,
    pub output_bytes: usize,
    pub stored_output_bytes: usize,
    pub truncated: bool,
    pub model_view_covers_full_file: Option<bool>,
    pub committed_file_presentation: Option<CommittedFilePresentation>,
    pub command_output_replay: Option<CommandOutputReplay>,
    pub command_process_presentation: Option<CommandProcessPresentation>,
    pub terminal_action_presentation: Option<TerminalActionPresentation>,
}
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ToolExecutionStep {
    pub assistant: Option<Vec<u8>>,
    pub tool_calls: Vec<ToolCall>,
    pub tool_results: Vec<PersistedToolResult>,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FileEvidence {
    pub path: Vec<u8>,
    pub new_path: Option<Vec<u8>>,
    pub tool_call_id: Vec<u8>,
    pub tool_name: Vec<u8>,
    pub action: FileEvidenceAction,
    pub status: PersistedToolStatus,
    pub model_view_covers_full_file: bool,
    pub stale: bool,
}
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ExecutionMemory {
    pub tool_steps: Vec<ToolExecutionStep>,
    pub files: Vec<FileEvidence>,
    pub steering: Vec<Vec<u8>>,
}
impl ExecutionMemory {
    pub fn is_empty(&self) -> bool {
        self.tool_steps.is_empty() && self.files.is_empty() && self.steering.is_empty()
    }
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ImageAttachment {
    pub id: usize,
    pub path: Vec<u8>,
    pub media_type: Vec<u8>,
    pub snapshot_path: Option<Vec<u8>>,
    pub snapshot_sha256: Option<Vec<u8>>,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UserTurn {
    pub text: Vec<u8>,
    pub images: Vec<ImageAttachment>,
    pub work_id: Option<Vec<u8>>,
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ChatCachePolicy {
    #[default]
    Default,
    NoCache,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChatMessage {
    pub role: ChatRole,
    pub content: Option<Vec<u8>>,
    pub images: Vec<ImageAttachment>,
    pub tool_call_id: Option<Vec<u8>>,
    pub tool_name: Option<Vec<u8>>,
    pub tool_calls: Vec<ToolCall>,
    pub provider_state_json: Option<Vec<u8>>,
    pub tool_result_status: Option<PersistedToolStatus>,
    pub tool_result_memory: Option<ToolResultMemory>,
    pub permission_feedback: bool,
    pub cache_policy: ChatCachePolicy,
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Usage {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub cache_read_tokens: Option<u64>,
    pub cache_write_tokens: Option<u64>,
    pub reasoning_tokens: Option<u64>,
}
#[derive(Clone, Debug, PartialEq)]
pub struct ProviderBilling {
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
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct TurnTokenProgress {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub input_exact: bool,
    pub output_exact: bool,
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ToolUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub web_search_requests: u32,
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct TurnSummary {
    pub thinking_duration_ms: u64,
    pub turn_duration_ms: u64,
    pub token_progress: TurnTokenProgress,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProviderFinishReason {
    Stop,
    Length,
    ContentFilter,
    ToolCalls,
    ProviderError,
    Other,
}
impl ProviderFinishReason {
    pub fn parse_unified(s: &str) -> Option<Self> {
        match s {
            "stop" => Some(Self::Stop),
            "length" => Some(Self::Length),
            "content-filter" => Some(Self::ContentFilter),
            "tool-calls" => Some(Self::ToolCalls),
            "error" => Some(Self::ProviderError),
            "other" => Some(Self::Other),
            _ => None,
        }
    }
    pub fn parse_legacy(s: &str) -> Option<Self> {
        match s {
            "tool_calls" => Some(Self::ToolCalls),
            "content_filter" => Some(Self::ContentFilter),
            _ => Self::parse_unified(s),
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Self::Stop => "stop",
            Self::Length => "length",
            Self::ContentFilter => "content-filter",
            Self::ToolCalls => "tool-calls",
            Self::ProviderError => "error",
            Self::Other => "other",
        }
    }
}
#[derive(Clone, Debug, PartialEq)]
pub struct ModelCompletion {
    pub content: Option<Vec<u8>>,
    pub tool_calls: Vec<ToolCall>,
    pub generation_id: Option<Vec<u8>>,
    pub billing: Option<ProviderBilling>,
    pub generation_metadata_invalid: bool,
    pub delivery_ambiguous: bool,
    pub provider_result_identity_failure: Option<ProviderResultIdentityFailure>,
    pub provider_failure_detail: Option<Vec<u8>>,
    pub provider_state_json: Option<Vec<u8>>,
    pub finish_reason: Option<ProviderFinishReason>,
    pub usage: Usage,
}
impl Default for ModelCompletion {
    fn default() -> Self {
        Self {
            content: None,
            tool_calls: vec![],
            generation_id: None,
            billing: None,
            generation_metadata_invalid: false,
            delivery_ambiguous: false,
            provider_result_identity_failure: None,
            provider_failure_detail: None,
            provider_state_json: None,
            finish_reason: None,
            usage: Usage::default(),
        }
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InvalidGatewayTimestamp;
pub fn parse_gateway_timestamp(text: &str) -> Result<i64, InvalidGatewayTimestamp> {
    let b = text.as_bytes();
    if b.len() < 20
        || b.get(4) != Some(&b'-')
        || b.get(7) != Some(&b'-')
        || b.get(10) != Some(&b'T')
        || b.get(13) != Some(&b':')
        || b.get(16) != Some(&b':')
    {
        return Err(InvalidGatewayTimestamp);
    }
    let num = |s: &[u8]| -> Result<u32, InvalidGatewayTimestamp> {
        if s.iter().all(u8::is_ascii_digit) {
            std::str::from_utf8(s)
                .ok()
                .and_then(|x| x.parse().ok())
                .ok_or(InvalidGatewayTimestamp)
        } else {
            Err(InvalidGatewayTimestamp)
        }
    };
    let (y, m, d, h, min, sec) = (
        num(&b[..4])?,
        num(&b[5..7])?,
        num(&b[8..10])?,
        num(&b[11..13])?,
        num(&b[14..16])?,
        num(&b[17..19])?,
    );
    if y < 1970
        || !(1..=12).contains(&m)
        || d < 1
        || d > days_in_month(y, m)
        || h > 23
        || min > 59
        || sec > 59
    {
        return Err(InvalidGatewayTimestamp);
    }
    let mut i = 19;
    let mut ms = 0;
    if b.get(i) == Some(&b'.') {
        i += 1;
        let start = i;
        while b.get(i).is_some_and(u8::is_ascii_digit) {
            i += 1
        }
        let f = &b[start..i];
        if f.is_empty() || f.len() > 9 {
            return Err(InvalidGatewayTimestamp);
        }
        let n = f.len().min(3);
        ms = num(&f[..n])? as i64
            * match n {
                1 => 100,
                2 => 10,
                _ => 1,
            };
    }
    if i + 1 != b.len() || b[i] != b'Z' {
        return Err(InvalidGatewayTimestamp);
    }
    let days = days_from_civil(y, m, d);
    if days < 0 {
        return Err(InvalidGatewayTimestamp);
    }
    Ok((days * 86400 + h as i64 * 3600 + min as i64 * 60 + sec as i64) * 1000 + ms)
}
fn days_in_month(y: u32, m: u32) -> u32 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if y % 4 == 0 && (y % 100 != 0 || y % 400 == 0) => 29,
        2 => 28,
        _ => 0,
    }
}
fn days_from_civil(y: u32, m: u32, d: u32) -> i64 {
    let y = y as i64 - if m <= 2 { 1 } else { 0 };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = m as i64 + if m > 2 { -3 } else { 9 };
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}
pub fn valid_gateway_generation_id(id: &[u8]) -> bool {
    !id.is_empty()
        && id.len() <= 256
        && id
            .iter()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.' | b':'))
}
pub fn valid_gateway_team(t: &[u8]) -> bool {
    !t.is_empty()
        && t.len() <= 128
        && t.iter()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProviderCompletionDisposition {
    Completed,
    LengthLimited,
    Interrupted,
    ProviderFailure,
    InvalidCompletion,
}
pub fn all_tool_calls_provider_executed(c: &[ToolCall]) -> bool {
    !c.is_empty()
        && c.iter()
            .all(|x| x.provenance == ToolExecutionProvenance::ProviderExecuted)
}
pub fn classify_provider_completion(c: &ModelCompletion) -> ProviderCompletionDisposition {
    match c.finish_reason {
        None => ProviderCompletionDisposition::Interrupted,
        Some(ProviderFinishReason::ProviderError | ProviderFinishReason::ContentFilter) => {
            ProviderCompletionDisposition::ProviderFailure
        }
        Some(ProviderFinishReason::Length) => ProviderCompletionDisposition::LengthLimited,
        Some(ProviderFinishReason::ToolCalls) => {
            if c.tool_calls.is_empty() {
                ProviderCompletionDisposition::InvalidCompletion
            } else {
                ProviderCompletionDisposition::Completed
            }
        }
        Some(ProviderFinishReason::Stop) => {
            if c.tool_calls.is_empty() || all_tool_calls_provider_executed(&c.tool_calls) {
                ProviderCompletionDisposition::Completed
            } else {
                ProviderCompletionDisposition::InvalidCompletion
            }
        }
        Some(ProviderFinishReason::Other) => {
            if c.tool_calls.is_empty() {
                ProviderCompletionDisposition::Completed
            } else {
                ProviderCompletionDisposition::InvalidCompletion
            }
        }
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AuthoritativeToolAdmission {
    Admitted,
    RejectMalformedIdentity(FinalToolIdentity),
    RejectMalformedProviderResult(ProviderResultIdentityFailure),
    RejectMalformedProviderArguments,
    RejectDuplicateIdentity,
}
pub fn authoritative_tool_admission(c: &ModelCompletion) -> AuthoritativeToolAdmission {
    if let Some(f) = c.provider_result_identity_failure {
        return AuthoritativeToolAdmission::RejectMalformedProviderResult(f);
    }
    for x in &c.tool_calls {
        let id = if x.final_identity == FinalToolIdentity::Valid && x.id.is_empty() {
            FinalToolIdentity::Empty
        } else {
            x.final_identity
        };
        if id != FinalToolIdentity::Valid {
            return if x.provenance == ToolExecutionProvenance::ProviderExecuted {
                AuthoritativeToolAdmission::RejectMalformedProviderResult(match id {
                    FinalToolIdentity::Absent => ProviderResultIdentityFailure::Absent,
                    FinalToolIdentity::Empty => ProviderResultIdentityFailure::Empty,
                    _ => ProviderResultIdentityFailure::WrongType,
                })
            } else {
                AuthoritativeToolAdmission::RejectMalformedIdentity(id)
            };
        }
    }
    for (i, x) in c.tool_calls.iter().enumerate() {
        if c.tool_calls[..i].iter().any(|p| p.id == x.id) {
            return AuthoritativeToolAdmission::RejectDuplicateIdentity;
        }
    }
    for x in &c.tool_calls {
        if x.provenance == ToolExecutionProvenance::ProviderExecuted
            && x.argument_integrity == ToolArgumentIntegrity::MalformedJson
        {
            return AuthoritativeToolAdmission::RejectMalformedProviderArguments;
        }
        if x.provenance == ToolExecutionProvenance::ProviderExecuted && x.provider_result.is_none()
        {
            return AuthoritativeToolAdmission::RejectMalformedProviderResult(
                ProviderResultIdentityFailure::MissingResult,
            );
        }
    }
    AuthoritativeToolAdmission::Admitted
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConversationLanguage {
    bytes: [u8; 24],
    len: u8,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InvalidConversationLanguage;
impl Default for ConversationLanguage {
    fn default() -> Self {
        Self::from_slice(b"und").unwrap()
    }
}
impl ConversationLanguage {
    pub const MAX_LEN: usize = 24;
    pub fn literal(tag: &[u8]) -> Self {
        Self::from_slice(tag).expect("invalid conversation language literal")
    }
    pub fn from_slice(tag: &[u8]) -> Result<Self, InvalidConversationLanguage> {
        let t = tag.strip_prefix(b" ").unwrap_or(tag);
        let t = t.strip_suffix(b" ").unwrap_or(t);
        if t.is_empty() || t.len() > 24 {
            return Err(InvalidConversationLanguage);
        }
        let mut v = Self {
            bytes: [0; 24],
            len: t.len() as u8,
        };
        v.bytes[..t.len()].copy_from_slice(t);
        Ok(v)
    }
    pub fn view(&self) -> &[u8] {
        &self.bytes[..self.len as usize]
    }
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AssistantHistoryTurn {
    pub user: UserTurn,
    pub assistant: Vec<u8>,
    pub execution: ExecutionMemory,
}
pub type StableBackgroundRecordId = [u8; 16];
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BackgroundCommandHistoryTurn {
    pub user: UserTurn,
    pub assistant: Option<Vec<u8>>,
    pub execution: ExecutionMemory,
    pub log_path: Vec<u8>,
    pub expect_url: bool,
    pub url: Option<Vec<u8>>,
    pub background_record_id: Option<StableBackgroundRecordId>,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InterruptedTerminalReason {
    Cancelled,
    Failed,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InterruptedHistoryTurn {
    pub user: UserTurn,
    pub assistant: Option<Vec<u8>>,
    pub tool_call: Option<ToolCall>,
    pub completed_tool_names: Vec<Vec<u8>>,
    pub execution: ExecutionMemory,
    pub cancelled_command: Option<CancelledCommandPresentation>,
    pub terminal_reason: InterruptedTerminalReason,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CompactedSummaryHistoryTurn {
    pub summary: Vec<u8>,
    pub removed_turn_count: usize,
    pub compaction_count: usize,
    pub root_user_messages: Vec<Vec<u8>>,
    pub root_user_messages_complete: bool,
    pub permission_feedback: Vec<Vec<u8>>,
    pub permission_feedback_complete: bool,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum HistoryTurn {
    CompactedSummary(CompactedSummaryHistoryTurn),
    Assistant(AssistantHistoryTurn),
    BackgroundCommand(BackgroundCommandHistoryTurn),
    Interrupted(InterruptedHistoryTurn),
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum FinishedPromptProjection {
    #[default]
    HistoryDefault,
    AssistantText,
}
#[derive(Clone, Debug, PartialEq)]
pub struct FinishedPrompt {
    pub turn: HistoryTurn,
    pub summary: Option<TurnSummary>,
    pub terminal_projection: FinishedPromptProjection,
    pub terminal_outcome: Option<TurnPresentationOutcome>,
}
pub fn dupe_history_turn(t: &HistoryTurn) -> HistoryTurn {
    t.clone()
}
pub fn free_history_turn(_: HistoryTurn) {}
pub fn dupe_finished_prompt(f: &FinishedPrompt) -> FinishedPrompt {
    f.clone()
}
pub fn free_finished_prompt(_: FinishedPrompt) {}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PermissionMode {
    Ask,
    Auto,
    Yolo,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RuleDecision {
    None,
    Allow,
    Ask,
    Deny,
}
pub type ContentHash = [u8; 32];
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ToolChoice {
    #[default]
    Auto,
    None,
    Required,
}
impl ToolChoice {
    pub fn label(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::None => "none",
            Self::Required => "required",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        if s.eq_ignore_ascii_case("auto") {
            Some(Self::Auto)
        } else if s.eq_ignore_ascii_case("none") {
            Some(Self::None)
        } else {
            None
        }
    }
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ReasoningEffort {
    Auto,
    Named(Vec<u8>),
}
impl Default for ReasoningEffort {
    fn default() -> Self {
        Self::Auto
    }
}
impl ReasoningEffort {
    pub const MAX_NAME_BYTES: usize = 64;
    pub const MAX_OPTIONS: usize = 16;
    pub fn literal(raw: &[u8]) -> Self {
        Self::parse(raw).expect("invalid reasoning effort literal")
    }
    pub fn label(&self) -> &[u8] {
        match self {
            Self::Auto => b"auto",
            Self::Named(n) => n,
        }
    }
    pub fn display_label(&self) -> &[u8] {
        if matches!(self, Self::Auto) {
            b"default"
        } else {
            self.label()
        }
    }
    pub fn gateway_value(&self) -> Option<&[u8]> {
        if matches!(self, Self::Auto) {
            None
        } else {
            Some(self.label())
        }
    }
    pub fn parse(raw: &[u8]) -> Option<Self> {
        if [b"auto".as_slice(), b"adaptive", b"default"]
            .iter()
            .any(|v| raw.eq_ignore_ascii_case(v))
        {
            return Some(Self::Auto);
        }
        if raw.is_empty()
            || raw.len() > 64
            || !raw
                .iter()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
        {
            None
        } else {
            Some(Self::Named(raw.to_vec()))
        }
    }
    pub fn parse_display_label(raw: &[u8]) -> Option<Self> {
        Self::parse(raw)
    }
    pub fn is_default(&self) -> bool {
        matches!(self, Self::Auto)
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ToolPermissionDenialReason {
    UserDenied,
    AutoDenied,
    ReviewCaution,
    ReviewUnavailable,
    PolicyDenied,
    PermissionRequired,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ToolPermissionDecision {
    Once,
    Always,
    Deny,
    PolicyDenied,
    PermissionRequired,
}
impl ToolPermissionDecision {
    pub fn is_denied(self) -> bool {
        matches!(
            self,
            Self::Deny | Self::PolicyDenied | Self::PermissionRequired
        )
    }
    pub fn denial_reason(self) -> Option<ToolPermissionDenialReason> {
        match self {
            Self::Deny => Some(ToolPermissionDenialReason::UserDenied),
            Self::PolicyDenied => Some(ToolPermissionDenialReason::PolicyDenied),
            Self::PermissionRequired => Some(ToolPermissionDenialReason::PermissionRequired),
            _ => None,
        }
    }
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PermissionGrant {
    pub tool_name: Vec<u8>,
    pub target_path: Vec<u8>,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PermissionAction {
    Allow,
    Ask,
    Deny,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PermissionRule {
    pub permission: Vec<u8>,
    pub pattern: Vec<u8>,
    pub action: PermissionAction,
}
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct PermissionRuleSet {
    pub rules: Vec<PermissionRule>,
}
impl PermissionRuleSet {
    pub fn deinit(&mut self) {
        self.rules.clear()
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ToolActivityKind {
    Read,
    List,
    Write,
    Edit,
    Open,
    Command,
    Subagent,
    Ask,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct QuestionOption {
    pub label: Vec<u8>,
    pub description: Option<Vec<u8>>,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct QuestionBatchEntry {
    pub question: Vec<u8>,
    pub options: Vec<QuestionOption>,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct QuestionAnswer {
    pub question: Vec<u8>,
    pub answer: Vec<u8>,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResolveMode {
    Existing,
    Create,
}
pub fn dupe_completed_tool_names(v: &[Vec<u8>]) -> Vec<Vec<u8>> {
    v.to_vec()
}
pub fn free_completed_tool_names(_: Vec<Vec<u8>>) {}
pub fn dupe_execution_memory(v: &ExecutionMemory) -> ExecutionMemory {
    v.clone()
}
pub fn free_execution_memory(_: ExecutionMemory) {}
pub fn dupe_tool_execution_steps(v: &[ToolExecutionStep]) -> Vec<ToolExecutionStep> {
    v.to_vec()
}
pub fn free_tool_execution_steps(_: Vec<ToolExecutionStep>) {}
pub fn dupe_tool_call_slice(v: &[ToolCall]) -> Vec<ToolCall> {
    v.to_vec()
}
pub fn free_tool_call_slice(_: Vec<ToolCall>) {}
pub fn dupe_persisted_tool_results(v: &[PersistedToolResult]) -> Vec<PersistedToolResult> {
    v.to_vec()
}
pub fn free_persisted_tool_results(_: Vec<PersistedToolResult>) {}
pub fn dupe_committed_file_presentation(
    v: &CommittedFilePresentation,
) -> CommittedFilePresentation {
    v.clone()
}
pub fn free_committed_file_presentation(_: CommittedFilePresentation) {}
pub fn dupe_command_output_replay(v: &CommandOutputReplay) -> CommandOutputReplay {
    v.clone()
}
pub fn free_command_output_replay(_: CommandOutputReplay) {}
pub fn dupe_permission_feedback(v: &[Vec<u8>]) -> Vec<Vec<u8>> {
    v.to_vec()
}
pub fn free_permission_feedback(_: Vec<Vec<u8>>) {}
pub fn dupe_file_evidence_slice(v: &[FileEvidence]) -> Vec<FileEvidence> {
    v.to_vec()
}
pub fn free_file_evidence_slice(_: Vec<FileEvidence>) {}
pub fn dupe_tool_call(v: &ToolCall) -> ToolCall {
    v.clone()
}
pub fn free_tool_call(_: ToolCall) {}
pub fn dupe_user_turn(v: &UserTurn) -> UserTurn {
    v.clone()
}
pub fn free_user_turn(_: UserTurn) {}
pub fn dupe_image_attachment_slice(v: &[ImageAttachment]) -> Vec<ImageAttachment> {
    v.to_vec()
}
pub fn free_image_attachment(_: ImageAttachment) {}
pub fn free_image_attachment_slice(_: Vec<ImageAttachment>) {}
pub fn dupe_permission_grant_slice(v: &[PermissionGrant]) -> Vec<PermissionGrant> {
    v.to_vec()
}
pub fn free_permission_grant_slice(_: Vec<PermissionGrant>) {}
pub fn dupe_permission_rule_slice(v: &[PermissionRule]) -> Vec<PermissionRule> {
    v.to_vec()
}
pub fn free_permission_rule_slice(_: Vec<PermissionRule>) {}
pub fn dupe_permission_rule_set(v: &PermissionRuleSet) -> PermissionRuleSet {
    v.clone()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn context() {
        assert_eq!(
            render_context_notice_body(b"[context] first\n[context] second\n[context]\n"),
            b"first\nsecond\n\n"
        )
    }
    #[test]
    fn timestamps() {
        assert_eq!(
            parse_gateway_timestamp("2024-01-02T03:04:05.678Z"),
            Ok(1_704_164_645_678)
        );
        assert!(parse_gateway_timestamp("2024-02-30T00:00:00Z").is_err())
    }
    #[test]
    fn language() {
        assert_eq!(ConversationLanguage::default().view(), b"und");
        assert_eq!(
            ConversationLanguage::from_slice(b"ja").unwrap().view(),
            b"ja"
        )
    }
    #[test]
    fn json_integrity() {
        assert_eq!(
            ToolArgumentIntegrity::classify_serialized(br#"{"a":1}"#),
            ToolArgumentIntegrity::Valid
        );
        assert_eq!(
            ToolArgumentIntegrity::classify_serialized(br#"{"a":1,"a":2}"#),
            ToolArgumentIntegrity::MalformedJson
        )
    }
    #[test]
    fn diagnostic() {
        let d = ModelFailureDiagnostic::new("é".repeat(200).as_bytes());
        assert!(d.view().len() <= 256);
        assert!(std::str::from_utf8(d.view()).is_ok());
        assert!(d.view().ends_with(b"..."));
    }
}
