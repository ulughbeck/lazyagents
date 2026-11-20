use std::collections::HashMap;

use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::requests::PermissionOption;
use super::{ContextUsage, Elicitation, ElicitationAnswer, ElicitationOutcome};

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ToolStatus {
    Pending,
    InProgress,
    Completed,
    Failed,
    #[serde(other)]
    Other,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(tag = "event", content = "data", rename_all = "snake_case")]
pub enum StreamEvent {
    Assistant(String),
    Reasoning(String),
    Tool {
        call_id: String,
        title: String,
        kind: Option<String>,
        status: Option<ToolStatus>,
        input: Option<Value>,
        output: Option<Value>,
    },
    PermissionRequest(PermissionRequest),
    PermissionDecision {
        title: String,
        allowed: bool,
    },
    /// Only read from old session logs. Current adapters report errors in the
    /// prompt response and on stderr.
    SystemError(String),
    /// A context compaction that ended. Progress is not stored.
    Compaction(CompactionEnd),
    /// The agent asked for a form or a link. The answers are not stored.
    Elicitation {
        message: String,
        outcome: ElicitationOutcome,
    },
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum CompactionEnd {
    Completed,
    Failed {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    Cancelled,
}

/// A live notice from the agent. Notices are not session history.
#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct Notice {
    pub severity: Severity,
    pub title: String,
    #[serde(default)]
    pub description: Option<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Info,
    Warning,
    Error,
    /// Unknown severities get a generic presentation.
    #[serde(other)]
    Other,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct PermissionRequest {
    pub title: String,
    #[serde(default)]
    pub tool_call: Option<Value>,
    pub options: Vec<PermissionOption>,
    /// The turn was cancelled, so the request was answered without asking.
    #[serde(default)]
    pub cancelled: bool,
}

/// Receives what the agent does during a request.
pub trait Handler {
    fn event(&mut self, event: StreamEvent) -> Result<()>;

    /// Asks the user about a request that is not cancelled. Returns the selected
    /// option id, or `None` to cancel the request.
    fn permission(&mut self, request: &PermissionRequest) -> Result<Option<String>> {
        let _ = request;
        Ok(None)
    }

    fn notice(&mut self, notice: Notice) -> Result<()> {
        let _ = notice;
        Ok(())
    }

    /// Asks the user about a form or link that is not cancelled.
    fn elicitation(&mut self, request: &Elicitation) -> Result<ElicitationAnswer> {
        let _ = request;
        Ok(ElicitationAnswer::Cancel)
    }
}

impl<H: Handler + ?Sized> Handler for &mut H {
    fn event(&mut self, event: StreamEvent) -> Result<()> {
        (**self).event(event)
    }

    fn permission(&mut self, request: &PermissionRequest) -> Result<Option<String>> {
        (**self).permission(request)
    }

    fn notice(&mut self, notice: Notice) -> Result<()> {
        (**self).notice(notice)
    }

    fn elicitation(&mut self, request: &Elicitation) -> Result<ElicitationAnswer> {
        (**self).elicitation(request)
    }
}

/// Ignores events and cancels permission requests and elicitations.
impl Handler for () {
    fn event(&mut self, _: StreamEvent) -> Result<()> {
        Ok(())
    }
}

/// The `session/update` payloads this client uses.
#[derive(Deserialize)]
#[serde(tag = "sessionUpdate", rename_all = "snake_case")]
pub(super) enum SessionUpdate {
    AgentMessageChunk(Chunk),
    AgentThoughtChunk(Chunk),
    ToolCall(ToolUpdate),
    ToolCallUpdate(ToolUpdate),
    UsageUpdate(ContextUsage),
    ConfigOptionUpdate {
        #[serde(rename = "configOptions")]
        config_options: Vec<Value>,
    },
    AvailableCommandsUpdate {
        #[serde(rename = "availableCommands")]
        available_commands: Vec<Value>,
    },
    Notice(Notice),
    CompactionUpdate(CompactionUpdate),
    /// Also `compaction_summary_chunk`: the summary text is not shown.
    #[serde(other)]
    Other,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct CompactionUpdate {
    compaction_id: String,
    status: CompactionStatus,
    /// A patch: omitted keeps the stored error and `null` clears it.
    #[serde(default, deserialize_with = "patch")]
    error: Option<Option<String>>,
}

fn patch<'de, D: serde::Deserializer<'de>>(value: D) -> Result<Option<Option<String>>, D::Error> {
    Option::deserialize(value).map(Some)
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum CompactionStatus {
    InProgress,
    Completed,
    Failed,
    Cancelled,
    #[serde(other)]
    Other,
}

/// Compactions by ID, so that each one ends at most once.
#[derive(Default)]
pub(super) struct Compactions(HashMap<String, Compaction>);

#[derive(Default)]
struct Compaction {
    error: Option<String>,
    ended: bool,
}

impl Compactions {
    /// Applies an update. Returns the end of the compaction when it first ends.
    pub(super) fn apply(&mut self, update: CompactionUpdate) -> Option<CompactionEnd> {
        let compaction = self.0.entry(update.compaction_id).or_default();
        if let Some(error) = update.error {
            compaction.error = error;
        }
        if compaction.ended {
            return None;
        }
        let end = match update.status {
            CompactionStatus::Completed => CompactionEnd::Completed,
            CompactionStatus::Failed => CompactionEnd::Failed {
                error: compaction.error.clone(),
            },
            CompactionStatus::Cancelled => CompactionEnd::Cancelled,
            // Unknown statuses do not end a compaction.
            CompactionStatus::InProgress | CompactionStatus::Other => return None,
        };
        compaction.ended = true;
        Some(end)
    }
}

/// Adapters put chunk text in `content.text`, in a string `content`, or in `text`.
#[derive(Deserialize)]
pub(super) struct Chunk {
    #[serde(default)]
    content: Option<ChunkContent>,
    text: Option<String>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum ChunkContent {
    Text(String),
    Block { text: String },
    Other(serde::de::IgnoredAny),
}

impl Chunk {
    fn text(&self) -> Option<&str> {
        match &self.content {
            Some(ChunkContent::Text(text) | ChunkContent::Block { text }) => Some(text),
            _ => self.text.as_deref(),
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct ToolUpdate {
    #[serde(default)]
    tool_call_id: String,
    title: Option<String>,
    kind: Option<String>,
    status: Option<ToolStatus>,
    raw_input: Option<Value>,
    raw_output: Option<Value>,
    content: Option<Value>,
    locations: Option<Value>,
}

impl ToolUpdate {
    fn into_event(self) -> Option<StreamEvent> {
        if self.tool_call_id.is_empty() {
            return None;
        }
        // Some adapters send an empty raw output and put the error in `content`.
        let raw_output = self.raw_output.filter(|value| {
            !value.as_object().is_some_and(serde_json::Map::is_empty)
                && !value.as_array().is_some_and(Vec::is_empty)
                && value.as_str() != Some("")
        });
        Some(StreamEvent::Tool {
            call_id: self.tool_call_id,
            title: self.title.unwrap_or_else(|| "Tool call".to_owned()),
            kind: self.kind,
            status: self.status,
            input: self.raw_input,
            output: raw_output.or(self.content).or(self.locations),
        })
    }
}

/// Converts session updates into events for one request.
pub(super) struct Updates<H> {
    handler: H,
    citations: CitationFilter,
    answer: String,
}

impl<H: Handler> Updates<H> {
    pub(super) fn new(handler: H) -> Self {
        Self {
            handler,
            citations: CitationFilter::default(),
            answer: String::new(),
        }
    }

    pub(super) fn event(&mut self, event: StreamEvent) -> Result<()> {
        if let StreamEvent::Assistant(text) = &event {
            self.answer.push_str(text);
        }
        self.handler.event(event)
    }

    pub(super) fn permission(&mut self, request: &PermissionRequest) -> Result<Option<String>> {
        self.handler.permission(request)
    }

    pub(super) fn elicitation(&mut self, request: &Elicitation) -> Result<ElicitationAnswer> {
        self.handler.elicitation(request)
    }

    pub(super) fn update(&mut self, update: SessionUpdate) -> Result<()> {
        let event = match update {
            SessionUpdate::AgentMessageChunk(chunk) => {
                let Some(text) = chunk.text() else {
                    return Ok(());
                };
                let text = self.citations.push(text);
                if text.is_empty() {
                    return Ok(());
                }
                StreamEvent::Assistant(text)
            }
            SessionUpdate::AgentThoughtChunk(chunk) => match chunk.text() {
                Some(text) => StreamEvent::Reasoning(text.to_owned()),
                None => return Ok(()),
            },
            SessionUpdate::ToolCall(tool) | SessionUpdate::ToolCallUpdate(tool) => {
                match tool.into_event() {
                    Some(event) => event,
                    None => return Ok(()),
                }
            }
            // The title is required and must not be empty.
            SessionUpdate::Notice(notice) if notice.title.trim().is_empty() => return Ok(()),
            SessionUpdate::Notice(notice) => return self.handler.notice(notice),
            // The client handles these updates itself.
            SessionUpdate::UsageUpdate(_)
            | SessionUpdate::ConfigOptionUpdate { .. }
            | SessionUpdate::AvailableCommandsUpdate { .. }
            | SessionUpdate::CompactionUpdate(_)
            | SessionUpdate::Other => return Ok(()),
        };
        self.event(event)
    }

    /// Sends text held back for a possible citation.
    /// Returns the complete assistant answer.
    pub(super) fn finish(mut self) -> Result<String> {
        let text = std::mem::take(&mut self.citations).finish();
        if !text.is_empty() {
            self.event(StreamEvent::Assistant(text))?;
        }
        Ok(self.answer)
    }
}

const CITATION_START: &str = "\u{e200}cite\u{e202}";
const CITATION_END: char = '\u{e201}';

#[derive(Default)]
struct CitationFilter {
    pending: String,
}

impl CitationFilter {
    fn push(&mut self, chunk: &str) -> String {
        self.pending.push_str(chunk);
        let input = std::mem::take(&mut self.pending);
        let mut remaining = input.as_str();
        let mut output = String::new();

        loop {
            if let Some(start) = remaining.find(CITATION_START) {
                output.push_str(&remaining[..start]);
                let marker = &remaining[start + CITATION_START.len()..];
                if let Some(end) = marker.find(CITATION_END) {
                    remaining = &marker[end + CITATION_END.len_utf8()..];
                    continue;
                }
                self.pending.push_str(&remaining[start..]);
                break;
            }

            let partial_start = CITATION_START
                .char_indices()
                .map(|(index, _)| index)
                .filter(|index| *index > 0 && remaining.ends_with(&CITATION_START[..*index]))
                .max()
                .unwrap_or(0);
            let complete = remaining.len() - partial_start;
            output.push_str(&remaining[..complete]);
            self.pending.push_str(&remaining[complete..]);
            break;
        }

        output
    }

    fn finish(self) -> String {
        self.pending
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    impl Handler for Vec<StreamEvent> {
        fn event(&mut self, event: StreamEvent) -> Result<()> {
            self.push(event);
            Ok(())
        }
    }

    fn events(updates: &[Value]) -> Vec<StreamEvent> {
        let mut events = Vec::new();
        let mut converter = Updates::new(&mut events);
        for update in updates {
            if let Ok(update) = SessionUpdate::deserialize(update) {
                converter.update(update).unwrap();
            }
        }
        converter.finish().unwrap();
        events
    }

    #[test]
    fn empty_raw_output_does_not_hide_tool_errors() {
        let content = json!([{"type": "content", "content": {
            "type": "text", "text": "mise: configuration is not trusted"
        }}]);
        for raw_output in [json!({}), json!([]), Value::Null, json!("")] {
            let events = events(&[json!({
                "sessionUpdate": "tool_call_update", "toolCallId": "bash-1",
                "status": "failed", "rawOutput": raw_output, "content": content
            })]);
            assert!(
                matches!(&events[0], StreamEvent::Tool { output: Some(output), .. } if output == &content)
            );
        }
    }

    #[test]
    fn tolerates_chunk_shapes_unknown_kinds_and_malformed_updates() {
        let events = events(&[
            json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "a"}}),
            json!({"sessionUpdate": "agent_message_chunk", "content": "b"}),
            json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "image"}, "text": "c"}),
            json!({"sessionUpdate": "plan", "entries": []}),
            json!({"sessionUpdate": "tool_call", "toolCallId": 5}),
            json!({"sessionUpdate": "tool_call", "toolCallId": "t", "status": "queued"}),
        ]);
        assert_eq!(
            events[..3],
            [
                StreamEvent::Assistant("a".into()),
                StreamEvent::Assistant("b".into()),
                StreamEvent::Assistant("c".into()),
            ]
        );
        assert!(matches!(
            &events[3],
            StreamEvent::Tool { title, status: Some(ToolStatus::Other), .. } if title == "Tool call"
        ));
        assert_eq!(events.len(), 4);
    }

    #[test]
    fn removes_embedded_citation_markers() {
        let mut filter = CitationFilter::default();
        assert_eq!(
            filter.push("Weather. \u{e200}cite\u{e202}turn2search1\u{e201}"),
            "Weather. "
        );
        assert_eq!(filter.finish(), "");
    }

    #[test]
    fn removes_citation_markers_split_across_chunks() {
        let mut filter = CitationFilter::default();
        assert_eq!(filter.push("Weather. \u{e200}ci"), "Weather. ");
        assert_eq!(filter.push("te\u{e202}turn2"), "");
        assert_eq!(filter.push("search1\u{e201} Next."), " Next.");
        assert_eq!(filter.finish(), "");
    }
}
