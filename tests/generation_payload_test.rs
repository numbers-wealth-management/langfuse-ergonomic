//! Mock tests verifying that generation and trace ingestion payloads include the
//! native Langfuse fields (`usageDetails`, `promptName`, `promptVersion`,
//! `environment`, etc.) emitted by the ergonomic builders.

use langfuse_ergonomic::{ClientBuilder, GenerationUsageDetails, LangfuseClient};
use mockito::{Matcher, Server};
use serde_json::{json, Value};

fn create_mock_client(server: &Server) -> LangfuseClient {
    ClientBuilder::new()
        .public_key("pk-lf-test")
        .secret_key("sk-lf-test")
        .base_url(server.url())
        .build()
        .expect("mock credentials should be valid")
}

/// Pull the first body of the requested ingestion event type out of a captured
/// `/api/public/ingestion` request body.
fn first_event_body<'a>(batch: &'a Value, event_type: &str) -> &'a Value {
    let events = batch
        .get("batch")
        .and_then(|b| b.as_array())
        .expect("batch array");
    let event = events
        .iter()
        .find(|e| e.get("type").and_then(|t| t.as_str()) == Some(event_type))
        .unwrap_or_else(|| panic!("no event with type {} in batch: {}", event_type, batch));
    event.get("body").expect("event has body")
}

#[tokio::test]
async fn generation_payload_includes_native_usage_prompt_and_environment() {
    let mut server = Server::new_async().await;

    let mock = server
        .mock("POST", "/api/public/ingestion")
        .match_body(Matcher::PartialJson(json!({
            "batch": [
                {
                    "type": "generation-create",
                    "body": {
                        "promptName": "advisor.step.summarize",
                        "promptVersion": 7,
                        "environment": "staging",
                        "usageDetails": {
                            "input": 120,
                            "output": 45,
                            "total": 165,
                        },
                    }
                }
            ]
        })))
        .with_status(207)
        .with_header("content-type", "application/json")
        .with_body(r#"{"successes": [], "errors": []}"#)
        .create_async()
        .await;

    let client = create_mock_client(&server);

    let result = client
        .generation()
        .trace_id("trace-abc")
        .id("gen-abc")
        .name("advisor-step")
        .model("claude-opus-4-7")
        .input(json!({"prompt": "hello"}))
        .output(json!({"response": "hi"}))
        .usage_details(GenerationUsageDetails::new(120, 45))
        .prompt_name("advisor.step.summarize")
        .prompt_version(7)
        .environment("staging")
        .call()
        .await;

    mock.assert_async().await;
    assert!(
        result.is_ok(),
        "generation call should succeed: {:?}",
        result
    );
}

#[tokio::test]
async fn generation_payload_falls_back_to_token_counts() {
    // When the caller provides only the legacy prompt/completion token counts,
    // the SDK should derive a usageDetails object so the native Langfuse field
    // is still populated.
    let mut server = Server::new_async().await;

    let mock = server
        .mock("POST", "/api/public/ingestion")
        .match_body(Matcher::PartialJson(json!({
            "batch": [
                {
                    "type": "generation-create",
                    "body": {
                        "usageDetails": {
                            "input": 10,
                            "output": 20,
                            "total": 30,
                        }
                    }
                }
            ]
        })))
        .with_status(207)
        .with_header("content-type", "application/json")
        .with_body(r#"{"successes": [], "errors": []}"#)
        .create_async()
        .await;

    let client = create_mock_client(&server);

    let result = client
        .generation()
        .trace_id("trace-abc")
        .name("legacy-tokens")
        .prompt_tokens(10)
        .completion_tokens(20)
        .call()
        .await;

    mock.assert_async().await;
    assert!(
        result.is_ok(),
        "legacy-token generation should succeed: {:?}",
        result
    );
}

#[tokio::test]
async fn update_generation_payload_includes_error_and_usage_details() {
    let mut server = Server::new_async().await;

    let captured = std::sync::Arc::new(std::sync::Mutex::new(None::<Value>));
    let captured_clone = captured.clone();

    let mock = server
        .mock("POST", "/api/public/ingestion")
        .with_status(207)
        .with_header("content-type", "application/json")
        .with_body_from_request(move |request| {
            let raw = request.body().expect("request body bytes");
            let body: Value = serde_json::from_slice(raw).expect("json body");
            *captured_clone.lock().unwrap() = Some(body);
            br#"{"successes": [], "errors": []}"#.to_vec()
        })
        .create_async()
        .await;

    let client = create_mock_client(&server);

    let result = client
        .update_generation()
        .id("gen-abc")
        .trace_id("trace-abc")
        .level("ERROR".to_string())
        .status_message("malformed JSON: expected `{`".to_string())
        .usage_details(GenerationUsageDetails::new(80, 30))
        .environment("staging")
        .prompt_name("advisor.step.summarize")
        .prompt_version(7)
        .call()
        .await;

    mock.assert_async().await;
    assert!(
        result.is_ok(),
        "update_generation should succeed: {:?}",
        result
    );

    let body = captured
        .lock()
        .unwrap()
        .clone()
        .expect("ingestion request body captured");
    let event_body = first_event_body(&body, "generation-update");

    assert_eq!(
        event_body.get("id").and_then(Value::as_str),
        Some("gen-abc")
    );
    assert_eq!(
        event_body.get("traceId").and_then(Value::as_str),
        Some("trace-abc")
    );
    assert_eq!(
        event_body.get("level").and_then(Value::as_str),
        Some("ERROR")
    );
    let status = event_body
        .get("statusMessage")
        .and_then(Value::as_str)
        .expect("statusMessage present");
    assert!(status.contains("malformed JSON"), "status was {}", status);
    assert_eq!(
        event_body.get("environment").and_then(Value::as_str),
        Some("staging")
    );
    assert_eq!(
        event_body.get("promptName").and_then(Value::as_str),
        Some("advisor.step.summarize")
    );
    assert_eq!(
        event_body.get("promptVersion").and_then(Value::as_i64),
        Some(7)
    );

    let usage = event_body
        .get("usageDetails")
        .expect("usageDetails present");
    assert_eq!(usage.get("input").and_then(Value::as_i64), Some(80));
    assert_eq!(usage.get("output").and_then(Value::as_i64), Some(30));
    assert_eq!(usage.get("total").and_then(Value::as_i64), Some(110));
}

#[tokio::test]
async fn trace_payload_includes_environment() {
    let mut server = Server::new_async().await;

    let mock = server
        .mock("POST", "/api/public/ingestion")
        .match_body(Matcher::PartialJson(json!({
            "batch": [
                {
                    "type": "trace-create",
                    "body": {
                        "environment": "staging",
                    }
                }
            ]
        })))
        .with_status(207)
        .with_header("content-type", "application/json")
        .with_body(r#"{"successes": [], "errors": []}"#)
        .create_async()
        .await;

    let client = create_mock_client(&server);

    let result = client
        .trace()
        .name("advisor-workflow")
        .environment("staging")
        .call()
        .await;

    mock.assert_async().await;
    assert!(
        result.is_ok(),
        "trace call should succeed: {}",
        result.err().map(|e| e.to_string()).unwrap_or_default()
    );
}
