//! Short-lived evidence from actual inference requests, never catalog probes.
//! Files contain only model IDs and timestamps, keyed by a hash of endpoint + key.

use std::{
    collections::{HashMap, HashSet},
    io::{Read, Write},
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};

const REJECTION_TTL: Duration = Duration::from_secs(15 * 60);
const MAX_SAVED_BYTES: u64 = 128 * 1024;
static TEMP_ID: AtomicU64 = AtomicU64::new(0);
type Rejections = HashMap<String, u64>;

pub(super) struct AvailabilityStore {
    root: PathBuf,
    accounts: Mutex<HashMap<[u8; 32], Rejections>>,
}

#[derive(Clone)]
pub(super) struct AvailabilityScope {
    store: Arc<AvailabilityStore>,
    key: [u8; 32],
}

#[derive(Deserialize, Serialize)]
struct Saved {
    version: u8,
    models: Rejections,
}

impl AvailabilityStore {
    pub(super) fn new(root: PathBuf) -> Self {
        Self {
            root,
            accounts: Mutex::new(HashMap::new()),
        }
    }

    pub(super) fn scope(self: &Arc<Self>, key: [u8; 32]) -> AvailabilityScope {
        AvailabilityScope {
            store: self.clone(),
            key,
        }
    }

    fn path(&self, key: &[u8; 32]) -> PathBuf {
        let hash: String = key.iter().map(|byte| format!("{byte:02x}")).collect();
        self.root.join(format!("{hash}.json"))
    }

    fn load(&self, key: &[u8; 32]) -> Rejections {
        let saved = (|| -> Option<Saved> {
            let file = std::fs::File::open(self.path(key)).ok()?;
            let mut bytes = Vec::new();
            file.take(MAX_SAVED_BYTES + 1)
                .read_to_end(&mut bytes)
                .ok()?;
            if bytes.len() as u64 > MAX_SAVED_BYTES {
                return None;
            }
            serde_json::from_slice(&bytes).ok()
        })();
        saved
            .filter(|saved| saved.version == 1)
            .map(|saved| saved.models)
            .unwrap_or_default()
    }

    fn save(&self, key: &[u8; 32], models: &Rejections) -> std::io::Result<()> {
        std::fs::create_dir_all(&self.root)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&self.root, std::fs::Permissions::from_mode(0o700))?;
        }
        let path = self.path(key);
        let staging = path.with_extension(format!(
            "{}.{}.tmp",
            std::process::id(),
            TEMP_ID.fetch_add(1, Ordering::Relaxed)
        ));
        let bytes = serde_json::to_vec(&Saved {
            version: 1,
            models: models.clone(),
        })?;
        let result = (|| {
            let mut options = std::fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let mut file = options.open(&staging)?;
            file.write_all(&bytes)?;
            file.sync_all()?;
            drop(file);
            #[cfg(windows)]
            if path.exists() {
                std::fs::remove_file(&path)?;
            }
            std::fs::rename(&staging, &path)
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(staging);
        }
        result
    }
}

impl AvailabilityScope {
    pub(super) fn unavailable(&self) -> HashSet<String> {
        self.unavailable_at(now())
    }

    pub(super) fn reject(&self, model: &str) -> std::io::Result<()> {
        self.update(model, true, now())
    }

    pub(super) fn allow(&self, model: &str) -> std::io::Result<()> {
        self.update(model, false, now())
    }

    fn unavailable_at(&self, now: u64) -> HashSet<String> {
        let mut accounts = self
            .store
            .accounts
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let models = accounts
            .entry(self.key)
            .or_insert_with(|| self.store.load(&self.key));
        prune(models, now);
        models.keys().cloned().collect()
    }

    fn update(&self, model: &str, reject: bool, now: u64) -> std::io::Result<()> {
        let mut accounts = self
            .store
            .accounts
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let models = accounts
            .entry(self.key)
            .or_insert_with(|| self.store.load(&self.key));
        prune(models, now);
        let changed = if reject {
            models.insert(model.to_owned(), now);
            true
        } else {
            models.remove(model).is_some()
        };
        // Keep the in-memory evidence even if private disk storage is unwritable.
        if changed {
            self.store.save(&self.key, models)
        } else {
            Ok(())
        }
    }
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn prune(models: &mut Rejections, now: u64) {
    // A backward clock adjustment must not hide a model indefinitely either.
    models.retain(|_, rejected_at| {
        *rejected_at <= now && now - *rejected_at < REJECTION_TTL.as_secs()
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempDir(PathBuf);
    impl TempDir {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "zeron-nim-availability-{}-{}-{}",
                std::process::id(),
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_nanos(),
                TEMP_ID.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }
        fn store(&self) -> Arc<AvailabilityStore> {
            Arc::new(AvailabilityStore::new(self.0.join("availability")))
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn rejection_survives_restart_and_is_scoped_to_one_key() {
        let dir = TempDir::new();
        let store = dir.store();
        let account = store.scope([1; 32]);
        account.update("fixture/chat", true, 1000).unwrap();
        assert!(account.unavailable_at(1001).contains("fixture/chat"));
        assert!(store.scope([2; 32]).unavailable_at(1001).is_empty());
        let restarted = dir.store().scope([1; 32]);
        assert!(restarted.unavailable_at(1001).contains("fixture/chat"));
        assert_eq!(restarted.unavailable_at(1001).len(), 1);
    }

    #[test]
    fn expiry_and_backward_clock_allow_recovery() {
        let dir = TempDir::new();
        let account = dir.store().scope([1; 32]);
        account.update("fixture/chat", true, 1000).unwrap();
        assert!(!account.unavailable_at(1899).is_empty());
        assert!(account.unavailable_at(1900).is_empty());
        assert!(dir.store().scope([1; 32]).unavailable_at(1900).is_empty());
        assert!(dir.store().scope([1; 32]).unavailable_at(999).is_empty());
    }

    #[test]
    fn a_successful_explicit_retry_clears_only_that_model() {
        let dir = TempDir::new();
        let account = dir.store().scope([1; 32]);
        account.reject("fixture/first").unwrap();
        account.reject("fixture/second").unwrap();
        account.allow("fixture/first").unwrap();
        let restarted = dir.store().scope([1; 32]);
        assert_eq!(
            restarted.unavailable(),
            HashSet::from(["fixture/second".into()])
        );
    }

    #[test]
    fn disk_failure_does_not_forget_an_observed_rejection() {
        let dir = TempDir::new();
        std::fs::write(dir.0.join("availability"), "not a directory").unwrap();
        let account = dir.store().scope([1; 32]);
        assert!(account.reject("fixture/chat").is_err());
        assert!(account.unavailable().contains("fixture/chat"));
    }

    #[test]
    fn corrupt_and_future_files_are_not_evidence_against_models() {
        let dir = TempDir::new();
        let store = dir.store();
        std::fs::create_dir_all(&store.root).unwrap();
        for text in [
            "not json",
            r#"{"version":2,"models":{"fixture/chat":1000}}"#,
        ] {
            std::fs::write(store.path(&[1; 32]), text).unwrap();
            assert!(dir.store().scope([1; 32]).unavailable_at(1001).is_empty());
        }
    }

    #[test]
    fn persistence_contains_only_model_ids_and_timestamps_in_private_files() {
        let dir = TempDir::new();
        let store = dir.store();
        store
            .scope([1; 32])
            .update("fixture/chat", true, 1000)
            .unwrap();
        let path = store.path(&[1; 32]);
        assert_eq!(path.file_name().unwrap().to_str().unwrap().len(), 69);
        let value: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(
            value,
            serde_json::json!({"version":1,"models":{"fixture/chat":1000}})
        );
        assert_eq!(std::fs::read_dir(&store.root).unwrap().count(), 1);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o600
            );
            assert_eq!(
                std::fs::metadata(&store.root).unwrap().permissions().mode() & 0o777,
                0o700
            );
        }
    }

    #[test]
    fn a_success_without_a_rejection_does_not_write_state() {
        let dir = TempDir::new();
        dir.store().scope([1; 32]).allow("fixture/chat").unwrap();
        assert!(!dir.0.join("availability").exists());
    }
}
