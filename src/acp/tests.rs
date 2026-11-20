use super::*;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use tempfile::{tempdir, TempDir};

/// Writes a fake adapter that logs every request to `requests.jsonl`.
fn write_fake(initialize: &str, prompt_script: &str) -> TempDir {
    let directory = tempdir().unwrap();
    let script = [
        r#"#!/bin/sh
prompts=0
while IFS= read -r line; do
  printf '%s\n' "$line" >> requests.jsonl
  case "$line" in
    *'"method":"initialize"'*)
      printf '{"jsonrpc":"2.0","id":1,"result":%s}\n' '"#,
        initialize,
        r#"'
      ;;
    *'"method":"session/new"'*|*'"method":"session/load"'*)
      printf '%s\n' '{"jsonrpc":"2.0","id":2,"result":{"sessionId":"test"}}'
      ;;
    *'"method":"session/close"'*)
      exit 0
      ;;
    *'"method":"session/prompt"'*)
      prompts=$((prompts + 1))
"#,
        prompt_script,
        "\n;;\nesac\ndone\n",
    ]
    .concat();
    fs::write(directory.path().join("fake-acp.sh"), script).unwrap();
    directory
}

fn fake_launch(directory: &TempDir) -> Launch {
    Launch {
        runtime_name: "Fake",
        program: "/bin/sh".into(),
        args: vec![directory.path().join("fake-acp.sh").into_os_string()],
        env: Default::default(),
        session_meta: None,
        system_prompt: None,
        elicitation: false,
    }
}

async fn start_fake(initialize: &str, prompt_script: &str) -> (TempDir, Result<Client>) {
    let directory = write_fake(initialize, prompt_script);
    let client = Client::start(fake_launch(&directory), directory.path(), Vec::new(), None).await;
    (directory, client)
}

async fn test_client(prompt_script: &str) -> (TempDir, Client) {
    let (directory, client) = start_fake("{}", prompt_script).await;
    (directory, client.unwrap())
}

fn requests(directory: &TempDir) -> String {
    fs::read_to_string(directory.path().join("requests.jsonl")).unwrap()
}

/// Records events and answers every permission request with `answer` and
/// every elicitation with `elicitation_answer`.
#[derive(Default)]
struct Recorder {
    events: Vec<StreamEvent>,
    requests: Vec<PermissionRequest>,
    notices: Vec<Notice>,
    answer: Option<String>,
    elicitations: Vec<Elicitation>,
    elicitation_answer: Option<ElicitationAnswer>,
}

impl Recorder {
    fn answering(answer: &str) -> Self {
        Self {
            answer: Some(answer.to_owned()),
            ..Self::default()
        }
    }
}

impl Handler for Recorder {
    fn event(&mut self, event: StreamEvent) -> Result<()> {
        self.events.push(event);
        Ok(())
    }

    fn permission(&mut self, request: &PermissionRequest) -> Result<Option<String>> {
        self.requests.push(request.clone());
        Ok(self.answer.clone())
    }

    fn notice(&mut self, notice: Notice) -> Result<()> {
        self.notices.push(notice);
        Ok(())
    }

    fn elicitation(&mut self, request: &Elicitation) -> Result<ElicitationAnswer> {
        self.elicitations.push(request.clone());
        Ok(self
            .elicitation_answer
            .clone()
            .unwrap_or(ElicitationAnswer::Cancel))
    }
}

fn update_line(update: Value) -> String {
    format!(
        "printf '%s\\n' '{}'\n",
        json!({"jsonrpc": "2.0", "method": "session/update",
            "params": {"sessionId": "test", "update": update}})
    )
}

/// Waits until the fake adapter has logged a message with `id`.
async fn logged_message(directory: &TempDir, id: u64) -> Value {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let message = requests(directory)
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .find(|message| message["id"] == id && message.get("method").is_none());
        if let Some(message) = message {
            return message;
        }
        assert!(Instant::now() < deadline, "message {id} has no answer");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

fn permission_request(id: u64) -> (Value, String, Box<serde_json::value::RawValue>) {
    let request = json!({"toolCall":{"title":"bash","_meta":{"toolName":"bash"}},
        "options":[{"optionId":"once","kind":"allow_once"}]});
    let params = serde_json::value::to_raw_value(&request).unwrap();
    (json!(id), "session/request_permission".to_owned(), params)
}

#[tokio::test]
async fn permanent_permissions_survive_restart_and_can_be_revoked() {
    let (directory, client) = test_client(r#"
printf '%s\n' '{"jsonrpc":"2.0","id":77,"method":"session/request_permission","params":{"toolCall":{"toolCallId":"bash-1","title":"bash","_meta":{"toolName":"bash"}},"options":[{"optionId":"once","kind":"allow_once","name":"Allow once"},{"optionId":"allow_always","kind":"allow_always","name":"Always allow bash"}]}}'
IFS= read -r answer
printf '%s\n' "$answer" >> answers.jsonl
printf '%s\n' '{"jsonrpc":"2.0","id":3,"result":{"stopReason":"end_turn"}}'
"#).await;
    fs::create_dir(directory.path().join(".agents")).unwrap();
    fs::write(
        directory.path().join(".agents/agent.json"),
        json!({
            "name": "test", "slug": "test", "description": "test", "harness": "pi"
        })
        .to_string(),
    )
    .unwrap();
    drop(client);
    for (index, decision) in [
        Some("allow_always"),
        Some(crate::permissions::REMEMBER),
        None,
        Some("once"),
    ]
    .into_iter()
    .enumerate()
    {
        let launch = fake_launch(&directory);
        let mut client = Client::start(launch, directory.path(), Vec::new(), None)
            .await
            .unwrap();
        client.remember_permissions(directory.path(), crate::harness::get("pi").unwrap());
        let mut recorder = Recorder {
            answer: decision.map(str::to_owned),
            ..Recorder::default()
        };
        client.prompt("test", &mut recorder).await.unwrap();
        assert_eq!(recorder.requests.len(), usize::from(index != 2));
        assert!(recorder.requests.iter().all(|request| request
            .options
            .iter()
            .any(|option| option.id == crate::permissions::REMEMBER)));
        let config = crate::config::AgentConfig::load(directory.path()).unwrap();
        assert_eq!(
            config.permissions.len(),
            usize::from(matches!(index, 1 | 2))
        );
        if index == 2 {
            let (id, method, params) = permission_request(99);
            let mut recorder = Recorder::answering(crate::permissions::REMEMBER);
            client
                .handle_request(id, &method, &params, &mut Updates::new(&mut recorder), true)
                .await
                .unwrap();
            assert!(recorder.requests.is_empty());
            assert!(matches!(
                &recorder.events[0],
                StreamEvent::PermissionRequest(PermissionRequest {
                    cancelled: true,
                    ..
                })
            ));
            crate::config::save_permissions(directory.path(), &[]).unwrap();
        }
        drop(client);
    }
    let answers = fs::read_to_string(directory.path().join("answers.jsonl")).unwrap();
    let ids = answers
        .lines()
        .map(|line| {
            serde_json::from_str::<Value>(line).unwrap()["result"]["outcome"]["optionId"].clone()
        })
        .collect::<Vec<_>>();
    assert_eq!(
        ids,
        vec![
            json!("allow_always"),
            json!("once"),
            json!("once"),
            json!("once")
        ]
    );
}

#[tokio::test]
async fn full_access_applies_immediately_and_ask_mode_restores_prompts() {
    let (directory, mut client) = test_client(r#"
response_id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9]*\).*/\1/p')
printf '%s\n' '{"jsonrpc":"2.0","id":77,"method":"session/request_permission","params":{"toolCall":{"title":"bash"},"options":[{"optionId":"once","kind":"allow_once","name":"Allow once"}]}}'
IFS= read -r answer
printf '%s\n' "$answer" >> answers.jsonl
printf '{"jsonrpc":"2.0","id":%s,"result":{"stopReason":"end_turn"}}\n' "$response_id"
"#).await;
    fs::create_dir(directory.path().join(".agents")).unwrap();
    fs::write(
        directory.path().join(".agents/agent.json"),
        json!({
            "name": "test", "slug": "test", "description": "test", "harness": "pi"
        })
        .to_string(),
    )
    .unwrap();
    client.remember_permissions(directory.path(), crate::harness::get("pi").unwrap());
    for mode in [
        crate::permissions::Mode::FullAccess,
        crate::permissions::Mode::Ask,
    ] {
        crate::config::save_permission_mode(directory.path(), mode).unwrap();
        let mut recorder = Recorder::answering("once");
        client.prompt("test", &mut recorder).await.unwrap();
        assert_eq!(
            recorder.requests.len(),
            usize::from(mode == crate::permissions::Mode::Ask)
        );
        assert!(crate::config::AgentConfig::load(directory.path())
            .unwrap()
            .permissions
            .is_empty());
    }
    let answers = fs::read_to_string(directory.path().join("answers.jsonl")).unwrap();
    assert_eq!(answers.lines().count(), 2);
    assert!(answers.lines().all(
        |line| serde_json::from_str::<Value>(line).unwrap()["result"]["outcome"]["optionId"]
            == "once"
    ));
    crate::config::save_permission_mode(directory.path(), crate::permissions::Mode::FullAccess)
        .unwrap();
    let (id, method, params) = permission_request(99);
    let mut recorder = Recorder::answering("once");
    client
        .handle_request(id, &method, &params, &mut Updates::new(&mut recorder), true)
        .await
        .unwrap();
    assert!(recorder.events.iter().any(|event| matches!(
        event,
        StreamEvent::PermissionDecision { allowed: false, .. }
    )));
}

#[tokio::test]
async fn retains_harness_usage_for_status() {
    let (_directory, mut client) = test_client(r#"
printf '%s\n' '{"jsonrpc":"2.0","method":"session/update","params":{"update":{"sessionUpdate":"usage_update","used":1234,"size":10000,"cost":{"amount":0.02,"currency":"USD"}}}}'
printf '%s\n' '{"jsonrpc":"2.0","id":3,"result":{"stopReason":"end_turn","usage":{"inputTokens":100,"outputTokens":20,"totalTokens":120}}}'
"#).await;
    assert!(client.usage().is_none());
    assert!(client.context_usage().is_none());
    client.prompt("hello", ()).await.unwrap();
    let context = client.context_usage().unwrap();
    assert_eq!((context.used, context.size), (1234, 10000));
    assert_eq!(context.cost.as_ref().unwrap().currency, "USD");
    assert_eq!(client.usage().unwrap().total_tokens, Some(120));
}

#[tokio::test]
async fn preserves_partial_response_across_inactivity_timeout() {
    let (_directory, mut client) = test_client(r#"
if [ "$prompts" -eq 1 ]; then
  printf '%s' '{"jsonrpc":"2.0","id":3,'
  sleep 0.1
  printf '%s\n' '"result":{"stopReason":"cancelled"}}'
else
  printf '%s\n' '{"jsonrpc":"2.0","method":"session/update","params":{"update":{"sessionUpdate":"agent_message_chunk","content":{"text":"Recovered"}}}}'
  printf '%s\n' '{"jsonrpc":"2.0","id":4,"result":{"stopReason":"end_turn"}}'
fi
"#).await;
    let error = client
        .prompt_with_timeout("Wait", (), Some(Duration::from_millis(30)))
        .await
        .unwrap_err();
    assert!(error
        .downcast_ref::<PromptTimeout>()
        .unwrap()
        .connection_ready());
    assert_eq!(client.prompt("Again", ()).await.unwrap(), "Recovered");
}

#[tokio::test]
async fn cancellation_deadline_does_not_restart_for_updates_or_permissions() {
    let (_directory, mut client) = test_client(r#"
sleep 0.1
printf '%s\n' '{"jsonrpc":"2.0","id":77,"method":"session/request_permission","params":{"toolCall":{"title":"Late write"},"options":[]}}'
while :; do
  printf '%s\n' '{"jsonrpc":"2.0","method":"session/update","params":{"update":{"sessionUpdate":"agent_message_chunk","content":{"text":"Still running"}}}}'
  sleep 0.02
done
"#).await;
    let started = Instant::now();
    let mut recorder = Recorder::default();
    let error = timeout(
        Duration::from_secs(7),
        client.prompt_with_timeout("Wait", &mut recorder, Some(Duration::from_millis(30))),
    )
    .await
    .expect("cancellation did not meet its deadline")
    .unwrap_err();
    assert!(!error
        .downcast_ref::<PromptTimeout>()
        .unwrap()
        .connection_ready());
    assert!(started.elapsed() < Duration::from_secs(6));
    assert!(recorder
        .events
        .iter()
        .any(|event| matches!(event, StreamEvent::Assistant(_))));
    let late_permissions = recorder
        .events
        .iter()
        .filter(|event| {
            matches!(
                event,
                StreamEvent::PermissionRequest(PermissionRequest {
                    cancelled: true,
                    ..
                })
            )
        })
        .count();
    assert_eq!(late_permissions, 1);
    assert!(recorder.requests.is_empty());
}

#[tokio::test]
async fn runs_a_streaming_session_with_tools_and_permission() {
    let directory = tempdir().unwrap();
    let adapter = directory.path().join("fake-acp.sh");
    fs::write(
        &adapter,
        r##"#!/bin/sh
while IFS= read -r line; do
  case "$line" in
    *'"method":"initialize"'*)
      printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":1,"agentCapabilities":{"sessionCapabilities":{"close":{}}}}}'
      ;;
    *'"method":"session/new"'*)
      printf '%s\n' '{"jsonrpc":"2.0","id":2,"result":{"sessionId":"fake-session"}}'
      ;;
    *'"method":"session/prompt"'*)
      printf '%s\n' '{"jsonrpc":"2.0","method":"session/update","params":{"update":{"sessionUpdate":"agent_thought_chunk","content":{"text":"Thinking"}}}}'
      printf '%s\n' '{"jsonrpc":"2.0","method":"session/update","params":{"update":{"sessionUpdate":"tool_call","toolCallId":"tool-1","title":"Read file","status":"pending"}}}'
      printf '%s\n' '{"jsonrpc":"2.0","method":"session/update","params":{"update":{"sessionUpdate":"tool_call_update","toolCallId":"tool-1","title":"Read file","status":"completed","rawOutput":"done"}}}'
      printf '%s\n' '{"jsonrpc":"2.0","id":77,"method":"session/request_permission","params":{"toolCall":{"title":"Write file","rawInput":{"command":"write workspace/file"}},"options":[{"optionId":"allow","name":"Allow once","kind":"allow_once"},{"optionId":"reject","name":"Reject once","kind":"reject_once"}]}}'
      IFS= read -r permission
      printf '%s\n' '{"jsonrpc":"2.0","method":"session/update","params":{"update":{"sessionUpdate":"agent_message_chunk","content":{"text":"Hello"}}}}'
      printf '%s\n' '{"jsonrpc":"2.0","id":3,"result":{"stopReason":"end_turn"}}'
      ;;
    *'"method":"session/close"'*)
      printf '%s\n' '{"jsonrpc":"2.0","id":4,"result":{}}'
      exit 0
      ;;
  esac
done
"##,
    )
    .unwrap();
    fs::set_permissions(&adapter, fs::Permissions::from_mode(0o755)).unwrap();
    let launch = Launch {
        runtime_name: "Fake",
        program: "/bin/sh".into(),
        args: vec![adapter.into_os_string()],
        env: Default::default(),
        session_meta: None,
        system_prompt: None,
        elicitation: false,
    };
    let mut client = Client::start(launch, directory.path(), Vec::new(), None)
        .await
        .unwrap();
    let mut recorder = Recorder::answering("allow");
    let answer = client.prompt("Hi", &mut recorder).await.unwrap();
    assert_eq!(answer, "Hello");
    assert!(recorder
        .requests
        .iter()
        .any(|request| matches!(&request.tool_call,
        Some(call) if call["rawInput"]["command"] == "write workspace/file")));
    let events = recorder.events;
    assert!(events
        .iter()
        .any(|event| matches!(event, StreamEvent::Reasoning(_))));
    assert!(events.iter().any(|event| matches!(
        event,
        StreamEvent::Tool {
            status: Some(ToolStatus::Completed),
            ..
        }
    )));
    assert!(events
        .iter()
        .any(|event| matches!(event, StreamEvent::PermissionDecision { allowed: true, .. })));
    client.close().await;
}

#[tokio::test]
async fn drains_a_cancelled_turn_before_the_next_prompt() {
    let (_directory, mut client) = test_client(r#"
      if [ "$prompts" -eq 1 ]; then
        IFS= read -r cancel
        printf '%s\n' '{"jsonrpc":"2.0","id":3,"result":{"stopReason":"cancelled"}}'
      else
        printf '%s\n' '{"jsonrpc":"2.0","method":"session/update","params":{"update":{"sessionUpdate":"agent_message_chunk","content":{"text":"Recovered"}}}}'
        printf '%s\n' '{"jsonrpc":"2.0","id":4,"result":{"stopReason":"end_turn"}}'
      fi
"#).await;
    let error = client
        .prompt_with_timeout("Wait", (), Some(Duration::from_millis(10)))
        .await
        .unwrap_err();
    assert!(error.downcast_ref::<PromptTimeout>().is_some());
    let answer = client.prompt("Again", ()).await.unwrap();
    assert_eq!(answer, "Recovered");
}

#[tokio::test]
async fn marks_the_connection_unusable_when_cancellation_does_not_finish() {
    let (_directory, mut client) = test_client("sleep 1").await;

    let error = client
        .prompt_with_timeout("Wait", (), Some(Duration::from_millis(10)))
        .await
        .unwrap_err();
    let timeout = error.downcast_ref::<PromptTimeout>().unwrap();
    assert!(!timeout.connection_ready());
    drop(client);
}

#[tokio::test]
async fn answers_a_permission_request_when_the_user_prompt_fails() {
    struct Interrupted;
    impl Handler for Interrupted {
        fn event(&mut self, _: StreamEvent) -> Result<()> {
            Ok(())
        }
        fn permission(&mut self, _: &PermissionRequest) -> Result<Option<String>> {
            Err(anyhow::anyhow!("interrupted"))
        }
        fn elicitation(&mut self, _: &Elicitation) -> Result<ElicitationAnswer> {
            Err(anyhow::anyhow!("interrupted"))
        }
    }
    let (directory, mut client) = test_client(r#"
if [ "$prompts" -eq 1 ]; then
  printf '%s\n' '{"jsonrpc":"2.0","id":77,"method":"session/request_permission","params":{"toolCall":{"title":"bash"},"options":[{"optionId":"once","kind":"allow_once"}]}}'
else
  printf '%s\n' '{"jsonrpc":"2.0","id":78,"method":"elicitation/create","params":{"sessionId":"test","mode":"form","message":"Name?","requestedSchema":{"properties":{"name":{"type":"string"}}}}}'
fi
"#).await;
    let error = client.prompt("test", Interrupted).await.unwrap_err();
    assert_eq!(error.to_string(), "interrupted");
    // The adapter logs the answer after it reads it.
    let answer = logged_message(&directory, 77).await;
    assert_eq!(answer["result"]["outcome"], json!({"outcome": "cancelled"}));
    client.elicitation = true;
    let error = client.prompt("again", Interrupted).await.unwrap_err();
    assert_eq!(error.to_string(), "interrupted");
    let answer = logged_message(&directory, 78).await;
    assert_eq!(answer["result"], json!({"action": "cancel"}));
}

#[tokio::test]
async fn advertises_elicitation_only_when_the_chat_can_show_it() {
    for elicitation in [false, true] {
        let directory = write_fake("{}", "");
        let launch = Launch {
            elicitation,
            ..fake_launch(&directory)
        };
        Client::start(launch, directory.path(), Vec::new(), None)
            .await
            .unwrap();
        let initialize =
            serde_json::from_str::<Value>(requests(&directory).lines().next().unwrap()).unwrap();
        let mut expected = json!({"session": {
            "configOptions": {"boolean": {}}, "notices": {}, "compaction": {}
        }});
        if elicitation {
            expected["elicitation"] = json!({"form": {}});
        }
        assert_eq!(initialize["params"]["clientCapabilities"], expected);
    }
}

#[tokio::test]
async fn answers_forms_in_the_wire_format_and_rejects_links() {
    let request = |id: u64, params: &str| {
        format!(
            r#"printf '%s\n' '{{"jsonrpc":"2.0","id":{id},"method":"elicitation/create","params":{{"sessionId":"test",{params}}}}}'
IFS= read -r answer
printf '%s\n' "$answer" >> requests.jsonl
"#
        )
    };
    let script = [
        request(
            80,
            r#""mode":"form","message":"Setup","requestedSchema":{"type":"object","properties":{"name":{"type":"string"},"count":{"type":"integer"}},"required":["name"]}"#,
        ),
        request(81, r#""mode":"url","message":"Sign in","url":"https://auth.example/login","elicitationId":"e1""#),
        request(
            82,
            r#""mode":"form","message":"Pattern","requestedSchema":{"properties":{"code":{"type":"string","pattern":"^[0-9]+$"}}}"#,
        ),
        r#"printf '%s\n' '{"jsonrpc":"2.0","method":"elicitation/complete","params":{"elicitationId":"unknown"}}'
"#
        .to_owned(),
        r#"printf '%s\n' '{"jsonrpc":"2.0","id":3,"result":{"stopReason":"end_turn"}}'"#.to_owned(),
    ]
    .concat();
    let directory = write_fake("{}", &script);
    let launch = Launch {
        elicitation: true,
        ..fake_launch(&directory)
    };
    let mut client = Client::start(launch, directory.path(), Vec::new(), None)
        .await
        .unwrap();
    let mut content = serde_json::Map::new();
    content.insert("name".into(), json!("Ada"));
    let mut recorder = Recorder {
        elicitation_answer: Some(ElicitationAnswer::Accept(content)),
        ..Recorder::default()
    };
    client.prompt("test", &mut recorder).await.unwrap();
    assert_eq!(
        logged_message(&directory, 80).await["result"],
        json!({"action": "accept", "content": {"name": "Ada"}})
    );
    // Link mode is not advertised, so it is an invalid request.
    assert_eq!(
        logged_message(&directory, 81).await["error"]["code"],
        -32602
    );
    // Unsupported forms are declined without asking.
    assert_eq!(
        logged_message(&directory, 82).await["result"],
        json!({"action": "decline"})
    );
    assert_eq!(recorder.elicitations.len(), 1);
    let outcomes = recorder
        .events
        .iter()
        .filter_map(|event| match event {
            StreamEvent::Elicitation { outcome, .. } => Some(*outcome),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        outcomes,
        [
            ElicitationOutcome::Answered,
            ElicitationOutcome::Unsupported,
        ]
    );

    // A cancelled turn and idle chat answer without asking.
    let params = serde_json::value::to_raw_value(&json!({"sessionId": "test", "mode": "form",
        "message": "Late", "requestedSchema": {}}))
    .unwrap();
    let mut recorder = Recorder::default();
    client
        .handle_request(
            json!(90),
            "elicitation/create",
            &params,
            &mut Updates::new(&mut recorder),
            true,
        )
        .await
        .unwrap();
    assert!(recorder.elicitations.is_empty());
    assert_eq!(
        logged_message(&directory, 90).await["result"],
        json!({"action": "cancel"})
    );
}

#[tokio::test]
async fn applies_config_option_updates_from_the_agent() {
    let (_directory, mut client) = test_client(r#"
printf '%s\n' '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"test","update":{"sessionUpdate":"config_option_update","configOptions":[{"id":"model","type":"select","category":"model","currentValue":"deep","options":[{"value":"fast","name":"Fast"},{"value":"deep","name":"Deep"}]}]}}}'
printf '%s\n' '{"jsonrpc":"2.0","id":3,"result":{"stopReason":"end_turn"}}'
"#).await;
    assert_eq!(client.current_config_label(ConfigKind::Model), None);
    client.prompt("test", ()).await.unwrap();
    assert_eq!(client.current_config_label(ConfigKind::Model), Some("deep"));
}

#[tokio::test]
async fn closes_the_session_only_when_the_agent_supports_it() {
    for (initialize, expected) in [
        ("{}", false),
        (
            r#"{"agentCapabilities":{"sessionCapabilities":{"close":{}}}}"#,
            true,
        ),
    ] {
        let (directory, client) = start_fake(initialize, "").await;
        client.unwrap().close().await;
        assert_eq!(
            requests(&directory).contains(r#""method":"session/close""#),
            expected
        );
    }
}

#[tokio::test]
async fn rejects_an_unsupported_protocol_version() {
    let (_directory, client) = start_fake(r#"{"protocolVersion":2}"#, "").await;
    let error = format!("{:#}", client.err().unwrap());
    assert!(error.contains("ACP protocol version 2"), "{error}");
    let (_directory, client) = start_fake(r#"{"protocolVersion":1}"#, "").await;
    assert!(client.is_ok());
}

#[tokio::test]
async fn sends_the_system_prompt_only_when_the_agent_supports_it() {
    let initialize =
        r#"{"agentCapabilities":{"loadSession":true,"sessionCapabilities":{"systemPrompt":{}}}}"#;
    for resume in [None, Some("saved")] {
        let directory = write_fake(initialize, "");
        let launch = Launch {
            system_prompt: Some("Be brief.".into()),
            ..fake_launch(&directory)
        };
        Client::start(launch, directory.path(), Vec::new(), resume)
            .await
            .unwrap();
        let session = requests(&directory)
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .find(|request| request["id"] == 2)
            .unwrap();
        assert_eq!(
            session["params"]["systemPrompt"],
            json!({"mode": "append", "content": [{"type": "text", "text": "Be brief."}]}),
            "{session}"
        );
    }

    let directory = write_fake(r#"{"agentCapabilities":{"loadSession":true}}"#, "");
    let launch = Launch {
        system_prompt: Some("Be brief.".into()),
        ..fake_launch(&directory)
    };
    let error = Client::start(launch, directory.path(), Vec::new(), None)
        .await
        .err()
        .unwrap();
    assert!(
        format!("{error:#}").contains("lazyagents repair"),
        "{error:#}"
    );
    assert!(!requests(&directory).contains("session/new"));

    let (directory, client) = start_fake(initialize, "").await;
    client.unwrap();
    assert!(!requests(&directory).contains("systemPrompt"));
}

#[tokio::test]
async fn advertises_boolean_options_and_sends_them_as_booleans() {
    let (directory, mut client) = test_client(r#"
printf '%s\n' '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"test","update":{"sessionUpdate":"config_option_update","configOptions":[{"id":"fast","name":"Fast","type":"boolean","category":"model_config","currentValue":false}]}}}'
printf '%s\n' '{"jsonrpc":"2.0","id":3,"result":{"stopReason":"end_turn"}}'
while IFS= read -r line; do
  printf '%s\n' "$line" >> requests.jsonl
  printf '%s\n' '{"jsonrpc":"2.0","id":4,"result":{}}'
done
"#).await;
    client.prompt("test", ()).await.unwrap();
    assert!(client
        .set_model_config("fast", &ConfigValue::Boolean(true))
        .await
        .unwrap());
    assert_eq!(
        client.model_config_values().get("fast"),
        Some(&ConfigValue::Boolean(true))
    );
    assert!(!client
        .set_model_config("fast", &ConfigValue::Select("true".into()))
        .await
        .unwrap());
    let requests: Vec<Value> = requests(&directory)
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(
        requests[0]["params"]["clientCapabilities"],
        json!({"session": {"configOptions": {"boolean": {}}, "notices": {}, "compaction": {}}})
    );
    let setting = requests
        .iter()
        .find(|request| request["method"] == "session/set_config_option")
        .unwrap();
    assert_eq!(
        setting["params"],
        json!({"sessionId": "test", "configId": "fast", "type": "boolean", "value": true})
    );
}

#[tokio::test]
async fn reports_notices_and_each_compaction_end_once() {
    let script = [
        json!({"sessionUpdate": "notice", "severity": "warning", "title": "MCP server unavailable",
            "description": "Continuing without it."}),
        json!({"sessionUpdate": "notice", "severity": "_custom", "title": "Custom", "description": null}),
        json!({"sessionUpdate": "notice", "severity": "info", "title": ""}),
        json!({"sessionUpdate": "compaction_update", "compactionId": "a", "status": "in_progress"}),
        json!({"sessionUpdate": "compaction_summary_chunk", "compactionId": "a",
            "content": {"type": "text", "text": "Summary"}}),
        json!({"sessionUpdate": "compaction_update", "compactionId": "a", "status": "_paused",
            "error": "context too large"}),
        json!({"sessionUpdate": "compaction_update", "compactionId": "a", "status": "failed"}),
        json!({"sessionUpdate": "compaction_update", "compactionId": "a", "status": "completed"}),
        json!({"sessionUpdate": "compaction_update", "compactionId": "b", "status": "completed"}),
        json!({"sessionUpdate": "compaction_update", "compactionId": "c", "status": "failed",
            "error": "first"}),
        json!({"sessionUpdate": "compaction_update", "compactionId": "d", "status": "in_progress",
            "error": "stale"}),
        json!({"sessionUpdate": "compaction_update", "compactionId": "d", "status": "cancelled",
            "error": null}),
    ]
    .into_iter()
    .map(update_line)
    .collect::<String>();
    let (_directory, mut client) = test_client(&format!(
        r#"{script}printf '%s\n' '{{"jsonrpc":"2.0","id":3,"result":{{"stopReason":"end_turn"}}}}'"#
    ))
    .await;
    let mut recorder = Recorder::default();
    assert_eq!(client.prompt("test", &mut recorder).await.unwrap(), "");
    assert_eq!(
        recorder.notices,
        [
            Notice {
                severity: Severity::Warning,
                title: "MCP server unavailable".into(),
                description: Some("Continuing without it.".into()),
            },
            Notice {
                severity: Severity::Other,
                title: "Custom".into(),
                description: None,
            },
        ]
    );
    assert_eq!(
        recorder.events,
        [
            StreamEvent::Compaction(CompactionEnd::Failed {
                error: Some("context too large".into())
            }),
            StreamEvent::Compaction(CompactionEnd::Completed),
            StreamEvent::Compaction(CompactionEnd::Failed {
                error: Some("first".into())
            }),
            StreamEvent::Compaction(CompactionEnd::Cancelled),
        ]
    );
}

#[tokio::test]
async fn reads_and_answers_agent_messages_between_requests() {
    let directory = write_fake("{}", "");
    let script = directory.path().join("fake-acp.sh");
    let session = r#"printf '%s\n' '{"jsonrpc":"2.0","id":2,"result":{"sessionId":"test"}}'
"#;
    let commands = update_line(json!({"sessionUpdate": "available_commands_update",
        "availableCommands": [{"name": "web", "description": "Search", "input": {"hint": "query"}}]}));
    // The notice arrives in two parts, so the first read is cancelled mid-line.
    let after_session = format!(
        r#"{commands}printf '%s' '{{"jsonrpc":"2.0","method":"session/update",'
sleep 0.3
printf '%s\n' '"params":{{"update":{{"sessionUpdate":"notice","severity":"info","title":"Ready"}}}}}}'
printf '%s\n' '{{"jsonrpc":"2.0","id":77,"method":"session/request_permission","params":{{"toolCall":{{"title":"bash"}},"options":[{{"optionId":"once","kind":"allow_once"}}]}}}}'
printf '%s\n' '{{"jsonrpc":"2.0","id":78,"method":"fs/read_text_file","params":{{}}}}'
"#
    );
    let source = fs::read_to_string(&script).unwrap();
    assert!(source.contains(session));
    fs::write(
        &script,
        source.replace(session, &format!("{session}{after_session}")),
    )
    .unwrap();
    let mut client = Client::start(fake_launch(&directory), directory.path(), Vec::new(), None)
        .await
        .unwrap();
    let mut recorder = Recorder::answering("once");
    let message = client.next_message().await.unwrap();
    client.handle_idle(message, &mut recorder).await.unwrap();
    assert_eq!(client.commands()[0].name, "web");
    assert_eq!(client.commands()[0].hint.as_deref(), Some("query"));
    assert!(
        timeout(Duration::from_millis(100), client.next_message())
            .await
            .is_err(),
        "the notice arrived early"
    );
    for _ in 0..3 {
        let message = timeout(Duration::from_secs(2), client.next_message())
            .await
            .unwrap()
            .unwrap();
        client.handle_idle(message, &mut recorder).await.unwrap();
    }
    assert_eq!(recorder.notices[0].title, "Ready");
    // No turn is active, so the user is not asked.
    assert!(recorder.requests.is_empty());
    let answer = logged_message(&directory, 77).await;
    assert_eq!(answer["result"]["outcome"], json!({"outcome": "cancelled"}));
    let answer = logged_message(&directory, 78).await;
    assert_eq!(answer["error"]["code"], -32601);
}

#[tokio::test]
async fn keeps_notices_from_control_requests() {
    let directory = write_fake("{}", "");
    let script = directory.path().join("fake-acp.sh");
    let session = r#"printf '%s\n' '{"jsonrpc":"2.0","id":2,"result":{"sessionId":"test"}}'"#;
    let notice =
        update_line(json!({"sessionUpdate": "notice", "severity": "error", "title": "No MCP"}));
    let source = fs::read_to_string(&script).unwrap();
    fs::write(
        &script,
        source.replace(session, &format!("{notice}{session}")),
    )
    .unwrap();
    let mut client = Client::start(fake_launch(&directory), directory.path(), Vec::new(), None)
        .await
        .unwrap();
    let notices = client.take_notices();
    assert_eq!(notices[0].severity, Severity::Error);
    assert!(client.take_notices().is_empty());
}
