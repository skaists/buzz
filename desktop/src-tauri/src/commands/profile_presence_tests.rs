//! Drive the actual get_presence command through its authenticated HTTP query.
//! In particular, an error must not become a successful empty IPC snapshot.
use super::get_presence;
use crate::app_state::build_app_state;
use crate::relay_admission::{reset_rate_limit_gate, TEST_SERIAL};
use tauri::Manager;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Read one full HTTP request (headers + content-length body) off the stream.
async fn read_http_request(stream: &mut tokio::net::TcpStream) -> String {
    let mut request = Vec::new();
    loop {
        let mut buf = [0; 4096];
        let count = stream.read(&mut buf).await.unwrap();
        assert!(count > 0);
        request.extend_from_slice(&buf[..count]);
        assert!(request.len() < 16384);
        if let Some(end) = request.windows(4).position(|w| w == b"\r\n\r\n") {
            let headers = String::from_utf8_lossy(&request[..end]).to_lowercase();
            let length: usize = headers
                .lines()
                .find_map(|line| {
                    line.strip_prefix("content-length:")
                        .map(|v| v.trim().parse().unwrap())
                })
                // Bodyless requests (the GET /info preflight carries no
                // Content-Length) read zero bytes past the header block.
                .unwrap_or(0);
            if request.len() >= end + 4 + length {
                break;
            }
        }
    }
    String::from_utf8(request).unwrap()
}

#[tokio::test]
async fn presence_command_preserves_query_failure_and_successful_absence() {
    let _serial = TEST_SERIAL.lock().await;
    reset_rate_limit_gate();
    for (status, body) in [
        ("200 OK", "[]"),
        ("401 Unauthorized", r#"{"error":"unauthorized"}"#),
        ("429 Too Many Requests", r#"{"error":"retry in 1s"}"#),
        (
            "500 Internal Server Error",
            r#"{"error":"storage unavailable"}"#,
        ),
        ("200 OK", "not json"),
    ] {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            // Canonical-origin preflight: the signed HTTP client resolves the
            // relay's /info document before its first NIP-98 call. Serve the
            // no-canonical-advertisement document ({}), which keeps signing
            // against the transport host, then answer the real query.
            let (mut stream, _) = listener.accept().await.unwrap();
            let preflight = read_http_request(&mut stream).await;
            assert!(preflight.starts_with("GET /info "));
            let info = "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}";
            stream.write_all(info.as_bytes()).await.unwrap();

            let (mut stream, _) = listener.accept().await.unwrap();
            let request = read_http_request(&mut stream).await;
            assert!(request.starts_with("POST /query "));
            assert!(request.to_lowercase().contains("authorization: nostr "));
            assert!(request.contains("20001"));
            let response = format!("HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
            stream.write_all(response.as_bytes()).await.unwrap();
        });
        let state = build_app_state();
        *state.relay_url_override.lock().unwrap() = Some(format!("ws://{addr}"));
        let app = tauri::test::mock_builder()
            .manage(state)
            .build(tauri::test::mock_context(tauri::test::noop_assets()))
            .unwrap();
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            get_presence(vec!["a".repeat(64)], app.state()),
        )
        .await
        .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), server)
            .await
            .unwrap()
            .unwrap();
        if status == "200 OK" && body == "[]" {
            assert_eq!(
                serde_json::to_value(result.unwrap()).unwrap(),
                serde_json::json!({})
            );
        } else {
            assert!(
                result.is_err(),
                "{status} / {body} must reject, not return Offline: {result:?}"
            );
        }
        reset_rate_limit_gate();
    }
}

#[tokio::test]
async fn presence_command_transport_failure_is_not_offline() {
    let _serial = TEST_SERIAL.lock().await;
    reset_rate_limit_gate();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);
    let state = build_app_state();
    *state.relay_url_override.lock().unwrap() = Some(format!("ws://{addr}"));
    let app = tauri::test::mock_builder()
        .manage(state)
        .build(tauri::test::mock_context(tauri::test::noop_assets()))
        .unwrap();
    let result = get_presence(vec!["a".repeat(64)], app.state()).await;
    assert!(result.is_err(), "transport failure must reject: {result:?}");
    // Empty input does not require a relay and remains a genuine empty result.
    assert!(get_presence(vec![], app.state()).await.unwrap().is_empty());
}
