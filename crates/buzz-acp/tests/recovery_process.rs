//! Exercise the shipped harness process, not a journal/queue source shim.
//! A loopback protocol fixture supplies membership and one signed request;
//! a deterministic ACP adapter records prompts without performing any tools.
#![cfg(unix)]

use futures_util::{SinkExt, StreamExt};
use nostr::{Event, EventBuilder, Keys, Kind, Tag};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::time::{sleep, timeout};
use uuid::Uuid;

const ADAPTER: &str = r#"
import json, os, sys
for line in sys.stdin:
    request = json.loads(line)
    method = request.get('method')
    if 'id' not in request:
        continue
    if method == 'initialize':
        result = {'protocolVersion': 2, 'agentCapabilities': {}, 'agentInfo': {'name': 'recovery-probe', 'version': '1'}}
    elif method == 'session/new':
        result = {'sessionId': 'isolated-probe'}
    elif method == 'session/prompt':
        with open(os.environ['RECOVERY_PROBE_PROMPTS'], 'a') as out:
            out.write(json.dumps(request['params']) + '\n')
            out.flush()
            os.fsync(out.fileno())
        if os.environ['RECOVERY_PROBE_HOLD'] == '1':
            continue
        result = {'stopReason': 'end_turn'}
    else:
        result = {}
    print(json.dumps({'jsonrpc': '2.0', 'id': request['id'], 'result': result}), flush=True)
"#;

struct ProbeProcess(Child);

impl Drop for ProbeProcess {
    fn drop(&mut self) {
        // Kill only this test's owned process group, including its adapter.
        let _ = nix::sys::signal::killpg(
            nix::unistd::Pid::from_raw(self.0.id() as i32),
            nix::sys::signal::Signal::SIGKILL,
        );
        let _ = self.0.wait();
    }
}

struct ProbeDir(PathBuf);

impl Drop for ProbeDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn spawn_harness(
    root: &Path,
    relay: &str,
    keys: &Keys,
    phase: &str,
    hold: bool,
    policy: &str,
) -> ProbeProcess {
    use std::os::unix::process::CommandExt;

    let log = std::fs::File::create(root.join(format!("{phase}.log"))).unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_buzz-acp"));
    command
        .process_group(0)
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", root)
        .env("BUZZ_PRIVATE_KEY", keys.secret_key().to_secret_hex())
        .env("BUZZ_ACP_RECOVERY_PATH", root.join("journal.json"))
        .env(
            "RECOVERY_PROBE_PROMPTS",
            root.join(format!("{phase}.prompts")),
        )
        .env("RECOVERY_PROBE_HOLD", if hold { "1" } else { "0" })
        .args([
            "--relay-url",
            relay,
            "--agent-command",
            "python3",
            "--agent-args",
        ])
        .arg(root.join("adapter.py"))
        .args([
            "--respond-to",
            policy,
            "--subscribe",
            "all",
            "--no-mention-filter",
            "--no-memory",
            "--no-presence",
            "--no-typing",
            "--no-base-prompt",
            "--context-message-limit",
            "0",
            "--multiple-event-handling",
            "queue",
        ])
        .current_dir(root)
        .stdin(Stdio::null())
        .stdout(log.try_clone().unwrap())
        .stderr(log);
    ProbeProcess(command.spawn().unwrap())
}

async fn serve_connection(
    mut stream: TcpStream,
    event: Event,
    channel: Uuid,
    deliver: Arc<AtomicBool>,
) {
    let mut peek = [0; 4];
    if stream.peek(&mut peek).await.unwrap_or(0) < 4 {
        return;
    }
    if &peek == b"GET " {
        let Ok(mut ws) = tokio_tungstenite::accept_async(stream).await else {
            return;
        };
        let _ = ws
            .send(
                json!(["AUTH", "isolated-recovery-probe"])
                    .to_string()
                    .into(),
            )
            .await;
        while let Some(Ok(message)) = ws.next().await {
            let Ok(text) = message.to_text() else {
                continue;
            };
            let Ok(frame) = serde_json::from_str::<Value>(text) else {
                continue;
            };
            match frame[0].as_str() {
                Some("AUTH") | Some("EVENT") => {
                    let _ = ws
                        .send(json!(["OK", frame[1]["id"], true, ""]).to_string().into())
                        .await;
                }
                Some("REQ") => {
                    if frame[1] == format!("ch-{channel}") && deliver.swap(false, Ordering::SeqCst)
                    {
                        let _ = ws
                            .send(json!(["EVENT", frame[1], event]).to_string().into())
                            .await;
                    }
                    let _ = ws.send(json!(["EOSE", frame[1]]).to_string().into()).await;
                }
                _ => {}
            }
        }
        return;
    }
    let mut request = Vec::new();
    let mut byte = [0];
    while !request.ends_with(b"\r\n\r\n") && request.len() < 65536 {
        if stream.read_exact(&mut byte).await.is_err() {
            return;
        }
        request.push(byte[0]);
    }
    let headers = String::from_utf8_lossy(&request);
    let length = headers
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().ok())
                .flatten()
        })
        .unwrap_or(0);
    if length > 1024 * 1024 {
        return;
    }
    let mut body = vec![0; length];
    if stream.read_exact(&mut body).await.is_err() {
        return;
    }
    let filters: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let filters = filters.get("filters").unwrap_or(&filters);
    let response = if filters.to_string().contains("39002") {
        json!([{"tags": [["d", channel.to_string()]]}])
    } else if filters.to_string().contains("39000") {
        json!([{"tags": [["d", channel.to_string()], ["name", "isolated-probe"], ["t", "stream"]]}])
    } else {
        json!([])
    }
    .to_string();
    let wire = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}",
        response.len()
    );
    let _ = stream.write_all(wire.as_bytes()).await;
}

async fn wait_until(root: &Path, description: &str, check: impl Fn() -> bool) {
    if timeout(Duration::from_secs(25), async {
        while !check() {
            sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .is_err()
    {
        for entry in std::fs::read_dir(root).unwrap().flatten() {
            if entry.path().extension().is_some_and(|ext| ext == "log") {
                eprintln!(
                    "{}: {}",
                    entry.path().display(),
                    std::fs::read_to_string(entry.path()).unwrap_or_default()
                );
            }
        }
        panic!("timed out waiting for {description}");
    }
}

fn snapshot(root: &Path) -> Value {
    std::fs::read(root.join("journal.json"))
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or(Value::Null)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shipped_harness_recovers_after_kill_and_retires_completed_replay() {
    assert!(
        Command::new("python3")
            .arg("--version")
            .stdout(Stdio::null())
            .status()
            .unwrap()
            .success(),
        "python3 fixture required"
    );
    let root =
        ProbeDir(std::env::temp_dir().join(format!("buzz-recovery-process-{}", Uuid::new_v4())));
    std::fs::create_dir(&root.0).unwrap();
    std::fs::write(root.0.join("adapter.py"), ADAPTER).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let relay = format!("ws://{}", listener.local_addr().unwrap());
    let channel = Uuid::new_v4();
    let agent = Keys::generate();
    let event = EventBuilder::new(Kind::Custom(9), "resume-isolated-request")
        .tags([Tag::parse(["h".to_owned(), channel.to_string()]).unwrap()])
        .sign_with_keys(&Keys::generate())
        .unwrap();
    let id = event.id.to_hex();
    let deliver = Arc::new(AtomicBool::new(true));
    let send = deliver.clone();
    let server = tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            tokio::spawn(serve_connection(
                stream,
                event.clone(),
                channel,
                send.clone(),
            ));
        }
    });

    let first = spawn_harness(&root.0, &relay, &agent, "active", true, "anyone");
    wait_until(&root.0, "active adapter prompt", || {
        root.0.join("active.prompts").exists()
    })
    .await;
    assert_eq!(snapshot(&root.0)["pending"][0]["event"]["id"], id);
    drop(first); // SIGKILL the real harness and adapter, without shutdown hooks.

    // Changed author policy withholds unfinished work without deleting it.
    let denied = spawn_harness(&root.0, &relay, &agent, "denied", false, "nobody");
    wait_until(&root.0, "denied harness startup", || {
        std::fs::read_to_string(root.0.join("denied.log"))
            .unwrap_or_default()
            .contains("subscribed to channel")
    })
    .await;
    sleep(Duration::from_secs(2)).await;
    assert!(!root.0.join("denied.prompts").exists());
    assert_eq!(snapshot(&root.0)["pending"][0]["event"]["id"], id);
    drop(denied);

    // Relay does not resend the request: generation two must read its journal.
    let recovered = spawn_harness(&root.0, &relay, &agent, "recovered", false, "anyone");
    wait_until(&root.0, "durable completion", || {
        snapshot(&root.0)["completed"]
            .as_array()
            .is_some_and(|ids| ids.contains(&json!(id)))
    })
    .await;
    assert_eq!(snapshot(&root.0)["pending"], json!([]));
    let prompts = std::fs::read_to_string(root.0.join("recovered.prompts")).unwrap();
    assert_eq!(prompts.lines().count(), 1);
    assert!(prompts.contains("resume-isolated-request"));
    assert!(prompts.contains("recover"));
    drop(recovered);

    // A completed signed request replayed by the relay must not reach ACP.
    deliver.store(true, Ordering::SeqCst);
    let replay = spawn_harness(&root.0, &relay, &agent, "replay", false, "anyone");
    wait_until(&root.0, "completed event relay replay", || {
        !deliver.load(Ordering::SeqCst)
    })
    .await;
    sleep(Duration::from_secs(2)).await;
    assert!(!root.0.join("replay.prompts").exists());
    assert_eq!(snapshot(&root.0)["pending"], json!([]));
    drop(replay);
    server.abort();
}
