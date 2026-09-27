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
    sandboxed_with(&[workspace], script)
}

fn default_grants(cmd: &mut Command) {
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
}

/// Run `script` under the launcher with the harness's real default grants
/// plus `rw` (the workspace, and e.g. the agent Cargo home), so a leak in
/// the defaults fails these tests.
fn sandboxed_with(rw: &[&Path], script: &str) -> Output {
    let mut cmd = Command::new(LAUNCHER);
    cmd.arg("sandbox-exec");
    for dir in rw {
        cmd.arg("--rw").arg(dir);
    }
    default_grants(&mut cmd);
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
        !out.status.success() && stderr.contains("kernel cannot enforce Landlock"),
        "agent did not start and the launcher gave no kernel refusal: {stderr}"
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
fn probe_reports_kernel_support_or_a_kernel_refusal() {
    // Fact 1: the minimum ABI and every promised right are enforceable, or
    // the probe fails closed with an error that names the kernel.
    let mut cmd = Command::new(LAUNCHER);
    cmd.arg("sandbox-exec");
    default_grants(&mut cmd);
    let out = cmd.arg("--check").output().unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success() || stderr.contains("kernel cannot enforce Landlock"),
        "probe failed without a kernel classification: {stderr}"
    );
    eprintln!(
        "landlock probe on this kernel: {}",
        if out.status.success() {
            "enforced"
        } else {
            "refused"
        }
    );
}

#[test]
fn bad_grants_are_a_configuration_error_not_a_kernel_error() {
    // Operators must know whether to fix the Buzz config or the kernel.
    let out = Command::new(LAUNCHER)
        .args([
            "sandbox-exec",
            "--ro",
            "/definitely/not/a/grant/path",
            "--check",
        ])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success());
    assert!(stderr.contains("configuration error"), "{stderr}");
    assert!(!stderr.contains("kernel cannot enforce"), "{stderr}");
}

#[test]
fn workspace_and_agent_cargo_home_stay_writable() {
    // Fact 2 (positive half): the grants the agent needs actually work.
    let workspace = scratch("ws-cargo");
    let cargo_home = scratch("cargo-home");
    let marker = workspace.join("ran");
    let script = format!(
        "touch '{marker}'; echo ok > '{ws}/w' && echo ok > '{ch}/registry-cache' && \
         cat '{ch}/registry-cache' > '{ws}/r'",
        marker = marker.display(),
        ws = workspace.display(),
        ch = cargo_home.display(),
    );
    let out = sandboxed_with(&[&workspace, &cargo_home], &script);
    if enforced_or_refused(&out, &marker) {
        assert!(workspace.join("w").exists());
        assert!(cargo_home.join("registry-cache").exists());
        assert_eq!(
            std::fs::read_to_string(workspace.join("r")).unwrap(),
            "ok\n"
        );
    }
    for dir in [workspace, cargo_home] {
        let _ = std::fs::remove_dir_all(dir);
    }
}

#[test]
fn symlink_inside_workspace_does_not_reach_outside_it() {
    // A link planted in an allowed directory must not become a door to a
    // forbidden file: Landlock checks the resolved target, not the link.
    let secrets = scratch("secrets-link");
    let workspace = scratch("ws-link");
    let key = secrets.join("wallet.json");
    std::fs::write(&key, "{\"seed\": \"host-wallet-secret\"}").unwrap();
    std::os::unix::fs::symlink(&key, workspace.join("wallet-link")).unwrap();
    std::os::unix::fs::symlink(&secrets, workspace.join("dir-link")).unwrap();
    let marker = workspace.join("ran");
    let script = format!(
        "touch '{marker}'; \
         {{ cat '{ws}/wallet-link'; cat '{ws}/dir-link/wallet.json'; }} > '{ws}/link.out' 2>&1",
        marker = marker.display(),
        ws = workspace.display(),
    );
    // Control: unsandboxed, the link reads the secret.
    let control = Command::new("/bin/cat")
        .arg(workspace.join("wallet-link"))
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&control.stdout).contains("host-wallet-secret"));

    let out = sandboxed(&workspace, &script);
    if enforced_or_refused(&out, &marker) {
        let read = std::fs::read_to_string(workspace.join("link.out")).unwrap();
        assert!(
            !read.contains("host-wallet-secret"),
            "symlink escaped: {read}"
        );
        assert_eq!(read.matches("Permission denied").count(), 2, "{read}");
    }
    for dir in [secrets, workspace] {
        let _ = std::fs::remove_dir_all(dir);
    }
}

#[test]
fn real_tools_spawned_by_the_agent_cannot_read_outside_it() {
    // Fact 3 through ordinary tool chains: agent shell → child shell →
    // grandchild tool (python3, perl, git). Escaping the policy by launching
    // a different executable must not work.
    let secrets = scratch("secrets-tools");
    let workspace = scratch("ws-tools");
    let key = secrets.join("credentials.toml");
    std::fs::write(&key, "token = \"tool-secret\"").unwrap();
    let marker = workspace.join("ran");
    let k = key.display();
    let tools: Vec<(&str, String)> = [
        ("python3", format!("python3 -c 'print(open(\"{k}\").read())'")),
        ("perl", format!("perl -e 'open(F, \"<{k}\") or die \\$!; print <F>'")),
        ("git", format!("git hash-object --no-filters '{k}' && git --no-pager diff --no-index /dev/null '{k}'")),
    ]
    .into_iter()
    .filter(|(tool, _)| Command::new(tool).arg("--version").output().is_ok())
    .collect();
    assert!(
        tools.len() >= 2,
        "need at least two of python3/perl/git to test tool chains"
    );

    let mut script = format!("touch '{}';", marker.display());
    for (tool, cmd) in &tools {
        script.push_str(&format!(
            " /bin/sh -c \"{}\" > '{}/{tool}.out' 2>&1;",
            cmd.replace('\"', "\\\""),
            workspace.display()
        ));
    }
    // Control: unsandboxed, each tool reads the secret.
    let control_ws = scratch("ws-tools-control");
    let control = Command::new("/bin/sh")
        .args([
            "-c",
            &script.replace(&*workspace.to_string_lossy(), &control_ws.to_string_lossy()),
        ])
        .output()
        .unwrap();
    assert!(control.status.success() || control_ws.join("ran").exists());
    for (tool, _) in &tools {
        let out = std::fs::read_to_string(control_ws.join(format!("{tool}.out"))).unwrap();
        assert!(out.contains("tool-secret"), "{tool} control: {out}");
    }

    let out = sandboxed(&workspace, &script);
    if enforced_or_refused(&out, &marker) {
        for (tool, _) in &tools {
            let read = std::fs::read_to_string(workspace.join(format!("{tool}.out"))).unwrap();
            assert!(
                !read.contains("tool-secret"),
                "{tool} read the secret: {read}"
            );
            // The tool really ran and the kernel refused it (not merely absent).
            assert!(
                read.contains("Permission denied") || read.contains("PermissionError"),
                "{tool} was not refused by the kernel: {read}"
            );
        }
    }
    for dir in [secrets, workspace, control_ws] {
        let _ = std::fs::remove_dir_all(dir);
    }
}

#[test]
fn launcher_refuses_malformed_invocations_without_running_anything() {
    let out = Command::new(LAUNCHER)
        .args(["sandbox-exec", "--rw"])
        .output()
        .unwrap();
    assert!(!out.status.success());
}
