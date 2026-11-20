use std::collections::BTreeMap;
use std::fs;

use anyhow::Result;
use serde_json::{json, Value};

use crate::config::paths;

use super::{
    any_variable_set, local_skills, owner_directory, AdapterSpec, Authentication, Harness,
    LaunchContext, LaunchOptions,
};

/// Platforms selected with `CLAUDE_CODE_USE_<name>` and `CLAUDE_CODE_SKIP_<name>_AUTH`.
const PLATFORMS: &[&str] = &[
    "BEDROCK",
    "VERTEX",
    "FOUNDRY",
    "MANTLE",
    "ANTHROPIC_AWS",
    "ANTHROPIC_GOOGLE_CLOUD",
];

/// Variables that give Claude Code credentials or a provider without a sign-in.
const CREDENTIAL_VARIABLES: &[&str] = &[
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_AUTH_TOKEN",
    "CLAUDE_CODE_OAUTH_TOKEN",
    "CLAUDE_CODE_USE_BEDROCK",
    "CLAUDE_CODE_USE_VERTEX",
    "CLAUDE_CODE_USE_FOUNDRY",
    "CLAUDE_CODE_USE_MANTLE",
    "CLAUDE_CODE_USE_ANTHROPIC_AWS",
    "CLAUDE_CODE_USE_ANTHROPIC_GOOGLE_CLOUD",
];

pub static CLAUDE: Claude = Claude;

pub struct Claude;

impl Harness for Claude {
    fn id(&self) -> &'static str {
        "claude"
    }

    fn display_name(&self) -> &'static str {
        "Claude Code"
    }

    fn runtime_command(&self) -> &'static str {
        "claude"
    }

    fn adapter(&self) -> Option<AdapterSpec> {
        Some(AdapterSpec {
            package: "@agentclientprotocol/claude-agent-acp",
            min_version: "0.107.0",
            next_major: "1.0.0",
            binary: "claude-agent-acp",
        })
    }

    fn authentication(&self) -> Option<Authentication> {
        Some(Authentication {
            status_args: &["auth", "status"],
            login_args: &["auth", "login"],
            has_provider_credentials: || {
                any_variable_set(CREDENTIAL_VARIABLES)
                    || owner_provider_settings().has_credentials()
            },
        })
    }

    fn launch_options(&self, context: LaunchContext<'_>) -> Result<LaunchOptions> {
        let skills = local_skills(&paths(context.root).skills)?;
        let provider = owner_provider_settings();
        let mut options = json!({
            "settingSources": ["project"],
            "skills": skills,
            "env": {
                "CLAUDE_CODE_DISABLE_AUTO_MEMORY": "1",
                "CLAUDE_CODE_DISABLE_BUNDLED_SKILLS": "1"
            }
        });
        if let Some(helper) = provider.api_key_helper {
            options["settings"] = json!({ "apiKeyHelper": helper });
        }
        Ok(LaunchOptions {
            system_prompt: Some(context.instruction.to_owned()),
            // The adapter process environment has the lowest precedence, so the
            // agent's own `.claude/settings.json` can still replace these values.
            env: provider.env,
            session_meta: Some(json!({ "claudeCode": { "options": options } })),
            ..LaunchOptions::default()
        })
    }
}

/// Provider routing taken from the owner's `settings.json`. User settings are
/// not loaded, so nothing else from that file reaches the agent.
#[derive(Debug, Default, PartialEq)]
struct ProviderSettings {
    env: BTreeMap<String, String>,
    api_key_helper: Option<String>,
}

impl ProviderSettings {
    fn has_credentials(&self) -> bool {
        self.api_key_helper.is_some()
            || CREDENTIAL_VARIABLES
                .iter()
                .any(|name| self.env.contains_key(*name))
    }
}

fn owner_provider_settings() -> ProviderSettings {
    owner_directory("CLAUDE_CONFIG_DIR", ".claude")
        .and_then(|directory| fs::read_to_string(directory.join("settings.json")).ok())
        .map(|contents| provider_settings(&contents))
        .unwrap_or_default()
}

fn provider_settings(contents: &str) -> ProviderSettings {
    let Ok(settings) = serde_json::from_str::<Value>(contents) else {
        return ProviderSettings::default();
    };
    let env = settings["env"]
        .as_object()
        .into_iter()
        .flatten()
        .filter(|(name, _)| is_provider_variable(name))
        .filter_map(|(name, value)| {
            let value = match value {
                Value::String(value) => value.clone(),
                Value::Number(_) | Value::Bool(_) => value.to_string(),
                _ => return None,
            };
            Some((name.clone(), value))
        })
        .collect();
    let api_key_helper = settings["apiKeyHelper"]
        .as_str()
        .filter(|helper| !helper.trim().is_empty())
        .map(str::to_owned);
    ProviderSettings {
        env,
        api_key_helper,
    }
}

/// Endpoint, credential, platform, region, and model name variables.
fn is_provider_variable(name: &str) -> bool {
    let platform = name.strip_prefix("CLAUDE_CODE_USE_").or_else(|| {
        name.strip_prefix("CLAUDE_CODE_SKIP_")
            .and_then(|rest| rest.strip_suffix("_AUTH"))
    });
    name.starts_with("ANTHROPIC_")
        || name.starts_with("AWS_")
        || name.starts_with("VERTEX_REGION_")
        || matches!(
            name,
            "CLOUD_ML_REGION" | "CLAUDE_CODE_OAUTH_TOKEN" | "CLAUDE_CODE_API_KEY_HELPER_TTL_MS"
        )
        || platform.is_some_and(|platform| PLATFORMS.contains(&platform))
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::*;

    #[test]
    fn launch_appends_instructions_and_selects_only_agent_skills() {
        let root = tempdir().unwrap();
        let skills = paths(root.path()).skills;
        fs::create_dir_all(skills.join("writer")).unwrap();
        fs::write(skills.join("writer/SKILL.md"), "# Writer").unwrap();
        fs::create_dir_all(skills.join("empty")).unwrap();

        let options = CLAUDE
            .launch_options(LaunchContext {
                root: root.path(),
                runtime_path: std::path::Path::new("/runtime/claude"),
                instruction: "You are a writer.",
            })
            .unwrap();
        let meta = options.session_meta.unwrap();

        assert_eq!(options.system_prompt.as_deref(), Some("You are a writer."));
        assert!(meta.get("systemPrompt").is_none());
        assert_eq!(meta["claudeCode"]["options"]["skills"], json!(["writer"]));
    }

    #[test]
    fn carries_only_provider_routing_settings() {
        let settings = provider_settings(
            r#"{
                "env": {
                    "ANTHROPIC_BASE_URL": "http://127.0.0.1:4000",
                    "ANTHROPIC_AUTH_TOKEN": "token",
                    "ANTHROPIC_DEFAULT_SONNET_MODEL": "gateway-sonnet",
                    "CLAUDE_CODE_USE_BEDROCK": 1,
                    "CLAUDE_CODE_SKIP_BEDROCK_AUTH": "1",
                    "AWS_REGION": "us-east-1",
                    "CLAUDE_CODE_USE_POWERSHELL_TOOL": "1",
                    "CLAUDE_CODE_SKIP_PROMPT_HISTORY": "1",
                    "DISABLE_TELEMETRY": "1",
                    "GITHUB_TOKEN": "secret"
                },
                "apiKeyHelper": "~/bin/claude-key",
                "mcpServers": {"tools": {"command": "tools"}},
                "enabledPlugins": {"writer@market": true},
                "permissions": {"allow": ["Bash"]}
            }"#,
        );

        assert_eq!(
            settings.env.keys().map(String::as_str).collect::<Vec<_>>(),
            [
                "ANTHROPIC_AUTH_TOKEN",
                "ANTHROPIC_BASE_URL",
                "ANTHROPIC_DEFAULT_SONNET_MODEL",
                "AWS_REGION",
                "CLAUDE_CODE_SKIP_BEDROCK_AUTH",
                "CLAUDE_CODE_USE_BEDROCK",
            ]
        );
        assert_eq!(settings.env["CLAUDE_CODE_USE_BEDROCK"], "1");
        assert_eq!(settings.api_key_helper.as_deref(), Some("~/bin/claude-key"));
        assert!(settings.has_credentials());
    }

    #[test]
    fn malformed_or_unrelated_settings_carry_nothing() {
        for contents in ["{", "[]", r#"{"env": "x", "apiKeyHelper": ""}"#] {
            assert_eq!(provider_settings(contents), ProviderSettings::default());
        }
        let model_only = provider_settings(r#"{"env": {"ANTHROPIC_MODEL": "opus"}}"#);
        assert!(!model_only.has_credentials());
    }
}
