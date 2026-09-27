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
    /// `None` = `--check`: enforce the ruleset, report, and exit.
    command: Option<OsString>,
    args: Vec<OsString>,
}

fn parse_launcher_args(mut argv: impl Iterator<Item = OsString>) -> Result<LauncherArgs, String> {
    let (mut read_write, mut read_only) = (Vec::new(), Vec::new());
    loop {
        let flag = argv
            .next()
            .ok_or("sandbox-exec: missing `-- <command>` or `--check`")?;
        match flag.to_str() {
            Some("--rw") => {
                read_write.push(argv.next().ok_or("sandbox-exec: --rw needs a path")?.into())
            }
            Some("--ro") => {
                read_only.push(argv.next().ok_or("sandbox-exec: --ro needs a path")?.into())
            }
            Some("--") => break,
            Some("--check") => {
                if argv.next().is_some() {
                    return Err("sandbox-exec: --check must be the last argument".into());
                }
                return Ok(LauncherArgs {
                    read_write,
                    read_only,
                    command: None,
                    args: Vec::new(),
                });
            }
            _ => return Err(format!("sandbox-exec: unexpected argument {flag:?}")),
        }
    }
    let command = argv
        .next()
        .ok_or("sandbox-exec: missing command after --")?;
    Ok(LauncherArgs {
        read_write,
        read_only,
        command: Some(command),
        args: argv.collect(),
    })
}

/// Why the boundary could not be established. Both stop an explicitly
/// isolated agent from starting; they differ in who has to act.
#[derive(Debug)]
pub(crate) enum SandboxError {
    /// The grants themselves are wrong (a path cannot be opened): fix the
    /// Buzz configuration.
    Config(String),
    /// The kernel cannot enforce the required Landlock ABI or ruleset:
    /// upgrade the worker kernel. Never a reason to fall back.
    Kernel(String),
}

impl std::fmt::Display for SandboxError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SandboxError::Config(msg) => write!(
                f,
                "sandbox-exec: configuration error: {msg} (fix the --agent-fs-* grants)"
            ),
            SandboxError::Kernel(msg) => write!(
                f,
                "sandbox-exec: kernel cannot enforce Landlock filesystem isolation: {msg} \
                 (needs Landlock ABI v3, Linux 6.2+; upgrade the worker kernel)"
            ),
        }
    }
}

impl std::error::Error for SandboxError {}

/// Entry point for `buzz-acp sandbox-exec`. With a command it only returns on
/// failure, because on success the process image is replaced by the agent.
/// With `--check` it enforces the ruleset and exits, as a startup probe.
pub(crate) fn run_launcher(argv: impl Iterator<Item = OsString>) -> anyhow::Result<()> {
    let launch = parse_launcher_args(argv).map_err(anyhow::Error::msg)?;
    imp::restrict(&launch.read_write, &launch.read_only)?;
    match launch.command {
        None => Ok(()),
        Some(command) => Err(imp::exec(&command, &launch.args)),
    }
}

/// Startup probe run by the harness: enforce `policy` in a throwaway launcher
/// so an unsupported kernel or a bad grant stops the harness before any
/// agent is spawned, with the launcher's classified error.
pub(crate) fn probe(policy: &FsPolicy) -> anyhow::Result<()> {
    let mut argv = policy.launcher_args("", &[]);
    argv.truncate(argv.len() - 2); // drop `-- <command>`
    argv.push("--check".into());
    let mut cmd = std::process::Command::new(&policy.launcher);
    cmd.args(argv);
    #[cfg(unix)]
    std::os::unix::process::CommandExt::arg0(&mut cmd, "buzz-acp");
    let out = cmd.output()?;
    if out.status.success() {
        return Ok(());
    }
    // First line only: the launcher's classified error, without a backtrace.
    let stderr = String::from_utf8_lossy(&out.stderr);
    let reason = stderr.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
    anyhow::bail!("agent filesystem isolation unavailable: {}", reason.trim())
}

#[cfg(target_os = "linux")]
mod imp {
    use std::ffi::OsString;
    use std::os::unix::process::CommandExt;
    use std::path::PathBuf;

    use super::SandboxError;
    use landlock::{
        Access, AccessFs, CompatLevel, Compatible, PathBeneath, PathFd, Ruleset, RulesetAttr,
        RulesetCreatedAttr, RulesetError, RulesetStatus, ABI,
    };

    /// Landlock ABI floor (Linux 6.2+): covers file rename/link across
    /// directories (V2) and truncation (V3). Older kernels are refused
    /// rather than sandboxed partially.
    const ABI_FLOOR: ABI = ABI::V3;

    pub(super) fn restrict(
        read_write: &[PathBuf],
        read_only: &[PathBuf],
    ) -> Result<(), SandboxError> {
        // Open every grant before touching Landlock, so a bad path is
        // reported as configuration, never as a kernel limitation.
        let rules = read_only
            .iter()
            .map(|p| (p, AccessFs::from_read(ABI_FLOOR)))
            .chain(
                read_write
                    .iter()
                    .map(|p| (p, AccessFs::from_all(ABI_FLOOR))),
            )
            .map(|(path, access)| {
                let fd = PathFd::new(path)
                    .map_err(|e| SandboxError::Config(format!("grant path: {e}")))?;
                let is_dir = path
                    .metadata()
                    .map_err(|e| {
                        SandboxError::Config(format!("grant path {}: {e}", path.display()))
                    })?
                    .is_dir();
                // Directory-only rights cannot apply to a file rule.
                let access = if is_dir {
                    access
                } else {
                    access & AccessFs::from_file(ABI_FLOOR)
                };
                Ok(PathBeneath::new(fd, access))
            })
            .collect::<Result<Vec<_>, SandboxError>>()?;

        let kernel = |e: RulesetError| SandboxError::Kernel(e.to_string());
        let status = Ruleset::default()
            .set_compatibility(CompatLevel::HardRequirement)
            .handle_access(AccessFs::from_all(ABI_FLOOR))
            .map_err(kernel)?
            .create()
            .map_err(kernel)?
            .add_rules(rules.into_iter().map(Ok::<_, RulesetError>))
            .map_err(kernel)?
            .restrict_self()
            .map_err(kernel)?;
        if status.ruleset != RulesetStatus::FullyEnforced || !status.no_new_privs {
            return Err(SandboxError::Kernel(format!(
                "ruleset {:?}, no_new_privs {}",
                status.ruleset, status.no_new_privs
            )));
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

    use super::SandboxError;

    pub(super) fn restrict(_: &[PathBuf], _: &[PathBuf]) -> Result<(), SandboxError> {
        Err(SandboxError::Kernel(
            "filesystem isolation requires Linux (Landlock)".into(),
        ))
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
        assert_eq!(parsed.command.as_deref(), Some("claude-agent-acp".as_ref()));
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
            &["--check", "extra"][..],
        ] {
            assert!(parse_launcher_args(os(argv)).is_err(), "{argv:?}");
        }
    }
}
