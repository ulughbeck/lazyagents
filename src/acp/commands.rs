use serde::Deserialize;
use serde_json::Value;

use super::Client;

/// A slash command that the agent advertises. The user runs it with a prompt
/// that starts with `/name`, optionally followed by whitespace and input text.
#[derive(Clone, Debug, PartialEq)]
pub struct AgentCommand {
    /// Without the leading slash.
    pub name: String,
    pub description: String,
    /// Shown when the command takes input.
    pub hint: Option<String>,
}

#[derive(Deserialize)]
struct RawCommand {
    name: String,
    description: Option<String>,
    #[serde(default)]
    input: Value,
}

impl Client {
    /// The latest commands from `available_commands_update`.
    pub fn commands(&self) -> &[AgentCommand] {
        &self.commands
    }

    /// Replaces the commands with a complete list from the agent.
    pub(super) fn set_commands(&mut self, commands: &[Value]) {
        self.commands = parse(commands);
    }
}

/// Skips malformed entries, repeated names, and names that a typed prompt
/// cannot end at, because the input starts after whitespace.
fn parse(commands: &[Value]) -> Vec<AgentCommand> {
    let mut parsed: Vec<AgentCommand> = Vec::new();
    for command in commands
        .iter()
        .filter_map(|value| RawCommand::deserialize(value).ok())
    {
        if command.name.is_empty()
            || command
                .name
                .chars()
                .any(|character| character.is_whitespace() || character.is_control())
            || parsed.iter().any(|known| known.name == command.name)
        {
            continue;
        }
        parsed.push(AgentCommand {
            hint: command
                .input
                .get("hint")
                .and_then(Value::as_str)
                .filter(|hint| !hint.trim().is_empty())
                .map(str::to_owned),
            name: command.name,
            description: command.description.unwrap_or_default(),
        });
    }
    parsed
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn keeps_commands_that_can_be_typed() {
        let commands = parse(&[
            json!({"name": "web", "description": "Search", "input": {"hint": "query"}}),
            json!({"name": "$skill", "description": "Skill", "input": null}),
            json!({"name": "web", "description": "Repeated"}),
            json!({"name": "two words", "description": "Space"}),
            json!({"name": "", "description": "Empty"}),
            json!({"description": "No name"}),
        ]);
        assert_eq!(
            commands,
            [
                AgentCommand {
                    name: "web".into(),
                    description: "Search".into(),
                    hint: Some("query".into()),
                },
                AgentCommand {
                    name: "$skill".into(),
                    description: "Skill".into(),
                    hint: None,
                },
            ]
        );
    }
}
