//! Filesystem capability boundary for agent subprocesses (`--agent-fs-isolation`).
//!
//! Environment isolation (`AgentEnvPolicy`) stops credentials from being
//! *inherited*; it does not stop an agent from reading `~/.ssh` or
//! `~/.cargo/credentials.toml` directly. This module closes that gap on Linux
//! with Landlock: the agent, and every process it starts, can only touch the
//! paths granted here. Everything else is denied by the kernel.
//!
//! Mechanism: instead of applying Landlock between fork and exec (which needs
//! `pre_exec`, i.e. `unsafe`), the harness spawns itself as a small launcher,
//! `buzz-acp sandbox-exec --rw <path>… --ro <path>… -- <agent> <args>…`. The
//! launcher restricts its own (single) thread and then `exec`s the agent;
//! Landlock restrictions survive `exec` and are inherited by all descendants.
//!
//! Fail closed: if the running kernel cannot enforce the full ruleset, the
//! launcher exits with an error and the agent never starts. This limits
//! filesystem access only; it is not a network or process sandbox, and paths
//! stay *visible* (Landlock denies access, it does not hide mounts).

use std::ffi::OsString;
use std::path::PathBuf;

/// Argv\[1\] that selects the launcher inside the `buzz-acp` binary.
pub(crate) const SUBCOMMAND: &str = "sandbox-exec";

/// System locations an agent needs read + execute access to in order to run
/// at all (toolchains, shared libraries, config). Missing ones are skipped;
/// operator-supplied paths must exist.
///
/// `/proc` is deliberately *not* granted as a whole: `/proc/<pid>/environ` of
/// the harness (or any same-user process) would hand the agent the very
/// variables env isolation withholds, and Landlock's ptrace check does not
/// stop that read (measured on Linux 6.18). Only system-wide `/proc` files
/// are granted, plus `/proc/self`, which the launcher resolves to its own pid
/// before exec (so the agent can read its own entry; its children cannot).
/// `/dev` is likewise narrowed to plain devices, keeping `/dev/shm` out.
pub const DEFAULT_READ_ONLY: &[&str] = &[
    "/usr",
    "/bin",
    "/sbin",
    "/lib",
    "/lib32",
    "/lib64",
    "/etc",
    "/opt",
    "/nix",
    "/sys",
    "/proc/self",
    "/proc/cpuinfo",
    "/proc/meminfo",
    "/proc/stat",
    "/proc/loadavg",
    "/proc/uptime",
    "/proc/version",
    "/proc/filesystems",
    "/proc/sys",
    "/dev/zero",
    "/dev/random",
    "/dev/urandom",
];

/// Device files that ordinary tools write to (`> /dev/null`).
pub const DEFAULT_READ_WRITE: &[&str] = &["/dev/null", "/dev/full"];

/// Paths an isolated agent may read, write, or execute.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FsPolicy {
    /// Full access (the workspace, a private temp dir, the agent's Cargo home).
    pub read_write: Vec<PathBuf>,
    /// Read and execute only (system paths, agent install directories).
    pub read_only: Vec<PathBuf>,
    /// The `buzz-acp` binary that runs the launcher (resolved at startup).
    pub launcher: PathBuf,
}

impl FsPolicy {
    /// Argv for the launcher: `sandbox-exec --rw … --ro … -- command args…`.
    pub(crate) fn launcher_args(&self, command: &str, args: &[String]) -> Vec<OsString> {
        let mut argv: Vec<OsString> = vec![SUBCOMMAND.into()];
        for path in &self.read_write {
            argv.push("--rw".into());
            argv.push(path.clone().into_os_string());
        }
        for path in &self.read_only {
            argv.push("--ro".into());
            argv.push(path.clone().into_os_string());
        }
        argv.push("--".into());
        argv.push(command.into());
        argv.extend(args.iter().map(OsString::from));
        argv
    }
}

/// Launcher arguments, parsed from everything after the subcommand.
#[derive(Debug, PartialEq, Eq)]
struct LauncherArgs {
    read_write: Vec<PathBuf>,
    read_only: Vec<PathBuf>,
    command: OsString,
    args: Vec<OsString>,
}

fn parse_launcher_args(mut argv: impl Iterator<Item = OsString>) -> Result<LauncherArgs, String> {
    let (mut read_write, mut read_only) = (Vec::new(), Vec::new());
    loop {
        let flag = argv.next().ok_or("sandbox-exec: missing `-- <command>`")?;
        match flag.to_str() {
            Some("--rw") => {
                read_write.push(argv.next().ok_or("sandbox-exec: --rw needs a path")?.into())
            }
            Some("--ro") => {
                read_only.push(argv.next().ok_or("sandbox-exec: --ro needs a path")?.into())
            }
            Some("--") => break,
            _ => return Err(format!("sandbox-exec: unexpected argument {flag:?}")),
        }
    }
    let command = argv
        .next()
        .ok_or("sandbox-exec: missing command after --")?;
    Ok(LauncherArgs {
        read_write,
        read_only,
        command,
        args: argv.collect(),
    })
}

/// Entry point for `buzz-acp sandbox-exec`. Only returns on failure; on
/// success the process image is replaced by the agent.
pub(crate) fn run_launcher(argv: impl Iterator<Item = OsString>) -> anyhow::Result<()> {
    let launch = parse_launcher_args(argv).map_err(anyhow::Error::msg)?;
    imp::restrict(&launch.read_write, &launch.read_only)?;
    Err(imp::exec(&launch.command, &launch.args))
}

#[cfg(target_os = "linux")]
mod imp {
    use std::ffi::OsString;
    use std::os::unix::process::CommandExt;
    use std::path::PathBuf;

    use landlock::{
        path_beneath_rules, Access, AccessFs, CompatLevel, Compatible, Ruleset, RulesetAttr,
        RulesetCreatedAttr, RulesetStatus, ABI,
    };

    /// Landlock ABI floor (Linux 6.2+): covers file rename/link across
    /// directories (V2) and truncation (V3). Older kernels are refused
    /// rather than sandboxed partially.
    const ABI_FLOOR: ABI = ABI::V3;

    pub(super) fn restrict(read_write: &[PathBuf], read_only: &[PathBuf]) -> anyhow::Result<()> {
        let status = Ruleset::default()
            .set_compatibility(CompatLevel::HardRequirement)
            .handle_access(AccessFs::from_all(ABI_FLOOR))?
            .create()?
            .add_rules(path_beneath_rules(
                read_only,
                AccessFs::from_read(ABI_FLOOR),
            ))?
            .add_rules(path_beneath_rules(
                read_write,
                AccessFs::from_all(ABI_FLOOR),
            ))?
            .restrict_self()?;
        if status.ruleset != RulesetStatus::FullyEnforced {
            anyhow::bail!(
                "sandbox-exec: Landlock not fully enforced ({:?}); refusing to start the agent",
                status.ruleset
            );
        }
        Ok(())
    }

    pub(super) fn exec(command: &OsString, args: &[OsString]) -> anyhow::Error {
        let err = std::process::Command::new(command).args(args).exec();
        anyhow::anyhow!(
            "sandbox-exec: cannot exec {command:?}: {err} (if permission was denied, \
             add its install directory to --agent-fs-ro)"
        )
    }
}

#[cfg(not(target_os = "linux"))]
mod imp {
    use std::ffi::OsString;
    use std::path::PathBuf;

    pub(super) fn restrict(_: &[PathBuf], _: &[PathBuf]) -> anyhow::Result<()> {
        anyhow::bail!("sandbox-exec: filesystem isolation requires Linux (Landlock)")
    }

    pub(super) fn exec(_: &OsString, _: &[OsString]) -> anyhow::Error {
        anyhow::anyhow!("sandbox-exec: unsupported platform")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn os(args: &[&str]) -> impl Iterator<Item = OsString> {
        args.iter()
            .map(OsString::from)
            .collect::<Vec<_>>()
            .into_iter()
    }

    #[test]
    fn launcher_args_round_trip_through_the_parser() {
        // The harness builds this argv and the launcher parses it; a mismatch
        // would silently drop a grant or, worse, run the wrong command.
        let policy = FsPolicy {
            read_write: vec!["/work".into()],
            read_only: vec!["/usr".into(), "/opt/agent".into()],
            launcher: "/bin/buzz-acp".into(),
        };
        let argv = policy.launcher_args("claude-agent-acp", &["--flag".into(), "--".into()]);
        assert_eq!(argv[0], SUBCOMMAND);
        let parsed = parse_launcher_args(argv.into_iter().skip(1)).unwrap();
        assert_eq!(parsed.read_write, vec![PathBuf::from("/work")]);
        assert_eq!(
            parsed.read_only,
            vec![PathBuf::from("/usr"), PathBuf::from("/opt/agent")]
        );
        assert_eq!(parsed.command, "claude-agent-acp");
        // A literal `--` in the agent's own args belongs to the agent.
        assert_eq!(parsed.args, vec![OsString::from("--flag"), "--".into()]);
    }

    #[test]
    fn launcher_rejects_malformed_argv() {
        for argv in [
            &["--rw"][..],
            &["--rw", "/work"][..],
            &["--"][..],
            &["--bogus", "x", "--", "sh"][..],
        ] {
            assert!(parse_launcher_args(os(argv)).is_err(), "{argv:?}");
        }
    }
}
