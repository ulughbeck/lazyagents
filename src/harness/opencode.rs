use std::ffi::OsString;
use std::fs;
use std::path::Path;

use anyhow::Result;
use serde_json::{json, Map, Value};

use crate::config::paths;

use super::{
    link_file, local_skills, owner_directory, AdapterSpec, Authentication, Harness, LaunchContext,
    LaunchOptions,
};

/// Global config files in the order OpenCode loads them. A later file wins.
const CONFIG_FILES: &[&str] = &["opencode.json", "opencode.jsonc"];

pub static OPENCODE: OpenCode = OpenCode;

pub struct OpenCode;

impl Harness for OpenCode {
    fn id(&self) -> &'static str {
        "opencode"
    }

    fn display_name(&self) -> &'static str {
        "OpenCode"
    }

    fn runtime_command(&self) -> &'static str {
        "opencode"
    }

    fn adapter(&self) -> Option<AdapterSpec> {
        None
    }

    fn authentication(&self) -> Option<Authentication> {
        None
    }

    fn always_allow_is_session_only(&self) -> bool {
        true
    }

    fn launch_options(&self, context: LaunchContext<'_>) -> Result<LaunchOptions> {
        let agent_paths = paths(context.root);
        let runtime = agent_paths.runtime.join(self.id());
        let config_home = runtime.join("config");
        let data_home = runtime.join("data");
        let state_home = runtime.join("state");
        fs::create_dir_all(&config_home)?;
        fs::create_dir_all(data_home.join("opencode"))?;
        fs::create_dir_all(&state_home)?;
        let authentication = owner_directory("XDG_DATA_HOME", ".local/share")
            .map(|data| data.join("opencode/auth.json"));
        link_file(authentication, &data_home.join("opencode/auth.json"))?;

        let skills = local_skills(&agent_paths.skills)?;
        let mut config = isolated_config(context.instruction, &agent_paths.skills, &skills);
        if let (Some(config), Some(directory)) = (
            config.as_object_mut(),
            owner_directory("XDG_CONFIG_HOME", ".config").map(|path| path.join("opencode")),
        ) {
            config.extend(provider_config(&directory));
        }
        // OpenCode 2 runs the ACP server in the process working directory, which is the agent root.
        let mut options = LaunchOptions {
            args: vec![OsString::from("acp")],
            ..LaunchOptions::default()
        };
        options
            .env
            .insert("XDG_CONFIG_HOME".into(), config_home.display().to_string());
        options
            .env
            .insert("XDG_DATA_HOME".into(), data_home.display().to_string());
        options
            .env
            .insert("XDG_STATE_HOME".into(), state_home.display().to_string());
        // Project config would also load opencode.json, .opencode, .claude and .agents
        // from every parent folder. The generated config adds the agent's own skills.
        options
            .env
            .insert("OPENCODE_DISABLE_PROJECT_CONFIG".into(), "1".into());
        options
            .env
            .insert("OPENCODE_DISABLE_CLAUDE_CODE".into(), "1".into());
        options.env.insert(
            "OPENCODE_CONFIG_CONTENT".into(),
            serde_json::to_string(&config)?,
        );
        Ok(options)
    }
}

fn provider_config(directory: &Path) -> Map<String, Value> {
    let documents = CONFIG_FILES
        .iter()
        .filter_map(|name| fs::read_to_string(directory.join(name)).ok())
        .collect::<Vec<_>>();
    merge_provider_settings(directory, documents.iter().map(String::as_str))
}

/// Merges the provider settings of the given JSONC documents and skips the rest.
fn merge_provider_settings<'a>(
    directory: &Path,
    documents: impl IntoIterator<Item = &'a str>,
) -> Map<String, Value> {
    let mut carried = Map::new();
    for contents in documents {
        let Ok(Some(Value::Object(document))) =
            jsonc_parser::parse_to_serde_value(contents, &Default::default())
        else {
            continue;
        };
        for (key, mut value) in document {
            match key.as_str() {
                "provider" | "providers" if value.is_object() => {}
                "enabled_providers" | "disabled_providers" if value.is_array() => {}
                "policies" => {
                    let Value::Array(policies) = value else {
                        continue;
                    };
                    let policies = policies
                        .into_iter()
                        .filter(|policy| policy["action"] == "provider.use")
                        .collect::<Vec<_>>();
                    if policies.is_empty() {
                        continue;
                    }
                    value = Value::Array(policies);
                }
                _ => continue,
            }
            absolute_file_references(&mut value, directory);
            match (carried.get_mut(&key), value) {
                (Some(Value::Object(current)), Value::Object(next)) => merge(current, next),
                // OpenCode adds the denials of every file and keeps the last allow list.
                (Some(Value::Array(current)), Value::Array(next)) if key != "enabled_providers" => {
                    current.extend(next)
                }
                (_, value) => {
                    carried.insert(key, value);
                }
            }
        }
    }
    carried
}

fn merge(current: &mut Map<String, Value>, next: Map<String, Value>) {
    for (key, value) in next {
        match (current.get_mut(&key), value) {
            (Some(Value::Object(current)), Value::Object(next)) => merge(current, next),
            (_, value) => {
                current.insert(key, value);
            }
        }
    }
}

/// OpenCode resolves a relative `{file:path}` from the directory of the file
/// that contains it. Generated config has no such directory.
fn absolute_file_references(value: &mut Value, directory: &Path) {
    match value {
        Value::String(text) if text.contains("{file:") => {
            let mut result = String::new();
            let mut rest = text.as_str();
            while let Some(start) = rest.find("{file:") {
                let (before, reference) = rest.split_at(start + "{file:".len());
                result.push_str(before);
                let Some(end) = reference.find('}') else {
                    rest = reference;
                    break;
                };
                let path = &reference[..end];
                if path.starts_with('/') || path.starts_with("~/") {
                    result.push_str(path);
                } else {
                    result.push_str(&directory.join(path).display().to_string());
                }
                rest = &reference[end..];
            }
            result.push_str(rest);
            *text = result;
        }
        Value::Array(values) => values
            .iter_mut()
            .for_each(|value| absolute_file_references(value, directory)),
        Value::Object(values) => values
            .values_mut()
            .for_each(|value| absolute_file_references(value, directory)),
        _ => {}
    }
}

fn isolated_config(instruction: &str, skills_directory: &Path, local_skills: &[String]) -> Value {
    let mut skill_permissions = Map::new();
    skill_permissions.insert("*".into(), Value::String("deny".into()));
    for skill in local_skills {
        skill_permissions.insert(skill.clone(), Value::String("allow".into()));
    }
    json!({
        "autoupdate": false,
        "share": "disabled",
        "plugin": [],
        // OpenCode 1 reads this object form, and OpenCode 2 migrates it.
        "skills": {"paths": [skills_directory]},
        "default_agent": "lazyagents",
        "agent": {
            "lazyagents": {
                "description": "The local lazyagents agent",
                "mode": "primary",
                "prompt": instruction,
                "permission": {
                    "*": "ask",
                    "read": "allow",
                    "glob": "allow",
                    "grep": "allow",
                    "list": "allow",
                    "lsp": "allow",
                    "skill": skill_permissions
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::*;

    #[test]
    fn launch_uses_native_acp_and_an_isolated_runtime() {
        let root = tempdir().unwrap();
        let skills = paths(root.path()).skills;
        fs::create_dir_all(skills.join("writer")).unwrap();
        fs::write(
            skills.join("writer/SKILL.md"),
            "---\nname: writer\ndescription: Write\n---\n",
        )
        .unwrap();

        let options = OPENCODE
            .launch_options(LaunchContext {
                root: root.path(),
                runtime_path: std::path::Path::new("/runtime/opencode"),
                instruction: "You are a writer.",
            })
            .unwrap();

        assert_eq!(options.args, ["acp"]);
        assert!(options.system_prompt.is_none());
        assert!(options.env["XDG_CONFIG_HOME"].contains(".agents/runtime/opencode/config"));
        assert_eq!(options.env["OPENCODE_DISABLE_PROJECT_CONFIG"], "1");
        assert_eq!(options.env["OPENCODE_DISABLE_CLAUDE_CODE"], "1");
        let config: Value = serde_json::from_str(&options.env["OPENCODE_CONFIG_CONTENT"]).unwrap();
        assert_eq!(config["skills"]["paths"], json!([skills]));
        assert_eq!(config["agent"]["lazyagents"]["prompt"], "You are a writer.");
        assert_eq!(
            config["agent"]["lazyagents"]["permission"]["skill"]["*"],
            "deny"
        );
        assert_eq!(
            config["agent"]["lazyagents"]["permission"]["skill"]["writer"],
            "allow"
        );
    }

    #[test]
    fn carries_provider_settings_from_both_global_files() {
        let directory = Path::new("/owner/opencode");
        let json = r#"{
            "provider": {
                "gateway": {
                    "options": {"baseURL": "http://old", "apiKey": "{env:GATEWAY_KEY}"},
                    "models": {"small": {}}
                }
            },
            "disabled_providers": ["openai"],
            "mcp": {"tools": {"type": "local", "command": ["tools"]}},
            "plugin": ["opencode-writer"]
        }"#;
        let jsonc = r#"{
            // The later file wins.
            "provider": {
                "gateway": {
                    "options": {"baseURL": "http://127.0.0.1:4000/v1", "headers": {"X-Key": "{file:./key.txt}"}},
                },
            },
            "disabled_providers": ["anthropic"],
            "policies": [
                {"action": "provider.use", "resource": "google", "effect": "deny"},
                {"action": "tool.use", "resource": "*", "effect": "allow"},
            ],
            "agent": {"build": {"prompt": "owner prompt"}},
        }"#;

        let config = merge_provider_settings(directory, ["{", json, "[]", jsonc]);

        assert_eq!(
            Value::Object(config),
            json!({
                "provider": {
                    "gateway": {
                        "options": {
                            "baseURL": "http://127.0.0.1:4000/v1",
                            "apiKey": "{env:GATEWAY_KEY}",
                            "headers": {"X-Key": "{file:/owner/opencode/./key.txt}"}
                        },
                        "models": {"small": {}}
                    }
                },
                "disabled_providers": ["openai", "anthropic"],
                "policies": [{"action": "provider.use", "resource": "google", "effect": "deny"}]
            })
        );
    }

    #[test]
    fn file_references_keep_absolute_and_home_paths() {
        let mut value = json!(["{file:/etc/key} {file:~/key}", "{file:keys/a}{file:b"]);
        absolute_file_references(&mut value, Path::new("/owner"));
        assert_eq!(
            value,
            json!([
                "{file:/etc/key} {file:~/key}",
                "{file:/owner/keys/a}{file:b"
            ])
        );
    }
}
