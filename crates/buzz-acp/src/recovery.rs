//! Durable accepted work for managed harness restarts.
//!
//! A synced snapshot is written before dispatch. Pending requests survive
//! process death; terminal IDs suppress completed relay replays. This records
//! requests, not ACP transcripts or tool results: recovered work must reconcile
//! prior side effects before repeating an action. An OS lock prevents two live
//! harnesses from sharing a journal; process death releases that lock.

use anyhow::{bail, Context, Result};
use fs2::FileExt;
use nostr::{Event, EventId, PublicKey};
use serde::{Deserialize, Serialize};
use std::collections::{HashSet, VecDeque};
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use uuid::Uuid;

const VERSION: u32 = 1;
const MAX_PENDING: usize = 10_000;
const MAX_COMPLETED: usize = 4096;
const MAX_SNAPSHOT_BYTES: u64 = 64 * 1024 * 1024;

/// A signed request awaiting a terminal turn outcome.
#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct RecoveryEvent {
    pub(crate) channel_id: Uuid,
    pub(crate) event: Event,
    pub(crate) prompt_tag: String,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Snapshot {
    version: u32,
    relay_url: String,
    pubkey: String,
    pending: Vec<RecoveryEvent>,
    completed: VecDeque<String>,
}

/// Holds one journal's exclusive writer lock for its entire lifetime.
pub(crate) struct RecoveryJournal {
    path: PathBuf,
    _lock: WriterLock,
    snapshot: Snapshot,
}

// Closing the owning descriptor is insufficient during a concurrent fork:
// the transient inherited descriptor can keep flock alive until exec. Unlock
// explicitly on every exit, including journal validation failures after lock.
struct WriterLock(File);
impl Drop for WriterLock {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.0);
    }
}
impl RecoveryJournal {
    /// Open a journal scoped to one relay and agent identity, failing closed on
    /// corrupt state or an existing live writer. Orphaned temporary snapshots
    /// are ignored: only the atomically replaced destination is authoritative.
    pub(crate) fn open(path: &Path, relay_url: &str, pubkey: &str) -> Result<Self> {
        let relay_url = canonical_relay(relay_url)?;
        let pubkey = PublicKey::from_hex(pubkey)
            .context("invalid recovery identity")?
            .to_hex();
        let parent = journal_parent(path);
        fs::create_dir_all(parent).context("create recovery journal directory")?;
        let name = path
            .file_name()
            .context("recovery journal requires a filename")?;
        let mut lock_name = name.to_os_string();
        lock_name.push(".lock");
        let lock_path = parent.join(lock_name);
        let lock = WriterLock(
            private_options()
                .open(&lock_path)
                .context("open recovery lock")?,
        );
        lock.0
            .try_lock_exclusive()
            .context("recovery journal already has a live writer")?;

        if fs::metadata(path).is_ok_and(|metadata| metadata.len() > MAX_SNAPSHOT_BYTES) {
            bail!("recovery snapshot exceeds size bound");
        }
        let snapshot = match fs::read(path) {
            Ok(bytes) => {
                if bytes.len() as u64 > MAX_SNAPSHOT_BYTES {
                    bail!("recovery snapshot exceeds size bound");
                }
                let stored: Snapshot = serde_json::from_slice(&bytes)
                    .context("invalid recovery snapshot; existing state preserved")?;
                validate_snapshot(&stored, &relay_url, &pubkey)?;
                stored
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Snapshot {
                version: VERSION,
                relay_url,
                pubkey,
                pending: Vec::new(),
                completed: VecDeque::new(),
            },
            Err(error) => return Err(error).context("read recovery snapshot"),
        };
        let journal = Self {
            path: path.to_path_buf(),
            _lock: lock,
            snapshot,
        };
        if !path.exists() {
            journal.persist(&journal.snapshot)?;
        }
        Ok(journal)
    }

    /// Sync a newly accepted request before it can be dispatched. Existing
    /// pending IDs are idempotent; terminal IDs return false and stay retired.
    pub(crate) fn record(
        &mut self,
        channel_id: Uuid,
        event: &Event,
        prompt_tag: &str,
    ) -> Result<bool> {
        validate_event(channel_id, event)?;
        let id = event.id.to_hex();
        if self.is_completed(&id) {
            return Ok(false);
        }
        if self.is_pending(&id) {
            return Ok(true);
        }
        if self.snapshot.pending.len() >= MAX_PENDING {
            bail!("recovery pending bound reached; request must not be dispatched");
        }
        let mut next = self.snapshot.clone();
        next.pending.push(RecoveryEvent {
            channel_id,
            event: event.clone(),
            prompt_tag: prompt_tag.to_owned(),
        });
        self.persist(&next)?;
        self.snapshot = next;
        Ok(true)
    }

    /// Return unresolved requests in their durable acceptance order.
    pub(crate) fn pending(&self) -> Vec<RecoveryEvent> {
        self.snapshot.pending.clone()
    }

    pub(crate) fn is_pending(&self, id: &str) -> bool {
        self.snapshot
            .pending
            .iter()
            .any(|entry| entry.event.id.to_hex() == id)
    }

    pub(crate) fn is_completed(&self, id: &str) -> bool {
        self.snapshot
            .completed
            .iter()
            .any(|completed| completed == id)
    }

    /// Retire completed or deliberately discarded requests atomically. A write
    /// failure leaves the in-memory snapshot unchanged so callers fail closed.
    pub(crate) fn settle(&mut self, ids: impl IntoIterator<Item = String>) -> Result<()> {
        let mut next = self.snapshot.clone();
        let ids: HashSet<String> = ids
            .into_iter()
            .map(|id| {
                EventId::from_hex(&id)
                    .map(|parsed| parsed.to_hex())
                    .context("invalid retired event ID")
            })
            .collect::<Result<_>>()?;
        if ids.is_empty() {
            return Ok(());
        }
        next.pending
            .retain(|entry| !ids.contains(&entry.event.id.to_hex()));
        // Sort terminal additions so snapshots do not depend on HashSet order.
        let mut sorted: Vec<_> = ids.into_iter().collect();
        sorted.sort();
        for id in sorted {
            if !next.completed.contains(&id) {
                next.completed.push_back(id);
            }
        }
        while next.completed.len() > MAX_COMPLETED {
            next.completed.pop_front();
        }
        self.persist(&next)?;
        self.snapshot = next;
        Ok(())
    }

    /// Retire all requests in a channel, including withheld or cancelled work
    /// that the in-memory queue's plain-event drain may not enumerate.
    pub(crate) fn settle_channel(&mut self, channel_id: Uuid) -> Result<()> {
        let ids: Vec<_> = self
            .snapshot
            .pending
            .iter()
            .filter(|entry| entry.channel_id == channel_id)
            .map(|entry| entry.event.id.to_hex())
            .collect();
        self.settle(ids)
    }

    fn persist(&self, next: &Snapshot) -> Result<()> {
        let bytes = serde_json::to_vec(next).context("serialize recovery snapshot")?;
        if bytes.len() as u64 > MAX_SNAPSHOT_BYTES {
            bail!("recovery snapshot exceeds size bound");
        }
        let parent = journal_parent(&self.path);
        let mut name = self
            .path
            .file_name()
            .context("recovery journal requires a filename")?
            .to_os_string();
        name.push(format!(".{}.tmp", Uuid::new_v4()));
        let temporary = parent.join(name);
        let result = (|| -> Result<()> {
            let mut options = private_options();
            options.create_new(true);
            let mut file = options
                .open(&temporary)
                .context("create recovery snapshot temporary file")?;
            file.write_all(&bytes).context("write recovery snapshot")?;
            file.sync_all().context("sync recovery snapshot")?;
            drop(file);
            fs::rename(&temporary, &self.path).context("replace recovery snapshot atomically")?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result
    }
}

fn journal_parent(path: &Path) -> &Path {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
}

fn private_options() -> OpenOptions {
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options
}

fn canonical_relay(relay_url: &str) -> Result<String> {
    buzz_core::relay::normalize_relay_url(relay_url).context("invalid recovery relay identity")
}
fn validate_event(channel_id: Uuid, event: &Event) -> Result<()> {
    event
        .verify()
        .context("recovery event signature or ID is invalid")?;
    // Match relay::extract_h_tag_uuid: the first parseable h tag selects
    // the channel. A later matching tag must not override that binding.
    let bound_channel = event.tags.iter().find_map(|tag| {
        let values = tag.as_slice();
        if values.first().is_some_and(|value| value == "h") {
            values.get(1).and_then(|value| value.parse::<Uuid>().ok())
        } else {
            None
        }
    });
    if bound_channel != Some(channel_id) {
        bail!("recovery event does not bind its journal channel");
    }
    Ok(())
}

fn validate_snapshot(snapshot: &Snapshot, relay_url: &str, pubkey: &str) -> Result<()> {
    if snapshot.version != VERSION || snapshot.relay_url != relay_url || snapshot.pubkey != pubkey {
        bail!("recovery journal version, relay, or identity mismatch; existing state preserved");
    }
    if snapshot.pending.len() > MAX_PENDING || snapshot.completed.len() > MAX_COMPLETED {
        bail!("recovery snapshot exceeds entry bounds");
    }
    let mut seen = HashSet::new();
    for id in &snapshot.completed {
        if EventId::from_hex(id)?.to_hex() != *id || !seen.insert(id.clone()) {
            bail!("invalid or duplicate recovery terminal ID");
        }
    }
    for entry in &snapshot.pending {
        validate_event(entry.channel_id, &entry.event)?;
        if !seen.insert(entry.event.id.to_hex()) {
            bail!("duplicate or already terminal recovery request");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use nostr::{EventBuilder, Keys, Kind, Tag};

    struct TestDirectory(PathBuf);
    impl TestDirectory {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!("buzz-recovery-{}", Uuid::new_v4()));
            fs::create_dir_all(&path).expect("create owned test directory");
            Self(path)
        }
        fn journal(&self) -> PathBuf {
            self.0.join("recovery.json")
        }
    }
    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    fn signed(channel_id: Uuid, content: &str) -> Event {
        EventBuilder::new(Kind::Custom(9), content)
            .tags([Tag::parse(["h".to_owned(), channel_id.to_string()]).expect("channel tag")])
            .sign_with_keys(&Keys::generate())
            .expect("signed test event")
    }
    fn identity() -> String {
        Keys::generate().public_key().to_hex()
    }
    fn open(path: &Path, pubkey: &str) -> RecoveryJournal {
        RecoveryJournal::open(path, "wss://relay.example", pubkey).expect("open journal")
    }

    #[test]
    fn process_restart_preserves_pending_and_suppresses_terminal_replay() {
        let dir = TestDirectory::new();
        let pubkey = identity();
        let channel_id = Uuid::new_v4();
        let pending = signed(channel_id, "interrupted work");
        let completed = signed(channel_id, "completed work");
        {
            let mut journal = open(&dir.journal(), &pubkey);
            assert!(journal
                .record(channel_id, &pending, "@mention")
                .expect("record pending"));
            assert!(journal
                .record(channel_id, &completed, "@mention")
                .expect("record completed"));
            journal
                .settle([completed.id.to_hex()])
                .expect("settle completed");
        }
        let mut restarted = open(&dir.journal(), &pubkey);
        assert_eq!(restarted.pending().len(), 1);
        assert_eq!(restarted.pending()[0].event.id, pending.id);
        assert!(!restarted
            .record(channel_id, &completed, "@mention")
            .expect("terminal replay"));
        assert!(restarted
            .record(channel_id, &pending, "@mention")
            .expect("pending replay"));
        assert_eq!(restarted.pending().len(), 1);
        restarted
            .settle([pending.id.to_hex()])
            .expect("finish resumed work");
        drop(restarted);
        assert!(open(&dir.journal(), &pubkey).pending().is_empty());
    }

    #[test]
    fn live_second_writer_is_refused_and_drop_releases_lock() {
        let dir = TestDirectory::new();
        let pubkey = identity();
        let first = open(&dir.journal(), &pubkey);
        assert!(RecoveryJournal::open(&dir.journal(), "wss://relay.example", &pubkey).is_err());
        drop(first);
        assert!(RecoveryJournal::open(&dir.journal(), "wss://relay.example", &pubkey).is_ok());
    }

    #[test]
    fn wrong_relay_or_identity_fails_closed() {
        let dir = TestDirectory::new();
        let pubkey = identity();
        drop(open(&dir.journal(), &pubkey));
        let bytes = fs::read(dir.journal()).expect("snapshot bytes");
        assert!(RecoveryJournal::open(&dir.journal(), "wss://other.example", &pubkey).is_err());
        assert!(RecoveryJournal::open(&dir.journal(), "wss://relay.example", &identity()).is_err());
        assert_eq!(fs::read(dir.journal()).expect("unchanged bytes"), bytes);
        assert!(RecoveryJournal::open(&dir.journal(), "WSS://RELAY.example/", &pubkey).is_ok());
    }

    #[test]
    fn journal_reopens_for_the_same_canonical_loopback_runtime() {
        let dir = TestDirectory::new();
        let pubkey = identity();
        drop(
            RecoveryJournal::open(&dir.journal(), "ws://localhost:80/", &pubkey)
                .expect("localhost journal"),
        );
        drop(RecoveryJournal::open(&dir.journal(), "ws://127.0.0.1", &pubkey).expect("IPv4 alias"));
        assert!(RecoveryJournal::open(&dir.journal(), "ws://[::1]/", &pubkey).is_ok());
        assert!(RecoveryJournal::open(&dir.journal(), "ws://localhost/#foreign", &pubkey).is_err());
    }
    #[test]
    fn corrupt_snapshot_is_preserved_and_orphaned_temp_is_ignored() {
        let dir = TestDirectory::new();
        let pubkey = identity();
        drop(open(&dir.journal(), &pubkey));
        fs::write(dir.0.join("recovery.json.stale.tmp"), b"half-written").expect("orphan temp");
        assert!(open(&dir.journal(), &pubkey).pending().is_empty());
        fs::write(dir.journal(), b"{\"version\":1").expect("truncated snapshot");
        assert!(RecoveryJournal::open(&dir.journal(), "wss://relay.example", &pubkey).is_err());
        assert_eq!(
            fs::read(dir.journal()).expect("preserved corrupt bytes"),
            b"{\"version\":1"
        );
    }

    #[test]
    fn tampered_event_and_wrong_channel_are_refused() {
        let dir = TestDirectory::new();
        let mut journal = open(&dir.journal(), &identity());
        let channel_id = Uuid::new_v4();
        let mut event = signed(channel_id, "work");
        assert!(journal.record(Uuid::new_v4(), &event, "@mention").is_err());
        let other_channel = Uuid::new_v4();
        let ambiguous = EventBuilder::new(Kind::Custom(9), "two channels")
            .tags([
                Tag::parse(["h".to_owned(), channel_id.to_string()]).expect("first channel"),
                Tag::parse(["h".to_owned(), other_channel.to_string()]).expect("later channel"),
            ])
            .sign_with_keys(&Keys::generate())
            .expect("signed ambiguous event");
        assert!(journal
            .record(other_channel, &ambiguous, "@mention")
            .is_err());
        event.content = "tampered".into();
        assert!(journal.record(channel_id, &event, "@mention").is_err());
        assert!(journal.pending().is_empty());
    }

    #[test]
    fn failed_write_does_not_change_memory_or_prior_disk_snapshot() {
        let dir = TestDirectory::new();
        let mut journal = open(&dir.journal(), &identity());
        let channel_id = Uuid::new_v4();
        let pending = signed(channel_id, "already accepted");
        journal
            .record(channel_id, &pending, "@mention")
            .expect("accept prior work");
        let bytes = fs::read(dir.journal()).expect("prior snapshot");
        let real_path = journal.path.clone();
        let blocking_path = dir.0.join("cannot-replace-directory");
        fs::create_dir(&blocking_path).expect("create blocker");
        journal.path = blocking_path;
        assert!(journal
            .record(channel_id, &signed(channel_id, "new work"), "@mention")
            .is_err());
        assert_eq!(journal.pending().len(), 1);
        assert!(journal.settle([pending.id.to_hex()]).is_err());
        assert_eq!(journal.pending().len(), 1);
        assert!(!journal.is_completed(&pending.id.to_hex()));
        assert_eq!(fs::read(real_path).expect("prior snapshot survived"), bytes);
    }

    #[test]
    fn channel_retirement_includes_all_accepted_events_and_preserves_other_channel() {
        let dir = TestDirectory::new();
        let mut journal = open(&dir.journal(), &identity());
        let first_channel = Uuid::new_v4();
        let second_channel = Uuid::new_v4();
        for content in ["in flight", "withheld steer", "cancelled prior"] {
            journal
                .record(first_channel, &signed(first_channel, content), "@mention")
                .expect("record");
        }
        journal
            .record(
                second_channel,
                &signed(second_channel, "other work"),
                "@mention",
            )
            .expect("record other");
        journal
            .settle_channel(first_channel)
            .expect("retire channel");
        assert_eq!(journal.pending().len(), 1);
        assert_eq!(journal.pending()[0].channel_id, second_channel);
    }

    #[test]
    fn terminal_history_is_bounded() {
        let dir = TestDirectory::new();
        let mut journal = open(&dir.journal(), &identity());
        let ids: Vec<_> = (0..MAX_COMPLETED + 4)
            .map(|index| format!("{index:064x}"))
            .collect();
        journal.settle(ids.clone()).expect("settle IDs");
        assert_eq!(journal.snapshot.completed.len(), MAX_COMPLETED);
        assert!(!journal.is_completed(&ids[0]));
        assert!(journal.is_completed(ids.last().expect("last ID")));
    }

    struct ChildGuard(std::process::Child);
    impl Drop for ChildGuard {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    #[test]
    #[ignore = "child fixture launched by killed_writer_releases_os_lock"]
    fn child_process_holds_writer_lock() {
        let Ok(path) = std::env::var("BUZZ_RECOVERY_LOCK_TEST_PATH") else {
            return;
        };
        let pubkey =
            std::env::var("BUZZ_RECOVERY_LOCK_TEST_PUBKEY").expect("fixture public identity");
        let _journal = open(Path::new(&path), &pubkey);
        fs::write(Path::new(&path).with_extension("ready"), b"locked")
            .expect("publish fixture readiness");
        loop {
            std::thread::park();
        }
    }

    #[test]
    fn killed_writer_releases_os_lock_without_graceful_cleanup() {
        let dir = TestDirectory::new();
        let path = dir.journal();
        let pubkey = identity();
        let child = std::process::Command::new(std::env::current_exe().expect("test binary"))
            .args([
                "--exact",
                "recovery::tests::child_process_holds_writer_lock",
                "--ignored",
            ])
            .env("BUZZ_RECOVERY_LOCK_TEST_PATH", &path)
            .env("BUZZ_RECOVERY_LOCK_TEST_PUBKEY", &pubkey)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn owned test child");
        let mut child = ChildGuard(child);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !path.with_extension("ready").exists() {
            assert!(
                child.0.try_wait().expect("poll owned child").is_none(),
                "fixture exited early"
            );
            assert!(
                std::time::Instant::now() < deadline,
                "fixture never acquired lock"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(RecoveryJournal::open(&path, "wss://relay.example", &pubkey).is_err());
        child.0.kill().expect("kill owned fixture without cleanup");
        child.0.wait().expect("reap owned fixture");
        assert!(RecoveryJournal::open(&path, "wss://relay.example", &pubkey).is_ok());
    }
    #[cfg(unix)]
    #[test]
    fn snapshots_are_private_to_the_owner() {
        use std::os::unix::fs::PermissionsExt;
        let dir = TestDirectory::new();
        drop(open(&dir.journal(), &identity()));
        assert_eq!(
            fs::metadata(dir.journal())
                .expect("metadata")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
}
