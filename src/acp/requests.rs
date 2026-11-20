use std::path::Path;

use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;
use serde_json::{json, Value};

use super::{Client, Handler, PermissionRequest, StreamEvent, Updates};
use crate::harness::Harness;
use crate::permissions::{Mode, Store, REMEMBER};

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct PermissionOption {
    pub id: String,
    pub kind: String,
    pub label: String,
}

impl PermissionOption {
    fn allows(&self) -> bool {
        matches!(self.kind.as_str(), "allow_once" | "allow_always")
    }
}

/// The `session/request_permission` params this client uses.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PermissionParams {
    #[serde(default)]
    tool_call: Option<Value>,
    options: Vec<Value>,
}

impl Client {
    pub fn remember_permissions(&mut self, root: &Path, harness: &'static dyn Harness) {
        self.permissions = Some(Store::new(root, harness));
    }

    pub(super) async fn handle_request<H: Handler>(
        &mut self,
        id: Value,
        method: &str,
        params: &RawValue,
        updates: &mut Updates<H>,
        cancelled: bool,
    ) -> Result<()> {
        match method {
            "session/request_permission" => {
                self.handle_permission(id, params, updates, cancelled)
                    .await
            }
            "elicitation/create" => {
                self.handle_elicitation(id, params, updates, cancelled)
                    .await
            }
            _ => {
                self.write(&json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": {"code": -32601, "message": format!("Unsupported ACP client method: {method}")}
                }))
                .await
            }
        }
    }

    async fn handle_permission<H: Handler>(
        &mut self,
        id: Value,
        params: &RawValue,
        updates: &mut Updates<H>,
        cancelled: bool,
    ) -> Result<()> {
        let Ok(params) = serde_json::from_str::<PermissionParams>(params.get()) else {
            return self
                .write(&json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": {"code": -32602, "message": "Invalid session/request_permission params"}
                }))
                .await;
        };
        let request = PermissionRequest {
            title: params
                .tool_call
                .as_ref()
                .and_then(|call| call.get("title"))
                .and_then(Value::as_str)
                .unwrap_or("The agent wants to use a tool")
                .to_owned(),
            tool_call: params.tool_call,
            options: params
                .options
                .iter()
                .filter_map(permission_option)
                .collect(),
            cancelled,
        };
        let decision = decide(self.permissions.as_ref(), &request, updates);
        let selected = decision
            .as_ref()
            .ok()
            .and_then(Option::as_deref)
            .and_then(|selected| request.options.iter().find(|option| option.id == selected));
        let outcome = selected.map_or_else(
            || json!({"outcome": "cancelled"}),
            |option| json!({"outcome": "selected", "optionId": option.id}),
        );
        // Every request needs an answer, also when the decision failed.
        self.write(&json!({"jsonrpc": "2.0", "id": id, "result": {"outcome": outcome}}))
            .await?;
        let allowed = selected.is_some_and(PermissionOption::allows);
        decision?;
        updates.event(StreamEvent::PermissionDecision {
            title: request.title,
            allowed,
        })
    }
}

/// Answers from saved policy when possible and asks the user otherwise.
/// Returns the selected option id, or `None` to cancel.
fn decide<H: Handler>(
    store: Option<&Store>,
    request: &PermissionRequest,
    updates: &mut Updates<H>,
) -> Result<Option<String>> {
    if request.cancelled {
        updates.event(StreamEvent::PermissionRequest(request.clone()))?;
        return Ok(None);
    }
    let Some(store) = store else {
        return updates.permission(request);
    };
    if store.mode()? == Mode::FullAccess {
        if let Some(option) = request
            .options
            .iter()
            .find(|option| option.kind == "allow_once")
        {
            return Ok(Some(option.id.clone()));
        }
    }
    let once = crate::permissions::allow_once(&request.options);
    let saved = once.zip(
        request
            .tool_call
            .as_ref()
            .and_then(|call| store.candidate(call)),
    );
    let Some((once, candidate)) = saved else {
        return updates.permission(request);
    };
    if store.contains(&candidate)? {
        return Ok(Some(once.to_owned()));
    }
    let selected = updates.permission(&PermissionRequest {
        options: store.options(&request.options, &candidate),
        ..request.clone()
    })?;
    if selected.as_deref() == Some(REMEMBER) {
        store.remember(candidate)?;
        return Ok(Some(once.to_owned()));
    }
    Ok(selected)
}

fn permission_option(value: &Value) -> Option<PermissionOption> {
    let kind = value.get("kind")?.as_str()?.to_owned();
    Some(PermissionOption {
        id: value.get("optionId")?.as_str()?.to_owned(),
        label: value
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or(&kind)
            .to_owned(),
        kind,
    })
}
