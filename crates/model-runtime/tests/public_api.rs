use std::collections::BTreeMap;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use serde_json::json;
use young_model_runtime::{
    FakeModelClient, ModelClient, ModelError, ModelMessage, ModelMessageContent, ModelMessageRole,
    ModelRequest, ModelRequestId, ModelStreamEvent, ModelToolCall, ModelToolCallId,
    ScriptedModelTurn,
};

fn request(messages: Vec<ModelMessage>) -> ModelRequest {
    ModelRequest {
        model: "test-model".to_string(),
        messages,
        tools: Vec::new(),
        metadata: BTreeMap::new(),
    }
}

#[test]
fn message_constructors_preserve_roles_text_and_structured_content() {
    let tool_call = ModelToolCall {
        id: ModelToolCallId::new("model-call-001"),
        name: "read_file".to_string(),
        arguments: json!({ "path": "README.md" }),
    };
    let messages = [
        (
            ModelMessage::system("system"),
            ModelMessageRole::System,
            Some("system"),
        ),
        (
            ModelMessage::user("user"),
            ModelMessageRole::User,
            Some("user"),
        ),
        (
            ModelMessage::assistant("assistant"),
            ModelMessageRole::Assistant,
            Some("assistant"),
        ),
        (
            ModelMessage::assistant_with_tool_calls("with call", vec![tool_call.clone()]),
            ModelMessageRole::Assistant,
            Some("with call"),
        ),
        (
            ModelMessage::assistant_tool_calls(vec![tool_call]),
            ModelMessageRole::Assistant,
            None,
        ),
        (
            ModelMessage::tool("result", "read_file", "model-call-001"),
            ModelMessageRole::Tool,
            Some("result"),
        ),
        (
            ModelMessage::tool_content(
                vec![
                    ModelMessageContent::json(json!({ "bytes": 6 })),
                    ModelMessageContent::text("fallback text"),
                ],
                "read_file",
                "model-call-002",
            ),
            ModelMessageRole::Tool,
            Some("fallback text"),
        ),
    ];

    for (message, role, text) in messages {
        assert_eq!(message.role(), role);
        assert_eq!(message.text_content(), text);
    }

    assert_eq!(ModelMessageContent::text("text").as_text(), Some("text"));
    assert_eq!(
        ModelMessageContent::json(json!({ "ok": true })).as_text(),
        None
    );
    assert_eq!(ModelRequestId::new("request-001").as_str(), "request-001");
}

#[test]
fn fake_model_client_reports_events_errors_and_script_exhaustion() {
    let provider_error = ModelError {
        code: "provider_down".to_string(),
        message: "provider unavailable".to_string(),
        retryable: true,
    };
    let completed = ModelStreamEvent::Completed {
        finish_reason: Some("stop".to_string()),
        extensions: BTreeMap::new(),
    };
    let mut client = FakeModelClient::new([
        ScriptedModelTurn::events([completed.clone()]),
        ScriptedModelTurn::error(provider_error.clone()),
    ]);
    let cancellation = Arc::new(AtomicBool::new(false));

    assert_eq!(client.remaining_turns(), 2);
    let first_request = request(vec![ModelMessage::user("first")]);
    let events = client
        .stream(&first_request, Arc::clone(&cancellation))
        .expect("first scripted turn should stream")
        .collect::<Vec<_>>();
    assert_eq!(events, vec![completed]);
    assert_eq!(client.request_count(), 1);
    assert_eq!(client.last_message(), Some(&ModelMessage::user("first")));
    assert_eq!(client.remaining_turns(), 1);

    let second_request = request(vec![ModelMessage::assistant("second")]);
    let second_error = client
        .stream(&second_request, Arc::clone(&cancellation))
        .expect_err("second scripted turn should return the provider error");
    assert_eq!(second_error, provider_error);
    assert_eq!(client.request_count(), 2);
    assert_eq!(
        client.last_message(),
        Some(&ModelMessage::assistant("second"))
    );
    assert_eq!(client.remaining_turns(), 0);

    let exhausted = client
        .stream(&request(Vec::new()), cancellation)
        .expect_err("missing scripted turn should fail closed");
    assert_eq!(exhausted.code, "fake_script_exhausted");
    assert!(!exhausted.retryable);
    assert_eq!(client.request_count(), 3);
    assert_eq!(client.last_message(), None);
}
