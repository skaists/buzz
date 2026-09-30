# Accepted-request recovery after a Desktop crash — 2026-09-30

Status: tested draft candidate; not deployed. Full CI is blocked at native
Desktop compilation by missing WSL system libraries.

## Incident and pinned source

At 16:53:35Z, a read-only SSH check of root@2.25.245.161 measured
buzz-hostinger uptime 5 days 18:51. The production relay was healthy, started
2026-09-26T03:44:02Z; PostgreSQL/Redis started 2026-09-24T22:02:16Z.
All three restart counts were zero. Windows process parentage showed current
buzz-acp children owned by the Desktop process restarted at 10:40:38 local.
Server uptime alone does not establish continuation of any interrupted turn.

Fetched origin/main: a8e844d4d6bcc4224d5719e236ab4b9a83b64c39.
Own branch: codex/agent-crash-recovery.
Own worktree: C:/Users/travi/.buzz/REPOS/wt-codex-crash-recovery.
No other seat's uncommitted work was staged, changed or claimed repaired.

The source gap is concrete: Desktop lazily restores harness processes;
EventQueue and in-flight batches are memory-only; pool sessions are memory-only;
a restarted harness uses a fresh subscription watermark with a short clock-skew
window. Older accepted requests are not recovered by restart alone.

## Candidate behavior

- Desktop derives one stable journal per canonical relay/agent runtime pair,
  applies its path after inherited/user environments, and reserves the override.
- ACP validates and locks the journal before spawning any eager adapter.
  Exclusive OS locking refuses a concurrent writer and releases after process
  death. Acceptance syncs a same-directory temporary snapshot before atomic
  replacement; a failed write prevents dispatch.
- Original signed requests remain durable through queueing, active turns,
  native steers, retries and graceful shutdown. A fresh runtime reapplies
  current membership, author policy and subscription rules. Ineligible or
  unresolvable records remain preserved rather than being silently deleted.
- Retrying a failed turn delivers accepted native steering payloads to the
  replacement session. Old acknowledgments cannot consume retry work or prove
  completion. Retry paths preserve annotated cancellation context.
- A turn whose deadline expires without a terminal result stops journal-enabled
  dispatch and retains all accepted work for a fresh runtime. It cannot allow
  an old result to settle an unrelated newer turn.
- Known terminal results and deliberate existing queue-policy discards retire
  records. Recent completed IDs suppress duplicate relay replay.

Signature/channel binding: crates/buzz-acp/src/recovery.rs::validate_event
calls nostr::Event::verify and matches the first parseable h tag, as the relay
does. Scope validation is RecoveryJournal::open/validate_snapshot. These are
source and regression-test claims; no broader cryptographic security claim is
made. The snapshot contains signed inbound requests and public event/agent IDs,
and does not separately store the configured private key or adapter transcript.
Original inbound request content is persisted and can contain sensitive text,
so the journal directory must be protected as local workspace data.

Recovery is at least once for unfinished requests. A tool action may succeed
before its terminal record is written, so recovered prompts require inspection
of current workspace and relay receipts before repeating an action. Process
restart does not preserve ACP transcript/cancellation framing; original signed
requests and reconciliation guidance are the recovery boundary.

Existing Drop mode, explicit cancellation, retry exhaustion and 500-per-channel
queue-overflow policy still apply. Journals cap pending records at 10,000,
completed history at 4096 and snapshot bytes at 64 MiB. This does not capture
messages arriving while the harness was offline, keep agents working with
Desktop closed, or restore accepted work predating this journal.

## Measured validation

Hermit activated for Linux package/CI commands. Owned Linux build target:
/home/travi/codex-crash-recovery-target; no shared target or live seat restarted.

1. Final package command: cargo test --offline -p buzz-acp.
   Unit result: 811 passed, 0 failed, 1 ignored, 42.40s; integration result:
   9 passed, 0 failed; binary/doc targets ran 0 tests; overall exit 0.
   The ignored test is a child fixture explicitly invoked by the passing
   forced-kill parent test, not omitted crash coverage.
   Log: /home/travi/codex-crash-recovery-final-package.log.
2. Final native Windows command: cargo test --offline --manifest-path
   C:/Users/travi/AppData/Local/Temp/buzz-native-recovery-check/Cargo.toml
   --target-dir C:/Users/travi/AppData/Local/Temp/buzz-native-recovery-target
   recovery -- --nocapture.
   Result: 25 passed, 0 failed, 1 child fixture ignored, 131 filtered; exit 0.
   Harness uses actual final journal, queue and relay canonicalizer; only
   DedupMode/default duration are shimmed. Forced process kill, lock release,
   repeated Windows atomic replacement, retry payload, late ACK and expiry
   regressions passed. This is not a full Desktop build or live relay smoke.
   Log: C:/Users/travi/AppData/Local/Temp/buzz-native-recovery-check/native-recovery-final-20260930.log.
3. Actual Desktop environment-validation source in a standalone native harness:
   43 passed, 0 failed, exit 0; unused AgentDefinition type is shimmed.
   Log: C:/Users/travi/AppData/Local/Temp/buzz-desktop-recovery-env-check/rerun-final-20260930.log.
4. Actual Desktop runtime key, buzz-core canonicalizer and path/env source in
   an isolated Cargo harness: 6 passed, 0 failed, exit 0. Only an unrelated
   ManagedAgentProcess import is removed; key/canonicalizer/path are real.
   Log: C:/Users/travi/AppData/Local/Temp/buzz-desktop-recovery-path-check/rerun-final-20260930.log.
5. Negative control omitted only RecoveryJournal::record's persist call,
   retaining true acceptance and its in-memory snapshot. Exact queue crash test
   reached reopen and failed on empty recovered IDs: 0 passed, 1 failed,
   811 filtered, exit 101. Original source bytes restored in finally; the final
   green package run above follows restoration. This proves the test detects
   loss across restart, rather than only testing an initial memory assertion.
   Log: /home/travi/codex-crash-recovery-negative-control.log.
6. Initial journal run was 8 passed, 2 failed, 1 ignored, exit 101. Concurrent
   child process creation exposed fork-inherited lock handles. Explicit RAII
   unlock now covers successful and failed-open paths; final suites passed.
7. cargo fmt --all and cargo fmt --manifest-path desktop/src-tauri/Cargo.toml
   --all completed with exit 0. Final full-CI results follow below.
8. First completed just ci: exit 1. Root Rust formatting and workspace Clippy
   passed; Desktop biome completed with two existing non-blocking warnings.
   Desktop file-size checking then failed because Linux Git cannot resolve the
   Windows worktree gitfile. Log: /home/travi/codex-crash-recovery-final-ci.log.
9. Final just ci used a temporary PATH bridge to the actual Windows Git binary,
   with cwd converted through wslpath. No gate/script or repository hook changed.
   Root formatting, workspace Clippy --all-targets -D warnings, Desktop checks
   (including size/pixel-text/pubkey gates), and Tauri formatting all passed.
   Native Tauri Clippy then failed: soup3-sys build script exit 1, cargo/recipe
   exit 101, outer just ci exit 1. Exact blocker: pkg-config could not find
   libsoup-3.0.pc (required libsoup-3.0 >= 3.0). Read-only prerequisite probes
   also found WebKitGTK 4.1 and JavaScriptCoreGTK 4.1 absent; GTK 3 was present.
   No system-library install or live runtime restart was attempted.
   Log: /home/travi/codex-crash-recovery-ci-windows-git.log.
   The runtime integration was kept below the existing Desktop size gate by
   placing the recovery implementation/tests in its own module.
10. Scoped diff whitespace check passed. The pinned origin/main already has a
    successful GitHub CI run: https://github.com/skaists/buzz/actions/runs/36526191246.
    No inherited source repair was added to this lane. Only fs2 was added;
    Cargo normalized existing dependency edges without other version updates.

## Boundaries and outstanding checks

No installed Desktop/ACP binary was replaced; no production relay, store,
workflow/B8/WF-08 gate, download lane or other agent runtime was changed.
Full Tauri compile and real relay/Desktop restart smoke remain required before
release. Native standalone source harnesses do not replace these gates.

Prescribed just hooks failed: WSL Git could not resolve this Windows-created
worktree gitfile (fatal: not a git repository; recipe exit 128, command exit 1).
The fork's shared hooks directory contains sample hooks only; no installed
hook was bypassed or rewritten. Commits use Windows Git in this owned worktree
and an explicit DCO signoff. Initial full-CI attempt was interrupted while
source corrections were still in progress (exit 1). Final completed attempts
and the resolved Windows-Git bridge are recorded above. Checks after the
native Clippy failure (full workspace unit sweep, Tauri tests, frontend builds,
web/mobile gates) were not reached by just ci. Independent ACP/native-source
receipts above remain passes; the full app/release gate remains open.
