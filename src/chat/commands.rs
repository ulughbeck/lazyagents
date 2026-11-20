use crate::acp::AgentCommand;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command {
    Model,
    Permissions,
    New,
    Status,
    Quit,
}

pub struct CommandSpec {
    pub command: Command,
    pub name: &'static str,
    pub description: &'static str,
}

pub const COMMANDS: [CommandSpec; 5] = [
    CommandSpec {
        command: Command::Model,
        name: "/model",
        description: "select model and reasoning level",
    },
    CommandSpec {
        command: Command::Permissions,
        name: "/permissions",
        description: "select what is allowed to do",
    },
    CommandSpec {
        command: Command::New,
        name: "/new",
        description: "start new conversation",
    },
    CommandSpec {
        command: Command::Status,
        name: "/status",
        description: "show session status",
    },
    CommandSpec {
        command: Command::Quit,
        name: "/quit",
        description: "exit",
    },
];

impl Command {
    pub fn parse(text: &str) -> Option<Self> {
        if text == "/exit" {
            return Some(Self::Quit);
        }
        COMMANDS
            .iter()
            .find(|entry| entry.name == text)
            .map(|entry| entry.command)
    }
}

/// One entry of the `/` menu.
#[derive(Clone, Debug, PartialEq)]
pub struct MenuEntry {
    /// With the leading slash.
    pub name: String,
    pub description: String,
    /// Completion adds a space, so that the user can type the input.
    pub takes_input: bool,
}

impl MenuEntry {
    pub fn completion(&self) -> String {
        if self.takes_input {
            format!("{} ", self.name)
        } else {
            self.name.clone()
        }
    }
}

/// Local commands, then agent commands. A local command wins when names collide.
pub fn menu(agent: &[AgentCommand]) -> Vec<MenuEntry> {
    let local = COMMANDS.iter().map(|command| MenuEntry {
        name: command.name.to_owned(),
        description: command.description.to_owned(),
        takes_input: false,
    });
    let agent = agent
        .iter()
        .map(|command| {
            let description = match &command.hint {
                Some(hint) => format!("{} ({hint})", command.description),
                None => command.description.clone(),
            };
            MenuEntry {
                name: format!("/{}", command.name),
                description: crate::markdown::sanitize(&description)
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" "),
                takes_input: command.hint.is_some(),
            }
        })
        .filter(|entry| Command::parse(&entry.name).is_none());
    local.chain(agent).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_commands_win_over_agent_commands() {
        let command = |name: &str, hint: Option<&str>| AgentCommand {
            name: name.into(),
            description: format!("agent {name}"),
            hint: hint.map(str::to_owned),
        };
        let entries = menu(&[
            command("status", None),
            command("exit", None),
            command("web", Some("query")),
            command("compact", None),
        ]);
        let names = entries
            .iter()
            .map(|entry| entry.name.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            names,
            [
                "/model",
                "/permissions",
                "/new",
                "/status",
                "/quit",
                "/web",
                "/compact"
            ]
        );
        assert_eq!(entries[3].description, "show session status");
        assert_eq!(entries[5].description, "agent web (query)");
        assert_eq!(entries[5].completion(), "/web ");
        assert_eq!(entries[6].completion(), "/compact");
    }
}
