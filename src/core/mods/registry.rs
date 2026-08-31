//! Tool and command registries keyed by stable public names
//! (ported 1:1 from zig-reference/src/core/mods/registry.zig).

/// Entries that expose a stable public tool name.
pub trait ToolName {
    fn name(&self) -> &str;
}

/// Registry for tool-like capabilities keyed by stable public name.
pub struct ToolRegistry<'a, T: ToolName> {
    pub tools: &'a [T],
}

impl<'a, T: ToolName> ToolRegistry<'a, T> {
    /// Looks up a tool by name. Duplicate registry names are a programmer bug.
    pub fn lookup(&self, name: &str) -> Option<&'a T> {
        self.tools.iter().find(|tool| tool.name() == name)
    }
}

/// Entries that expose a public command token and aliases.
pub trait CommandSpec {
    fn command(&self) -> &str;
    fn aliases(&self) -> &[&str];
}

/// Registry for command-like capabilities keyed by a public command token and aliases.
pub struct CommandRegistry<'a, C: CommandSpec> {
    pub commands: &'a [C],
}

impl<'a, C: CommandSpec> CommandRegistry<'a, C> {
    pub fn lookup(&self, command: &str) -> Option<&'a C> {
        self.match_exact(command).map(|matched| matched.command)
    }

    pub fn match_exact(&self, command: &str) -> Option<Match<'a, C>> {
        for entry in self.commands {
            if matches_command_token(command, entry.command()) {
                return Some(Match {
                    command: entry,
                    token: entry.command(),
                });
            }
            for alias in entry.aliases() {
                if matches_command_token(command, alias) {
                    return Some(Match {
                        command: entry,
                        token: alias,
                    });
                }
            }
        }
        None
    }

    pub fn match_entry_prefix(&self, command: &str, entry: &C) -> Option<&'a str> {
        for candidate in self.commands {
            if !std::ptr::eq(candidate, entry) {
                continue;
            }
            if starts_with_command_prefix(command, candidate.command()) {
                return Some(candidate.command());
            }
            for alias in candidate.aliases() {
                if starts_with_command_prefix(command, alias) {
                    return Some(alias);
                }
            }
            return None;
        }
        None
    }
}

pub struct Match<'a, C: CommandSpec> {
    pub command: &'a C,
    pub token: &'a str,
}

fn matches_command_token(input: &str, command: &str) -> bool {
    if input == command {
        return true;
    }
    if input.is_empty() {
        return false;
    }
    let last = input.as_bytes()[input.len() - 1];
    if last != b' ' && last != b'\t' {
        return false;
    }
    let trimmed = input.trim_end_matches([' ', '\t']);
    trimmed == command
}

fn starts_with_command_prefix(input: &str, command: &str) -> bool {
    if !input.starts_with(command) {
        return false;
    }
    input.len() == command.len()
        || matches!(input.as_bytes()[command.len()], b' ' | b'\t')
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestTool {
        name: &'static str,
    }

    impl ToolName for TestTool {
        fn name(&self) -> &str {
            self.name
        }
    }

    #[test]
    fn tool_registry_lookup_returns_hit_and_miss() {
        let tools = [
            TestTool { name: "read_file" },
            TestTool { name: "run_command" },
        ];
        let registry = ToolRegistry { tools: &tools };

        let found = registry.lookup("run_command").unwrap();
        assert_eq!("run_command", found.name);
        assert!(registry.lookup("missing").is_none());
    }

    struct TestCommand {
        command: &'static str,
        aliases: &'static [&'static str],
    }

    impl CommandSpec for TestCommand {
        fn command(&self) -> &str {
            self.command
        }

        fn aliases(&self) -> &[&str] {
            self.aliases
        }
    }

    #[test]
    fn command_registry_lookup_and_prefix_matching_use_command_aliases() {
        let commands = [
            TestCommand {
                command: "/model",
                aliases: &["/m"],
            },
            TestCommand {
                command: "/models",
                aliases: &[],
            },
        ];
        let registry = CommandRegistry { commands: &commands };

        let alias = registry.lookup("/m  ").unwrap();
        assert_eq!("/model", alias.command);

        let prefixed = registry.match_entry_prefix("/m model-id", alias).unwrap();
        assert_eq!("/m", prefixed);

        assert!(registry.match_entry_prefix("/m\nmodel-id", alias).is_none());
        assert!(registry.lookup("/missing").is_none());
    }

    #[test]
    fn command_registry_matches_a_specific_entry_prefix_when_commands_overlap() {
        let commands = [
            TestCommand {
                command: "/background",
                aliases: &[],
            },
            TestCommand {
                command: "/background stop",
                aliases: &[],
            },
        ];
        let registry = CommandRegistry { commands: &commands };

        let matched = registry
            .match_entry_prefix("/background stop last", &commands[1])
            .unwrap();
        assert_eq!("/background stop", matched);

        let external = TestCommand {
            command: "/background stop",
            aliases: &[],
        };
        assert!(registry
            .match_entry_prefix("/background stop last", &external)
            .is_none());
    }
}
