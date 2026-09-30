use std::path::{Path, PathBuf};
use std::process::Command;

use crate::managed_agents::ManagedAgentRuntimeKey;

const RECOVERY_PATH_ENV: &str = "BUZZ_ACP_RECOVERY_PATH";

/// Journal filename is stable across process generations and opaque about the
/// relay URL. The runtime key already validates the pubkey and canonicalizes
/// the relay, preventing collisions between communities or path traversal.
fn recovery_path(agents_dir: &Path, key: &ManagedAgentRuntimeKey) -> PathBuf {
    agents_dir
        .join("recovery")
        .join(format!("{}.json", key.runtime_id()))
}

/// Replace any ambient journal path with the Desktop-owned runtime-pair path.
/// The harness creates the recovery directory when opening its journal.
pub(super) fn apply_recovery_env(
    command: &mut Command,
    agents_dir: &Path,
    key: &ManagedAgentRuntimeKey,
) {
    command.env(RECOVERY_PATH_ENV, recovery_path(agents_dir, key));
}

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;

    use super::{
        apply_recovery_env, recovery_path, Command, ManagedAgentRuntimeKey, Path, RECOVERY_PATH_ENV,
    };

    #[test]
    fn recovery_path_is_stable_for_the_canonical_runtime_pair() {
        let first =
            ManagedAgentRuntimeKey::new("a".repeat(64), "wss://ONE.example/").expect("valid pair");
        let restarted = ManagedAgentRuntimeKey::new("A".repeat(64), "wss://one.example")
            .expect("valid canonical pair");
        let root = Path::new("agents");
        let path = recovery_path(root, &first);
        assert_eq!(path, recovery_path(root, &restarted));
        assert_eq!(path.parent(), Some(root.join("recovery").as_path()));
        assert_eq!(
            path.file_name(),
            Some(OsStr::new(&format!("{}.json", first.runtime_id())))
        );
        assert!(!path.to_string_lossy().contains("one.example"));
    }

    #[test]
    fn recovery_paths_separate_agent_and_community() {
        let root = Path::new("agents");
        let first =
            ManagedAgentRuntimeKey::new("a".repeat(64), "wss://one.example").expect("valid pair");
        let other_agent = ManagedAgentRuntimeKey::new("b".repeat(64), "wss://one.example")
            .expect("valid agent pair");
        let other_community = ManagedAgentRuntimeKey::new("a".repeat(64), "wss://two.example")
            .expect("valid community pair");
        assert_ne!(
            recovery_path(root, &first),
            recovery_path(root, &other_agent)
        );
        assert_ne!(
            recovery_path(root, &first),
            recovery_path(root, &other_community)
        );
    }

    #[test]
    fn recovery_env_overwrites_inherited_path_with_the_pair_path() {
        let key =
            ManagedAgentRuntimeKey::new("a".repeat(64), "wss://one.example").expect("valid pair");
        let root = Path::new("agents");
        let mut command = Command::new("unused-test-command");
        command.env(RECOVERY_PATH_ENV, "foreign-journal.json");
        apply_recovery_env(&mut command, root, &key);
        let path = recovery_path(root, &key);
        assert_eq!(
            command
                .get_envs()
                .find(|(name, _)| *name == RECOVERY_PATH_ENV)
                .map(|(_, value)| value),
            Some(Some(path.as_os_str()))
        );
    }
}
