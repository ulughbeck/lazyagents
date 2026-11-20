use std::fs;

use crate::config::paths;
use anyhow::Result;
use serde_json::{Map, Value};

use super::{
    any_variable_set, link_file, owner_directory, AdapterSpec, Authentication, Harness,
    LaunchContext, LaunchOptions,
};

pub static CODEX: Codex = Codex;

pub struct Codex;

impl Harness for Codex {
    fn id(&self) -> &'static str {
        "codex"
    }

    fn display_name(&self) -> &'static str {
        "Codex"
    }

    fn runtime_command(&self) -> &'static str {
        "codex"
    }

    fn adapter(&self) -> Option<AdapterSpec> {
        Some(AdapterSpec {
            package: "@agentclientprotocol/codex-acp",
            min_version: "2.6.0",
            next_major: "3.0.0",
            binary: "codex-acp",
        })
    }

    fn authentication(&self) -> Option<Authentication> {
        Some(Authentication {
            status_args: &["login", "status"],
            login_args: &["login"],
            has_provider_credentials: || {
                any_variable_set(&["CODEX_API_KEY", "OPENAI_API_KEY"])
                    || owner_provider_config()
                        .get("model_provider")
                        .and_then(Value::as_str)
                        .is_some_and(|provider| provider != "openai")
            },
        })
    }

    fn launch_options(&self, context: LaunchContext<'_>) -> Result<LaunchOptions> {
        let agent_paths = paths(context.root);
        let runtime = agent_paths.runtime.join(self.id());
        let private_home = runtime.join("home");
        fs::create_dir_all(&private_home)?;
        let authentication =
            owner_directory("CODEX_HOME", ".codex").map(|home| home.join("auth.json"));
        link_file(authentication, &runtime.join("auth.json"))?;

        let mut options = LaunchOptions::default();
        options
            .env
            .insert("CODEX_HOME".into(), runtime.display().to_string());
        options
            .env
            .insert("HOME".into(), private_home.display().to_string());
        options.env.insert(
            "CODEX_PATH".into(),
            context.runtime_path.display().to_string(),
        );
        let provider = owner_provider_config();
        if !provider.is_empty() {
            // codex-acp passes these to `codex app-server` and to each new thread.
            options
                .env
                .insert("CODEX_CONFIG".into(), Value::Object(provider).to_string());
        }
        options.system_prompt = Some(context.instruction.to_owned());
        Ok(options)
    }
}

fn owner_provider_config() -> Map<String, Value> {
    owner_directory("CODEX_HOME", ".codex")
        .and_then(|home| fs::read_to_string(home.join("config.toml")).ok())
        .map(|contents| provider_config(&contents))
        .unwrap_or_default()
}

/// The effective provider, the provider definitions, and the OpenAI base URL.
/// A key in the active profile replaces the top-level key.
fn provider_config(contents: &str) -> Map<String, Value> {
    let Ok(config) = contents.parse::<toml::Table>() else {
        return Map::new();
    };
    let profile = config
        .get("profile")
        .and_then(toml::Value::as_str)
        .and_then(|name| config.get("profiles")?.get(name));
    let mut carried = Map::new();
    for key in ["model_provider", "openai_base_url"] {
        let value = profile
            .and_then(|profile| profile.get(key))
            .or_else(|| config.get(key));
        if let Some(value) = value.and_then(|value| serde_json::to_value(value).ok()) {
            carried.insert(key.into(), value);
        }
    }
    if let Some(providers) = config
        .get("model_providers")
        .filter(|providers| providers.is_table())
        .and_then(|providers| serde_json::to_value(providers).ok())
    {
        carried.insert("model_providers".into(), providers);
    }
    carried
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::*;

    #[test]
    fn launch_uses_an_isolated_runtime_and_the_standard_system_prompt() {
        let root = tempdir().unwrap();
        let runtime = std::path::Path::new("/runtime/codex");
        let options = CODEX
            .launch_options(LaunchContext {
                root: root.path(),
                runtime_path: runtime,
                instruction: "You are a reviewer.",
            })
            .unwrap();

        assert_eq!(options.env["CODEX_PATH"], runtime.display().to_string());
        assert!(options.env["CODEX_HOME"].contains(".agents/runtime/codex"));
        assert!(options.env["HOME"].contains(".agents/runtime/codex/home"));
        assert_eq!(
            options.system_prompt.as_deref(),
            Some("You are a reviewer.")
        );
        assert!(options.session_meta.is_none());
    }

    #[test]
    fn carries_only_the_provider_of_the_active_profile() {
        let config = provider_config(
            r#"
model = "gpt-5"
model_provider = "openai"
openai_base_url = "https://proxy.example/v1"
profile = "local"

[profiles.local]
model_provider = "gateway"

[model_providers.gateway]
name = "Gateway"
base_url = "http://127.0.0.1:4000/v1"
env_key = "GATEWAY_KEY"

[mcp_servers.tools]
command = "tools"

[plugins.writer]
enabled = true
"#,
        );

        assert_eq!(
            Value::Object(config),
            serde_json::json!({
                "model_provider": "gateway",
                "openai_base_url": "https://proxy.example/v1",
                "model_providers": {
                    "gateway": {
                        "name": "Gateway",
                        "base_url": "http://127.0.0.1:4000/v1",
                        "env_key": "GATEWAY_KEY"
                    }
                }
            })
        );
    }

    #[test]
    fn malformed_or_unrelated_config_carries_nothing() {
        assert!(provider_config("model_provider = ").is_empty());
        assert!(
            provider_config("model = \"gpt-5\"\n[mcp_servers.tools]\ncommand = \"x\"").is_empty()
        );
    }
}
