//! The slash-command catalog: categories for the `/help` menu, aliases,
//! ranked matching for the `/` popup, argument completion, and deciding
//! whether a line that starts with `/` is a command at all.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Category {
    General,
    Session,
    Account,
    Model,
    Appearance,
    Security,
    Workspace,
    Media,
    Agents,
    Extensions,
    Product,
}

impl Category {
    pub const ALL: [Category; 11] = [
        Category::General,
        Category::Session,
        Category::Account,
        Category::Model,
        Category::Appearance,
        Category::Security,
        Category::Workspace,
        Category::Media,
        Category::Agents,
        Category::Extensions,
        Category::Product,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Category::General => "General",
            Category::Session => "Session",
            Category::Account => "Account",
            Category::Model => "Model",
            Category::Appearance => "Appearance",
            Category::Security => "Security",
            Category::Workspace => "Workspace",
            Category::Media => "Media",
            Category::Agents => "Agents",
            Category::Extensions => "Extensions",
            Category::Product => "Product",
        }
    }
}

/// The category a command is listed under. Prompt templates, MCP prompts
/// and anything else unknown are extensions.
pub fn category(cmd: &str) -> Category {
    match cmd {
        "/help" | "/exit" | "/copy" | "/undo" | "/tools" | "/transcript" | "/keys" => {
            Category::General
        }
        "/new" | "/clear" | "/reset" | "/continue" | "/rename" | "/resume" | "/sessions"
        | "/compact" => Category::Session,
        "/login" | "/logout" | "/setup" | "/usage" | "/stats" => Category::Account,
        "/model" | "/effort" | "/thinking" | "/fast" => Category::Model,
        "/settings" | "/statusline" | "/sound" | "/notifications" => Category::Appearance,
        "/permissions" | "/allowlist" | "/yolo" | "/hooks" => Category::Security,
        "/workspace" | "/background" | "/memory" => Category::Workspace,
        "/image" | "/images" | "/paste" => Category::Media,
        "/agents" | "/skills" => Category::Agents,
        "/status" | "/version" | "/feedback" | "/trace" => Category::Product,
        _ => Category::Extensions,
    }
}

/// `(alias, command)`: other names a command answers to.
pub const ALIASES: &[(&str, &str)] = &[
    ("/?", "/help"),
    ("/quit", "/exit"),
    ("/q", "/exit"),
    ("/cost", "/usage"),
    ("/img", "/image"),
    ("/thinking", "/effort"),
];

pub fn aliases(cmd: &str) -> Vec<&'static str> {
    ALIASES
        .iter()
        .filter(|(_, c)| *c == cmd)
        .map(|(a, _)| *a)
        .collect()
}

/// The command an alias stands for (or `cmd` itself).
pub fn canonical(cmd: &str) -> &str {
    ALIASES
        .iter()
        .find(|(a, _)| *a == cmd)
        .map_or(cmd, |(_, c)| c)
}

/// How well `query` (typed after `/`) matches `name`: 0 exact, 1 prefix,
/// 2 substring; `None` for no match. Case-insensitive.
pub fn rank(name: &str, query: &str) -> Option<u8> {
    let name = name.trim_start_matches(['/', '$']).to_lowercase();
    let query = query.trim_start_matches(['/', '$']).to_lowercase();
    if name == query {
        Some(0)
    } else if name.starts_with(&query) {
        Some(1)
    } else if name.contains(&query) {
        Some(2)
    } else {
        None
    }
}

/// Commands that do nothing useful without an argument: the help menu
/// inserts them for editing instead of running them.
pub fn needs_args(cmd: &str) -> bool {
    matches!(cmd, "/image")
}

/// One completion for a command's argument: the word, what it does, and
/// whether more input must follow it.
pub type ArgOption = (&'static str, &'static str, bool);

const PERMISSIONS: &[ArgOption] = &[
    ("ask", "ask before risky tool calls", false),
    ("auto", "the classifier allows, asks or denies", false),
    ("yolo", "never ask; deny rules still block", false),
    ("rules", "list rules and session grants", false),
    ("reload", "re-read permissions.json", false),
    (
        "remember",
        "remember allow|deny TOOL ARGS-JSON for this session",
        true,
    ),
    ("revoke", "drop a remembered session rule: revoke ID", true),
    ("reset", "back to ask and forget session grants", false),
];
const SOUND: &[ArgOption] = &[
    ("on", "bell when a long turn ends or nyq waits", false),
    ("off", "no bell", false),
    ("max", "bell after every turn and prompt", false),
];
const NOTIFICATIONS: &[ArgOption] = &[
    ("on", "desktop notifications on", false),
    ("off", "desktop notifications off", false),
    ("after", "only for turns longer than SECONDS", true),
];
const STATUSLINE: &[ArgOption] = &[
    ("model", "toggle the model segment", false),
    ("provider", "toggle the provider segment", false),
    ("context", "toggle the context segment", false),
    ("tokens", "toggle the tokens segment", false),
    ("sandbox", "toggle the sandbox segment", false),
    ("folder", "toggle the folder segment", false),
    ("branch", "toggle the branch segment", false),
];
const WORKSPACE: &[ArgOption] = &[
    ("list", "list workspace directories", false),
    ("add", "add a directory: add PATH", true),
    ("remove", "remove a directory: remove PATH", true),
    ("clear", "forget saved directories", false),
];
const ALLOWLIST: &[ArgOption] = &[
    ("view", "show trusted entries [effective|local|user]", false),
    (
        "add",
        "trust a command, tool or URL: add KIND PATTERN",
        true,
    ),
    ("remove", "remove N, or remove KIND PATTERN", true),
    ("reset", "forget entries [commands|tools|urls|all]", false),
    ("local", "this workspace only: local add|remove|reset", true),
    ("user", "every workspace: user add|remove|reset", true),
];
const ALLOWLIST_SCOPED: &[ArgOption] = &[
    (
        "add",
        "trust a command, tool or URL: add KIND PATTERN",
        true,
    ),
    ("remove", "remove KIND PATTERN", true),
    ("reset", "forget entries [commands|tools|urls|all]", false),
];
const ALLOWLIST_KINDS: &[ArgOption] = &[
    ("command", "a shell command prefix", true),
    ("tool", "a tool name or pattern", true),
    ("url", "a web_fetch URL pattern", true),
];
const ALLOWLIST_RESET: &[ArgOption] = &[
    ("commands", "forget trusted commands", false),
    ("tools", "forget trusted tools", false),
    ("urls", "forget trusted URLs", false),
    ("all", "forget everything", false),
];
const ALLOWLIST_VIEW: &[ArgOption] = &[
    ("effective", "everything that applies here", false),
    ("local", "this workspace's entries", false),
    ("user", "entries for every workspace", false),
];
const SKILLS: &[ArgOption] = &[
    ("list", "list skills", false),
    ("install", "install SOURCE [--project] [--skill NAME]", true),
    ("remove", "remove an installed skill: remove NAME", true),
    ("info", "show a skill: info NAME", true),
    ("create", "create a skill: create NAME [--project]", true),
    (
        "disable",
        "turn a skill off (disabled_skills): disable NAME",
        true,
    ),
    ("enable", "turn a skill back on: enable NAME", true),
    (
        "copy",
        "copy into nyq's skills: copy NAME [--project] [--force]",
        true,
    ),
    ("path", "where skills are installed", false),
];
const TOOLS: &[ArgOption] = &[
    ("list", "every tool with its state", false),
    ("info", "description and parameters: info NAME", true),
    (
        "disable",
        "turn a tool off (disabled_tools): disable NAME",
        true,
    ),
    ("enable", "turn a tool back on: enable NAME", true),
    (
        "trust",
        "always allow it (permissions.json): trust NAME",
        true,
    ),
    ("untrust", "stop always allowing it: untrust NAME", true),
];
const MCP: &[ArgOption] = &[
    ("list", "servers, state and tools", false),
    (
        "add",
        "add NAME CMD [ARGS] or add --transport http NAME URL",
        true,
    ),
    ("remove", "remove a profile server: remove NAME", true),
    ("reload", "re-read mcp.json and connect new servers", false),
    ("path", "the profile MCP config path", false),
    ("connect", "connect a server: connect NAME", true),
    ("disconnect", "disconnect a server: disconnect NAME", true),
    ("login", "sign in to an HTTP server: login NAME", true),
    ("logout", "forget a sign-in: logout NAME", true),
    ("trust", "trust project servers", false),
    (
        "resource",
        "browse resources: resource list|templates|read|complete",
        true,
    ),
    ("prompt", "browse prompts: prompt list|get|complete", true),
];
const MCP_TRUST: &[ArgOption] = &[
    ("list", "project servers and trust", false),
    ("approve", "trust one server: approve NAME", true),
    ("reject", "stop trusting one server: reject NAME", true),
    ("approve-all", "trust every project server", false),
    ("reset", "forget trust for this project", false),
];
const MCP_RESOURCE: &[ArgOption] = &[
    ("list", "list SERVER's resources", true),
    ("templates", "list SERVER's resource templates", true),
    ("read", "read SERVER URI", true),
    ("complete", "complete SERVER URI-TEMPLATE VAR [VALUE]", true),
];
const MCP_PROMPT: &[ArgOption] = &[
    ("list", "list SERVER's prompts", true),
    ("get", "show SERVER NAME [ARGS-JSON]", true),
    ("complete", "complete SERVER NAME ARG [VALUE]", true),
];
const AGENTS: &[ArgOption] = &[
    ("list", "named subagents", false),
    ("show", "show NAME", true),
    ("stop", "stop NAME", true),
    ("delete", "delete NAME", true),
];
const IMAGES: &[ArgOption] = &[
    ("clear", "drop every pending image", false),
    ("remove", "remove N", true),
];
const BACKGROUND: &[ArgOption] = &[
    ("list", "background processes", false),
    ("logs", "head and tail of the output: logs ID|last", true),
    ("open", "open the server URL it printed: open ID|last", true),
    ("kill", "stop a process: kill ID|last", true),
];
const BACKGROUND_ID: &[ArgOption] = &[("last", "the most recent process", false)];
const HOOKS: &[ArgOption] = &[("trust", "trust the project's hooks", false)];
const EXTENSIONS: &[ArgOption] = &[
    ("list", "extensions and their state", false),
    ("reload", "restart them and pick up new ones", false),
    ("trust", "trust the project's extensions", false),
];
const MEMORY: &[ArgOption] = &[
    ("list", "saved memories", false),
    ("remove", "forget one: remove N", true),
];
const USAGE: &[ArgOption] = &[
    ("session", "this session", false),
    ("24h", "the last 24 hours", false),
    ("7d", "the last 7 days", false),
    ("30d", "the last 30 days", false),
];
const RESUME: &[ArgOption] = &[("last", "the newest conversation here", false)];
const PROVIDERS: &[ArgOption] = &[
    ("codex", "ChatGPT subscription", false),
    ("opencode-go", "OpenCode Go", false),
    ("opencode", "OpenCode Zen", false),
    ("xai", "xAI", false),
];

/// Completions for the next argument of `cmd`, given the words already
/// typed after it.
pub fn arg_options(cmd: &str, prior: &[&str]) -> &'static [ArgOption] {
    match (canonical(cmd), prior) {
        ("/permissions", []) => PERMISSIONS,
        ("/permissions", ["remember"]) => &[
            ("allow", "allow this exact call", true),
            ("deny", "deny this exact call", true),
        ],
        ("/sound", []) => SOUND,
        ("/notifications", []) => NOTIFICATIONS,
        ("/statusline", _) => STATUSLINE,
        ("/workspace", []) => WORKSPACE,
        ("/allowlist", []) => ALLOWLIST,
        ("/allowlist", ["local" | "user"]) => ALLOWLIST_SCOPED,
        ("/allowlist", ["view"]) => ALLOWLIST_VIEW,
        ("/allowlist", ["add"] | ["local" | "user", "add" | "remove"]) => ALLOWLIST_KINDS,
        ("/allowlist", ["remove"]) => ALLOWLIST_KINDS,
        ("/allowlist", ["reset"] | ["local" | "user", "reset"]) => ALLOWLIST_RESET,
        ("/skills", []) => SKILLS,
        ("/tools", []) if crate::config::debug() => TOOLS,
        ("/mcp", []) => MCP,
        ("/mcp", ["trust"]) => MCP_TRUST,
        ("/mcp", ["resource" | "resources"]) => MCP_RESOURCE,
        ("/mcp", ["prompt" | "prompts"]) => MCP_PROMPT,
        ("/agents", []) => AGENTS,
        ("/images", []) => IMAGES,
        ("/background", []) => BACKGROUND,
        ("/background", ["logs" | "open" | "kill" | "stop"]) => BACKGROUND_ID,
        ("/hooks", []) => HOOKS,
        ("/extensions", []) => EXTENSIONS,
        ("/memory", []) => MEMORY,
        ("/usage", []) => USAGE,
        ("/resume", []) => RESUME,
        ("/login" | "/logout", []) => PROVIDERS,
        _ => &[],
    }
}

/// Whether a line starting with `/` should go to the model as a prompt
/// rather than run as a command: its first word is not a command (`known`)
/// and the line has more words, or the word has another `/` in it, as in
/// `/usr/bin/foo crashes, why?` or `/tmp/x.log`.
pub fn is_prompt(line: &str, known: impl Fn(&str) -> bool) -> bool {
    let line = line.trim();
    let Some(rest) = line.strip_prefix('/') else {
        return false;
    };
    let word = rest.split_whitespace().next().unwrap_or("");
    if word.is_empty() || known(&format!("/{word}")) {
        return false;
    }
    word.contains('/') || rest.trim_end().len() > word.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_category_has_a_label_and_commands_map() {
        assert_eq!(Category::ALL.len(), 11);
        assert_eq!(category("/help"), Category::General);
        assert_eq!(category("/usage"), Category::Account);
        assert_eq!(category("/permissions"), Category::Security);
        assert_eq!(category("/my-template"), Category::Extensions);
        assert_eq!(category("/extensions"), Category::Extensions);
        assert_eq!(Category::Media.label(), "Media");
        assert_eq!(canonical("/cost"), "/usage");
        assert_eq!(canonical("/usage"), "/usage");
        assert_eq!(aliases("/exit"), ["/quit", "/q"]);
    }

    #[test]
    fn thinking_alias_discovers_the_effort_command() {
        assert_eq!(canonical("/thinking"), "/effort");
        assert_eq!(aliases("/effort"), ["/thinking"]);
        assert_eq!(category("/effort"), Category::Model);
        assert_eq!(category("/thinking"), Category::Model);
        assert!(aliases("/effort")
            .iter()
            .any(|alias| rank(alias, "think") == Some(1)));
        let known = |cmd: &str| canonical(cmd) == "/effort";
        assert!(!is_prompt("/effort high", known));
        assert!(!is_prompt("/thinking HIGH", known));
    }

    #[test]
    fn ranks_exact_then_prefix_then_substring() {
        assert_eq!(rank("/model", "/model"), Some(0));
        assert_eq!(rank("/model", "/mo"), Some(1));
        assert_eq!(rank("/model", "/ode"), Some(2));
        assert_eq!(rank("/model", "/MOD"), Some(1));
        assert_eq!(rank("/model", "/x"), None);
        assert_eq!(rank("/model", "/"), Some(1));
    }

    #[test]
    fn argument_options_follow_the_words_typed() {
        let names = |cmd: &str, prior: &[&str]| {
            arg_options(cmd, prior)
                .iter()
                .map(|o| o.0)
                .collect::<Vec<_>>()
        };
        assert!(names("/permissions", &[]).contains(&"reset"));
        assert_eq!(names("/allowlist", &["local"]), ["add", "remove", "reset"]);
        assert_eq!(
            names("/allowlist", &["user", "add"]),
            ["command", "tool", "url"]
        );
        assert_eq!(
            names("/mcp", &["resource"]),
            ["list", "templates", "read", "complete"]
        );
        assert_eq!(names("/cost", &[]), ["session", "24h", "7d", "30d"]);
        assert!(names("/permissions", &["ask"]).is_empty());
        assert!(names("/nope", &[]).is_empty());
    }

    #[test]
    fn paths_and_sentences_go_to_the_model() {
        let known = |c: &str| matches!(c, "/help" | "/model");
        assert!(is_prompt("/usr/bin/foo crashes, why?", known));
        assert!(is_prompt("/tmp/x.log", known));
        assert!(is_prompt("/what is this", known));
        assert!(!is_prompt("/unknown", known));
        assert!(!is_prompt("/help", known));
        assert!(!is_prompt("/model gpt-5", known));
        assert!(!is_prompt("/", known));
        assert!(!is_prompt("plain", known));
    }
}
