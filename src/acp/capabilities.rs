use anyhow::{bail, Result};
use serde_json::Value;

pub(super) const PROTOCOL_VERSION: u64 = 1;

/// What the agent supports, from its `initialize` result.
#[derive(Debug, Default)]
pub(super) struct Capabilities {
    load_session: bool,
    resume_session: bool,
    close_session: bool,
    system_prompt: bool,
}

impl Capabilities {
    /// Reads an `initialize` result and rejects other protocol versions.
    /// A field with an unexpected shape means that the agent does not support it.
    pub(super) fn parse(result: &Value) -> Result<Self> {
        match result.get("protocolVersion") {
            // Agents that omit the version are treated as version 1.
            None | Some(Value::Null) => {}
            Some(version) if version.as_u64() == Some(PROTOCOL_VERSION) => {}
            Some(version) => bail!(
                "the agent uses ACP protocol version {version}, but lazyagents supports only version {PROTOCOL_VERSION}"
            ),
        }
        let session = |name: &str| {
            result
                .pointer(&format!("/agentCapabilities/sessionCapabilities/{name}"))
                .is_some_and(Value::is_object)
        };
        Ok(Self {
            load_session: result.pointer("/agentCapabilities/loadSession")
                == Some(&Value::Bool(true)),
            resume_session: session("resume"),
            close_session: session("close"),
            system_prompt: session("systemPrompt"),
        })
    }

    pub(super) fn load_session(&self) -> bool {
        self.load_session
    }

    pub(super) fn close_session(&self) -> bool {
        self.close_session
    }

    /// The agent accepts `systemPrompt` in append mode.
    pub(super) fn system_prompt(&self) -> bool {
        self.system_prompt
    }

    /// The method that continues a saved session, preferring `session/resume`.
    pub(super) fn continuation_method(&self) -> Option<&'static str> {
        if self.resume_session {
            Some("session/resume")
        } else if self.load_session {
            Some("session/load")
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn selects_the_advertised_continuation_method() {
        let method = |result| Capabilities::parse(&result).unwrap().continuation_method();
        assert_eq!(
            method(json!({"agentCapabilities": {"sessionCapabilities": {"resume": {}}}})),
            Some("session/resume")
        );
        assert_eq!(
            method(json!({"agentCapabilities": {"loadSession": true}})),
            Some("session/load")
        );
        assert_eq!(method(json!({})), None);
    }

    #[test]
    fn treats_null_and_wrong_typed_capabilities_as_unsupported() {
        for result in [
            json!(null),
            json!({"protocolVersion": null, "agentCapabilities": null}),
            json!({"agentCapabilities": {"loadSession": null, "sessionCapabilities": null}}),
            json!({"agentCapabilities": {"loadSession": "true", "sessionCapabilities": {
                "resume": true, "close": null, "systemPrompt": []
            }}}),
            json!({"agentCapabilities": {"sessionCapabilities": "all"}}),
        ] {
            let capabilities = Capabilities::parse(&result).unwrap();
            assert!(!capabilities.load_session(), "{result}");
            assert!(!capabilities.close_session(), "{result}");
            assert!(!capabilities.system_prompt(), "{result}");
            assert_eq!(capabilities.continuation_method(), None, "{result}");
        }
        let capabilities = Capabilities::parse(&json!({"agentCapabilities": {
            "sessionCapabilities": {"systemPrompt": {}, "close": {}}
        }}))
        .unwrap();
        assert!(capabilities.system_prompt());
        assert!(capabilities.close_session());
    }

    #[test]
    fn rejects_only_an_unsupported_protocol_version() {
        for version in [json!(2), json!("1"), json!(1.5)] {
            let error = Capabilities::parse(&json!({"protocolVersion": version})).unwrap_err();
            assert!(
                error.to_string().contains("supports only version 1"),
                "{error}"
            );
        }
        Capabilities::parse(&json!({"protocolVersion": 1})).unwrap();
    }
}
