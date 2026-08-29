#[derive(Debug, PartialEq, Eq)]
pub enum Command {
    Interactive,
    Help,
    Version,
    Ask(Vec<String>),
    Resume(Vec<String>),
    Acp(Vec<String>),
    Login(Vec<String>),
    Logout(Vec<String>),
    Setup(Vec<String>),
    Status(Vec<String>),
    Permissions(Vec<String>),
    Mcp(Vec<String>),
    Models(Vec<String>),
    Provider(Vec<String>),
    Doctor(Vec<String>),
    Background(Vec<String>),
    Session(Vec<String>),
    Sessions(Vec<String>),
    Credits(Vec<String>),
    Usage(Vec<String>),
    Upgrade(Vec<String>),
    Replay(Vec<String>),
    Workspace(Vec<String>),
    Unknown(String),
}

pub fn parse<I>(args: I) -> Command
where
    I: IntoIterator<Item = String>,
{
    let args: Vec<String> = args.into_iter().collect();
    let Some(first) = args.first() else {
        return Command::Interactive;
    };
    if matches!(first.as_str(), "-h" | "--help") {
        return Command::Help;
    }
    if matches!(first.as_str(), "-V" | "--version") {
        return Command::Version;
    }

    let rest = args[1..].to_vec();
    match first.as_str() {
        "ask" => Command::Ask(rest),
        "resume" | "--resume" | "-r" => Command::Resume(rest),
        "acp" => Command::Acp(rest),
        "login" => Command::Login(rest),
        "logout" => Command::Logout(rest),
        "setup" => Command::Setup(rest),
        "status" => Command::Status(rest),
        "permissions" => Command::Permissions(rest),
        "mcp" => Command::Mcp(rest),
        "models" | "model" => Command::Models(rest),
        "provider" => Command::Provider(rest),
        "doctor" => Command::Doctor(rest),
        "background" => Command::Background(rest),
        "session" => Command::Session(rest),
        "sessions" => Command::Sessions(rest),
        "credits" => Command::Credits(rest),
        "usage" => Command::Usage(rest),
        "upgrade" => Command::Upgrade(rest),
        "replay" => Command::Replay(rest),
        "workspace" => Command::Workspace(rest),
        _ => Command::Unknown(first.clone()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(args: &[&str]) -> Command {
        parse(args.iter().map(|arg| (*arg).to_owned()))
    }

    #[test]
    fn no_args_starts_interactive() {
        assert_eq!(p(&[]), Command::Interactive);
    }

    #[test]
    fn ask_preserves_prompt_arguments() {
        assert_eq!(
            p(&["ask", "explain", "this"]),
            Command::Ask(vec!["explain".into(), "this".into()])
        );
    }

    #[test]
    fn resume_aliases_are_supported() {
        assert_eq!(p(&["-r", "last"]), Command::Resume(vec!["last".into()]));
        assert_eq!(p(&["--resume", "abc"]), Command::Resume(vec!["abc".into()]));
    }

    #[test]
    fn command_arguments_are_not_lost() {
        assert_eq!(p(&["mcp", "list"]), Command::Mcp(vec!["list".into()]));
    }
}
