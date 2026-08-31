//! Skill command contract (ported 1:1 from zig-reference/src/core/skills/skill_commands.zig).

/// Provider execution failure. The Zig source uses `anyerror!`, so this is a
/// type-level marker; providers replace it with their own concrete error type
/// when they wrap this contract.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SkillCommandError;

/// Owned names and backing storage returned by the provider.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct InstallResult {
    pub installed: Vec<Vec<u8>>,
}

impl InstallResult {
    pub fn deinit(&mut self) {
        self.installed.clear();
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SkillInfo<'a> {
    pub path: &'a [u8],
    pub source_label: &'a [u8],
    pub managed_install: bool,
}

pub type FindSkillFn = for<'a> fn(ctx: *mut (), name: &'a [u8]) -> Option<SkillInfo<'a>>;

pub struct CommandRequest<'a> {
    pub skills_dir: &'a [u8],
    pub find_ctx: *mut (),
    pub find_skill: FindSkillFn,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InstallCommand<'a> {
    pub source: &'a [u8],
    pub filter: Option<&'a [u8]>,
}

/// Parsed command slices borrow from the original command input.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command<'a> {
    List,
    Show(&'a [u8]),
    Install(InstallCommand<'a>),
    Create(&'a [u8]),
    Remove(&'a [u8]),
    Path,
    Usage,
}

#[derive(Debug, PartialEq, Eq)]
pub struct Notice {
    pub text: Vec<u8>,
    pub reload: bool,
}

impl Notice {
    pub fn new(text: Vec<u8>) -> Self {
        Notice { text, reload: false }
    }
}

/// Provider output. Slice payloads are owned and released by `deinit`.
#[derive(Debug, PartialEq, Eq)]
pub enum CommandResult {
    OpenMenu,
    FocusMenu(Vec<u8>),
    Notice(Notice),
    Installed(InstallResult),
}

impl CommandResult {
    /// Releases every allocation returned by a provider.
    pub fn deinit(&mut self) {
        match self {
            CommandResult::OpenMenu => {}
            CommandResult::FocusMenu(_) | CommandResult::Notice(_) => {}
            CommandResult::Installed(result) => result.deinit(),
        }
        *self = CommandResult::OpenMenu;
    }
}

pub type ParseCommandFn = for<'a> fn(rest: &'a [u8]) -> Command<'a>;
pub type ExecuteCommandFn = for<'a> fn(
    command: Command<'a>,
    request: CommandRequest<'a>,
) -> Result<CommandResult, SkillCommandError>;

/// Product behavior supplied to the Core skills command harness.
pub struct Provider {
    pub parse_command_fn: ParseCommandFn,
    pub execute_command_fn: ExecuteCommandFn,
}

impl Provider {
    pub fn parse_command<'a>(&self, rest: &'a [u8]) -> Command<'a> {
        (self.parse_command_fn)(rest)
    }

    pub fn execute_command<'a>(
        &self,
        command: Command<'a>,
        request: CommandRequest<'a>,
    ) -> Result<CommandResult, SkillCommandError> {
        (self.execute_command_fn)(command, request)
    }
}
