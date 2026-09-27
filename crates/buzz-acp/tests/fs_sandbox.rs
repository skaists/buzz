//! `buzz-acp sandbox-exec` is the filesystem boundary for isolated agents:
//! env isolation keeps credentials out of the agent's environment, but only
//! this keeps the agent from reading them off disk (`~/.ssh`,
//! `~/.cargo/credentials.toml`, wallet files). These tests run the real
//! launcher binary and check what the agent can and cannot touch, with an
//! unsandboxed control run so they fail if the boundary disappears.
#![cfg(target_os = "linux")]

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const LAUNCHER: &str = env!("CARGO_BIN_EXE_buzz-acp");

fn scratch(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "buzz-acp-fs-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn sandboxed(workspace: &Path, script: &str) -> Output {
    // The harness's real defaults, so a leak in them fails these tests.
    let mut cmd = Command::new(LAUNCHER);
    cmd.arg("sandbox-exec").arg("--rw").arg(workspace);
    for rw in buzz_acp::DEFAULT_READ_WRITE
        .iter()
        .filter(|p| Path::new(p).exists())
    {
        cmd.args(["--rw", rw]);
    }
    for ro in buzz_acp::DEFAULT_READ_ONLY
        .iter()
        .filter(|p| Path::new(p).exists())
    {
        cmd.args(["--ro", ro]);
    }
    cmd.args(["--", "/bin/sh", "-c", script]);
    cmd.output().expect("run launcher")
}

/// `true` when the agent started under an enforced boundary (it touched the
/// marker); `false` when the launcher refused to start. Either is
/// acceptable; running without the boundary is not, and the launcher only
/// execs the agent after full enforcement.
fn enforced_or_refused(out: &Output, marker: &Path) -> bool {
    if marker.exists() {
        return true;
    }
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !out.status.success() && stderr.contains("Landlock"),
        "agent did not start and the launcher gave no Landlock refusal: {stderr}"
    );
    eprintln!("Landlock unavailable on this kernel; verified fail-closed refusal only");
    false
}

#[test]
fn agent_cannot_read_list_or_write_outside_its_grants() {
    let secrets = scratch("secrets");
    let workspace = scratch("ws");
    let key = secrets.join("credentials.toml");
    std::fs::write(&key, "token = \"host-secret\"").unwrap();
    let marker = workspace.join("ran");

    let script = format!(
        "touch '{marker}'; \
         cat '{key}' > '{ws}/read.out' 2>&1; \
         ls '{secrets}' > '{ws}/ls.out' 2>&1; \
         echo planted > '{secrets}/planted' 2> '{ws}/write.err'; \
         echo ok > '{ws}/workspace-write'",
        marker = marker.display(),
        key = key.display(),
        secrets = secrets.display(),
        ws = workspace.display(),
    );

    // Control: without the launcher the same script reads the secret.
    let control_ws = scratch("control");
    let control = Command::new("/bin/sh")
        .args([
            "-c",
            &script.replace(&*workspace.to_string_lossy(), &control_ws.to_string_lossy()),
        ])
        .output()
        .unwrap();
    assert!(control.status.success());
    assert!(std::fs::read_to_string(control_ws.join("read.out"))
        .unwrap()
        .contains("host-secret"));
    std::fs::remove_file(secrets.join("planted")).unwrap();

    let out = sandboxed(&workspace, &script);
    if enforced_or_refused(&out, &marker) {
        assert!(marker.exists(), "agent should run inside the sandbox");
        let read = std::fs::read_to_string(workspace.join("read.out")).unwrap();
        assert!(!read.contains("host-secret"), "secret leaked: {read}");
        assert!(read.contains("Permission denied"), "{read}");
        let listing = std::fs::read_to_string(workspace.join("ls.out")).unwrap();
        assert!(
            !listing.contains("credentials.toml"),
            "listing leaked: {listing}"
        );
        assert!(
            !secrets.join("planted").exists(),
            "agent must not write outside its grants"
        );
        assert!(
            workspace.join("workspace-write").exists(),
            "agent must still write its workspace"
        );
    }
    for dir in [secrets, workspace, control_ws] {
        let _ = std::fs::remove_dir_all(dir);
    }
}

#[test]
fn restrictions_are_inherited_by_grandchildren() {
    // Agents shell out constantly; the boundary must hold for every process
    // they start, not only the first.
    let secrets = scratch("secrets-gc");
    let workspace = scratch("ws-gc");
    let key = secrets.join("id_ed25519");
    std::fs::write(&key, "PRIVATE KEY").unwrap();
    let marker = workspace.join("ran");
    let script = format!(
        "touch '{marker}'; /bin/sh -c \"/bin/sh -c 'cat {key}'\" > '{ws}/gc.out' 2>&1",
        marker = marker.display(),
        key = key.display(),
        ws = workspace.display(),
    );
    let out = sandboxed(&workspace, &script);
    if enforced_or_refused(&out, &marker) {
        let read = std::fs::read_to_string(workspace.join("gc.out")).unwrap();
        assert!(
            !read.contains("PRIVATE KEY"),
            "grandchild read the key: {read}"
        );
    }
    for dir in [secrets, workspace] {
        let _ = std::fs::remove_dir_all(dir);
    }
}

#[test]
fn agent_cannot_read_another_processs_environment_via_proc() {
    // The harness keeps its full environment even when agents get a filtered
    // one; /proc/<pid>/environ must not hand it back to the agent.
    let workspace = scratch("ws-proc");
    let marker = workspace.join("ran");
    let mut holder = Command::new("/bin/sleep")
        .arg("30")
        .env("BUZZ_ACP_TEST_HARNESS_SECRET", "harness-only-value")
        .spawn()
        .unwrap();
    let pid = holder.id();

    let script = format!(
        "touch '{marker}'; \
         {{ tr '\\0' '\\n' < /proc/{pid}/environ; }} > '{ws}/environ.out' 2>&1; \
         cat /proc/cpuinfo > /dev/null && echo cpuinfo-ok > '{ws}/cpuinfo'; \
         IFS= read -r first < /proc/$$/status && echo self-ok > '{ws}/self'",
        marker = marker.display(),
        ws = workspace.display(),
    );
    // Control: unsandboxed, the same read succeeds.
    let control = Command::new("/bin/sh")
        .args(["-c", &format!("tr '\\0' '\\n' < /proc/{pid}/environ")])
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&control.stdout).contains("harness-only-value"));

    let out = sandboxed(&workspace, &script);
    let _ = holder.kill();
    let _ = holder.wait();
    if enforced_or_refused(&out, &marker) {
        let leaked = std::fs::read_to_string(workspace.join("environ.out")).unwrap();
        assert!(
            !leaked.contains("harness-only-value") && leaked.contains("Permission denied"),
            "agent read another process's environment: {leaked}"
        );
        assert!(
            workspace.join("cpuinfo").exists(),
            "system /proc files stay readable"
        );
        assert!(
            workspace.join("self").exists(),
            "the agent process can read its own /proc entry (a shell builtin, so no child pid)"
        );
    }
    let _ = std::fs::remove_dir_all(workspace);
}

#[test]
fn launcher_refuses_malformed_invocations_without_running_anything() {
    let out = Command::new(LAUNCHER)
        .args(["sandbox-exec", "--rw"])
        .output()
        .unwrap();
    assert!(!out.status.success());
}
