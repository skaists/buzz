use sha2::{Digest, Sha256};

use crate::client::{
    extract_d_tag, extract_relay_response_field, normalize_write_response, print_create_response,
    BuzzClient,
};
use crate::error::CliError;
use crate::validate::{parse_uuid, read_or_stdin, sdk_err, validate_uuid};

// TODO(phase-4): Replace raw nostr::EventBuilder usage with buzz-sdk builder functions

/// List workflows in a channel — query kind:30620 workflow definition events.
pub async fn cmd_list_workflows(client: &BuzzClient, channel_id: &str) -> Result<(), CliError> {
    validate_uuid(channel_id)?;
    let filter = serde_json::json!({
        "kinds": [30620],
        "#h": [channel_id]
    });
    let resp = client.query(&filter).await?;
    let events: Vec<serde_json::Value> = serde_json::from_str(&resp).unwrap_or_default();
    let workflows: Vec<serde_json::Value> = events
        .iter()
        .map(|e| {
            serde_json::json!({
                "workflow_id": extract_d_tag(e),
                "content": e.get("content").and_then(|v| v.as_str()).unwrap_or(""),
                "created_at": e.get("created_at").and_then(|v| v.as_u64()).unwrap_or(0),
                "pubkey": e.get("pubkey").and_then(|v| v.as_str()).unwrap_or(""),
            })
        })
        .collect();
    let output = serde_json::to_string(&workflows).unwrap_or_default();
    println!("{output}");
    Ok(())
}

/// Get a single workflow definition.
pub async fn cmd_get_workflow(client: &BuzzClient, workflow_id: &str) -> Result<(), CliError> {
    validate_uuid(workflow_id)?;
    let filter = serde_json::json!({
        "kinds": [30620],
        "#d": [workflow_id]
    });
    let resp = client.query(&filter).await?;
    let events: Vec<serde_json::Value> = serde_json::from_str(&resp).unwrap_or_default();
    if let Some(e) = events.first() {
        let normalized = serde_json::json!({
            "workflow_id": extract_d_tag(e),
            "content": e.get("content").and_then(|v| v.as_str()).unwrap_or(""),
            "created_at": e.get("created_at").and_then(|v| v.as_u64()).unwrap_or(0),
            "pubkey": e.get("pubkey").and_then(|v| v.as_str()).unwrap_or(""),
        });
        println!("{normalized}");
    } else {
        println!("null");
    }
    Ok(())
}

/// Fetch workflow run history from `GET /workflows/{workflow_id}/runs` (NIP-98).
///
/// Run history lives in the relay's `workflow_runs` table, not in Nostr events:
/// the relay emits no 46001–46003 lifecycle events, so an event query can never
/// return a run. Returns the newest-first `runs` array of one page.
async fn fetch_workflow_runs(
    client: &BuzzClient,
    workflow_id: &str,
    limit: Option<u32>,
) -> Result<Vec<serde_json::Value>, CliError> {
    validate_uuid(workflow_id)?;
    let limit = limit.unwrap_or(20).clamp(1, 100);
    let resp = client
        .get_authed(&format!("/workflows/{workflow_id}/runs?limit={limit}"))
        .await?;
    let body: serde_json::Value = serde_json::from_str(&resp)
        .map_err(|e| CliError::Other(format!("workflow runs response is not JSON: {e}")))?;
    body.get("runs")
        .and_then(|runs| runs.as_array())
        .cloned()
        .ok_or_else(|| CliError::Other("workflow runs response has no `runs` array".into()))
}

/// Get workflow run history — one page of runs, newest first.
pub async fn cmd_get_workflow_runs(
    client: &BuzzClient,
    workflow_id: &str,
    limit: Option<u32>,
) -> Result<(), CliError> {
    let runs = fetch_workflow_runs(client, workflow_id, limit).await?;
    let output = serde_json::to_string(&runs).unwrap_or_default();
    println!("{output}");
    Ok(())
}

/// Create a workflow — sign and submit a kind:30620 event.
pub async fn cmd_create_workflow(
    client: &BuzzClient,
    channel_id: &str,
    yaml: &str,
) -> Result<(), CliError> {
    let channel_uuid = parse_uuid(channel_id)?;
    let yaml_definition = read_or_stdin(yaml)?;

    let workflow_id = uuid::Uuid::new_v4();
    let builder = buzz_sdk::build_workflow_def(channel_uuid, workflow_id, &yaml_definition)
        .map_err(sdk_err)?;
    let event = client.sign_event(builder)?;

    let resp = client.submit_event(event).await?;
    let final_workflow_id = extract_relay_response_field(&resp, "workflow_id")
        .unwrap_or_else(|| workflow_id.to_string());
    print_create_response(&resp, "workflow_id", &final_workflow_id);
    Ok(())
}

/// Update a workflow — sign and submit an updated kind:30620 event with same d-tag.
pub async fn cmd_update_workflow(
    client: &BuzzClient,
    channel_id: &str,
    workflow_id: &str,
    yaml: &str,
) -> Result<(), CliError> {
    let channel_uuid = parse_uuid(channel_id)?;
    let wf_uuid = parse_uuid(workflow_id)?;
    let yaml_definition = read_or_stdin(yaml)?;

    let builder = buzz_sdk::build_workflow_update(channel_uuid, wf_uuid, &yaml_definition)
        .map_err(sdk_err)?;
    let event = client.sign_event(builder)?;

    let resp = client.submit_event(event).await?;
    println!("{}", normalize_write_response(&resp));
    Ok(())
}

/// Delete a workflow — sign and submit a kind:5 deletion event.
pub async fn cmd_delete_workflow(client: &BuzzClient, workflow_id: &str) -> Result<(), CliError> {
    let wf_uuid = parse_uuid(workflow_id)?;
    let keys = client.keys();

    let builder =
        buzz_sdk::build_workflow_delete(&keys.public_key().to_hex(), wf_uuid).map_err(sdk_err)?;
    let event = client.sign_event(builder)?;

    let resp = client.submit_event(event).await?;
    println!("{}", normalize_write_response(&resp));
    Ok(())
}

/// Trigger a workflow — sign and submit a kind:46020 event.
///
/// When `inputs` is provided, it is parsed as a JSON object and used as the
/// event content (MCP parity). When omitted, the event content is `{}`.
pub async fn cmd_trigger_workflow(
    client: &BuzzClient,
    workflow_id: &str,
    inputs: Option<&str>,
) -> Result<(), CliError> {
    let wf_uuid = parse_uuid(workflow_id)?;

    if let Some(raw) = inputs {
        // Parse and validate it is a JSON object, then build the event manually
        // so we can embed the inputs as the event content.
        let parsed: serde_json::Value = serde_json::from_str(raw)
            .map_err(|e| CliError::Usage(format!("--inputs is not valid JSON: {e}")))?;
        if !parsed.is_object() {
            return Err(CliError::Usage("--inputs must be a JSON object".into()));
        }
        let content = serde_json::to_string(&parsed).unwrap_or_default();
        use nostr::{EventBuilder, Kind, Tag};
        let tags = vec![Tag::parse(["d", &wf_uuid.to_string()])
            .map_err(|e| CliError::Other(format!("tag error: {e}")))?];
        let builder = EventBuilder::new(
            Kind::Custom(buzz_sdk::kind::KIND_WORKFLOW_TRIGGER as u16),
            &content,
        )
        .tags(tags);
        let event = client.sign_event(builder)?;
        let resp = client.submit_event(event).await?;
        println!("{}", normalize_write_response(&resp));
    } else {
        let builder = buzz_sdk::build_workflow_trigger(wf_uuid).map_err(sdk_err)?;
        let event = client.sign_event(builder)?;
        let resp = client.submit_event(event).await?;
        println!("{}", normalize_write_response(&resp));
    }
    Ok(())
}

/// Approve or deny a workflow step — sign and submit a kind:46030 (grant) or 46031 (deny) event.
pub async fn cmd_approve_step(
    client: &BuzzClient,
    approval_token: &str,
    approved: bool,
    note: Option<&str>,
    binding: buzz_sdk::ApprovalBinding<'_>,
) -> Result<(), CliError> {
    validate_uuid(approval_token)?;

    let content = note.unwrap_or("");

    // The relay expects d-tag = hex(SHA256(token)), not the raw token UUID.
    let token_hash = hex::encode(Sha256::digest(approval_token.as_bytes()));
    let builder = buzz_sdk::build_workflow_approval_bound(&token_hash, approved, content, &binding)
        .map_err(sdk_err)?;
    let event = client.sign_event(builder)?;

    let resp = client.submit_event(event).await?;
    println!("{}", normalize_write_response(&resp));
    Ok(())
}

pub async fn dispatch(cmd: crate::WorkflowsCmd, client: &BuzzClient) -> Result<(), CliError> {
    use crate::WorkflowsCmd;
    match cmd {
        WorkflowsCmd::List { channel } => cmd_list_workflows(client, &channel).await,
        WorkflowsCmd::Get { workflow } => cmd_get_workflow(client, &workflow).await,
        WorkflowsCmd::Create { channel, yaml } => {
            cmd_create_workflow(client, &channel, &yaml).await
        }
        WorkflowsCmd::Update {
            channel,
            workflow,
            yaml,
        } => cmd_update_workflow(client, &channel, &workflow, &yaml).await,
        WorkflowsCmd::Delete { workflow } => cmd_delete_workflow(client, &workflow).await,
        WorkflowsCmd::Trigger { workflow, inputs } => {
            cmd_trigger_workflow(client, &workflow, inputs.as_deref()).await
        }
        WorkflowsCmd::Runs { workflow, limit } => {
            cmd_get_workflow_runs(client, &workflow, limit).await
        }
        WorkflowsCmd::Approve {
            token,
            approved,
            note,
            candidate,
            run,
            step,
        } => {
            // approved is already a bool — no parse_bool_flag needed
            let binding = buzz_sdk::ApprovalBinding {
                run_id: run.as_deref(),
                step_id: step.as_deref(),
                candidate: candidate.as_deref(),
            };
            cmd_approve_step(client, &token, approved, note.as_deref(), binding).await
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::{HeaderMap, Uri};
    use axum::Router;
    use nostr::Keys;
    use std::sync::{Arc, Mutex};
    use tokio::net::TcpListener;

    const WORKFLOW_ID: &str = "11111111-2222-4333-8444-555555555555";

    /// Serve `body` for every request and record `METHOD path?query auth-scheme`.
    async fn recording_server(body: &'static str) -> (String, Arc<Mutex<Vec<String>>>) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = seen.clone();
        let app = Router::new().fallback(
            move |method: axum::http::Method, uri: Uri, headers: HeaderMap| {
                let log = log.clone();
                async move {
                    let scheme = headers
                        .get("authorization")
                        .and_then(|v| v.to_str().ok())
                        .and_then(|v| v.split(' ').next())
                        .unwrap_or("-")
                        .to_string();
                    log.lock().unwrap().push(format!("{method} {uri} {scheme}"));
                    ([("content-type", "application/json")], body)
                }
            },
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (format!("http://{addr}"), seen)
    }

    fn test_client(base_url: &str) -> BuzzClient {
        BuzzClient::new(base_url.to_string(), Keys::generate(), None, None).unwrap()
    }

    #[tokio::test]
    async fn runs_are_read_from_the_relay_run_history_endpoint() {
        let (url, seen) = recording_server(
            r#"{"runs":[{"id":"r2","status":"waiting_approval"},{"id":"r1","status":"completed"}],"next":null}"#,
        )
        .await;
        let runs = fetch_workflow_runs(&test_client(&url), WORKFLOW_ID, Some(5))
            .await
            .unwrap();

        assert_eq!(runs.len(), 2);
        assert_eq!(runs[0]["id"], "r2");
        assert_eq!(runs[0]["status"], "waiting_approval");
        assert_eq!(
            *seen.lock().unwrap(),
            vec![format!("GET /workflows/{WORKFLOW_ID}/runs?limit=5 Nostr")],
            "exactly one NIP-98 GET to the run-history endpoint, and no event query"
        );
    }

    #[tokio::test]
    async fn runs_limit_defaults_to_20_and_stays_inside_the_relay_bounds() {
        let (url, seen) = recording_server(r#"{"runs":[],"next":null}"#).await;
        let client = test_client(&url);
        for limit in [None, Some(0), Some(500)] {
            assert!(fetch_workflow_runs(&client, WORKFLOW_ID, limit)
                .await
                .unwrap()
                .is_empty());
        }
        let limits: Vec<String> = seen
            .lock()
            .unwrap()
            .iter()
            .map(|line| line.split("limit=").nth(1).unwrap().to_string())
            .collect();
        assert_eq!(limits, ["20 Nostr", "1 Nostr", "100 Nostr"]);
    }

    #[tokio::test]
    async fn runs_response_without_a_runs_array_is_an_error_not_an_empty_list() {
        let (url, _) = recording_server(r#"{"error":"nope"}"#).await;
        let err = fetch_workflow_runs(&test_client(&url), WORKFLOW_ID, None)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("no `runs` array"), "{err}");
    }

    #[tokio::test]
    async fn runs_rejects_a_malformed_workflow_id_before_any_request() {
        let (url, seen) = recording_server(r#"{"runs":[]}"#).await;
        assert!(fetch_workflow_runs(&test_client(&url), "not-a-uuid", None)
            .await
            .is_err());
        assert!(seen.lock().unwrap().is_empty());
    }
}
