//! Live reload of the trusted, host-owned capability manifest file.
//!
//! The host that launched the driver owns the manifest file and replaces it
//! atomically when its grants change. Every tool admission asks
//! [`ConfiguredManifest::current`] for the manifest to authorize against; the
//! file is re-read only when its identity (size, modification time and, on
//! Unix, device and inode) changed, and re-parsed only when its bytes changed.
//! An invalid replacement keeps the previously applied manifest and is
//! reported through [`ConfiguredManifest::status_json`]. Nothing an agent can
//! call triggers a reload except a change to that host-owned file.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use sha2::{Digest, Sha256};

use super::{parse_manifest_bytes, SessionManifest};
use crate::authorization::PermissionMode;

#[derive(Debug, Clone, PartialEq, Eq)]
struct FileIdentity {
    len: u64,
    modified: Option<SystemTime>,
    #[cfg(unix)]
    dev: u64,
    #[cfg(unix)]
    ino: u64,
}

impl FileIdentity {
    fn of(metadata: &std::fs::Metadata) -> Self {
        #[cfg(unix)]
        use std::os::unix::fs::MetadataExt as _;
        Self {
            len: metadata.len(),
            modified: metadata.modified().ok(),
            #[cfg(unix)]
            dev: metadata.dev(),
            #[cfg(unix)]
            ino: metadata.ino(),
        }
    }
}

/// One applied manifest version. `generation` increases by one for every
/// version applied by the same store, so holders can order versions.
#[derive(Debug, Clone)]
pub struct AppliedManifest {
    pub generation: u64,
    pub manifest: Arc<SessionManifest>,
}

#[derive(Debug)]
struct ReloadState {
    identity: Option<FileIdentity>,
    file_sha256: String,
    applied: AppliedManifest,
    last_error: Option<String>,
    parses: u64,
}

/// A capability manifest backed by a host-owned file.
#[derive(Debug)]
pub struct ConfiguredManifest {
    path: PathBuf,
    mode: Option<PermissionMode>,
    state: Mutex<ReloadState>,
}

fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

impl ConfiguredManifest {
    /// Load the file once. A missing or invalid file at load is an error,
    /// exactly as for a manifest that is never reloaded. When `mode` is
    /// known every version must validate for it before it is applied.
    pub fn load(path: PathBuf, mode: Option<PermissionMode>) -> Result<Self, String> {
        let metadata = std::fs::metadata(&path).map_err(|error| {
            format!(
                "failed to read capability manifest {}: {error}",
                path.display()
            )
        })?;
        let bytes = std::fs::read(&path).map_err(|error| {
            format!(
                "failed to read capability manifest {}: {error}",
                path.display()
            )
        })?;
        let manifest = parse_manifest_bytes(&path, &bytes)?;
        Ok(Self {
            state: Mutex::new(ReloadState {
                identity: Some(FileIdentity::of(&metadata)),
                file_sha256: sha256_hex(&bytes),
                applied: AppliedManifest {
                    generation: 1,
                    manifest: Arc::new(manifest),
                },
                last_error: None,
                parses: 1,
            }),
            path,
            mode,
        })
    }

    /// The manifest to authorize the next call against. Re-checks the file
    /// first; the whole check is one critical section, so concurrent callers
    /// never apply versions out of order.
    pub fn current(&self) -> AppliedManifest {
        let mut state = self.state.lock().unwrap();
        self.refresh(&mut state);
        state.applied.clone()
    }

    fn refresh(&self, state: &mut ReloadState) {
        let before = match std::fs::metadata(&self.path) {
            Ok(metadata) => FileIdentity::of(&metadata),
            Err(error) => {
                // Transient (for example mid-replacement): keep the applied
                // version and look again on the next call.
                state.last_error = Some(format!(
                    "failed to read capability manifest {}: {error}",
                    self.path.display()
                ));
                return;
            }
        };
        if state.identity.as_ref() == Some(&before) {
            return;
        }
        let bytes = match std::fs::read(&self.path) {
            Ok(bytes) => bytes,
            Err(error) => {
                state.last_error = Some(format!(
                    "failed to read capability manifest {}: {error}",
                    self.path.display()
                ));
                return;
            }
        };
        // The file changed while it was read: apply nothing and look again
        // on the next call, so one version is never mixed with another.
        match std::fs::metadata(&self.path) {
            Ok(metadata) if FileIdentity::of(&metadata) == before => {}
            _ => return,
        }
        let file_sha256 = sha256_hex(&bytes);
        if file_sha256 == state.file_sha256 {
            state.identity = Some(before);
            state.last_error = None;
            return;
        }
        // An invalid version is not retried until the file changes again.
        state.identity = Some(before);
        state.parses += 1;
        let mut manifest = match parse_manifest_bytes(&self.path, &bytes) {
            Ok(manifest) => manifest,
            Err(error) => {
                state.last_error = Some(error);
                return;
            }
        };
        manifest.inherit_lease(&state.applied.manifest);
        if let Some(mode) = self.mode {
            if let Err(error) = manifest.validate_for_mode(mode) {
                state.last_error = Some(error);
                return;
            }
        }
        state.applied = AppliedManifest {
            generation: state.applied.generation + 1,
            manifest: Arc::new(manifest),
        };
        state.file_sha256 = file_sha256;
        state.last_error = None;
    }

    /// Content-free reload status for health and configuration output.
    pub fn status_json(&self) -> serde_json::Value {
        let mut state = self.state.lock().unwrap();
        self.refresh(&mut state);
        serde_json::json!({
            "applied_sha256": state.applied.manifest.sha256(),
            "applied_file_sha256": state.file_sha256,
            "applied_generation": state.applied.generation,
            "last_error": state.last_error,
        })
    }

    #[cfg(test)]
    fn parses(&self) -> u64 {
        self.state.lock().unwrap().parses
    }
}

#[cfg(all(test, feature = "yaml"))]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    const WIDE: &str = "version: 3\nresources:\n  browser:\n    origins: [\"https://app.example\", \"https://docs.example\"]\nallow:\n  tools: [browser_click]\n";
    const NARROW: &str = "version: 3\nresources:\n  browser:\n    origins: [\"https://app.example\"]\nallow:\n  tools: [browser_click]\n";

    /// Replace the file the way GOAT does: write a sibling, then rename.
    fn replace(path: &std::path::Path, source: &str) {
        let temp = path.with_extension("tmp");
        std::fs::write(&temp, source).unwrap();
        std::fs::rename(&temp, path).unwrap();
    }

    fn store(source: &str) -> (tempfile::TempDir, PathBuf, ConfiguredManifest) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("capability-manifest.yaml");
        std::fs::write(&path, source).unwrap();
        let store = ConfiguredManifest::load(path.clone(), Some(PermissionMode::Standard)).unwrap();
        (dir, path, store)
    }

    #[test]
    fn a_widened_manifest_applies_at_the_next_call_without_restart() {
        let (_dir, path, store) = store(NARROW);
        let first = store.current();
        assert!(first
            .manifest
            .authorize_browser_url("https://docs.example/a")
            .is_err());
        replace(&path, WIDE);
        let second = store.current();
        assert_eq!(second.generation, first.generation + 1);
        second
            .manifest
            .authorize_browser_url("https://docs.example/a")
            .unwrap();
        assert_ne!(first.manifest.sha256(), second.manifest.sha256());
        assert_eq!(
            store.status_json()["applied_sha256"],
            second.manifest.sha256()
        );
        assert_eq!(
            store.status_json()["applied_file_sha256"],
            sha256_hex(WIDE.as_bytes())
        );
    }

    #[test]
    fn a_narrowed_manifest_applies_at_the_next_call() {
        let (_dir, path, store) = store(WIDE);
        store
            .current()
            .manifest
            .authorize_browser_url("https://docs.example/")
            .unwrap();
        replace(&path, NARROW);
        assert!(store
            .current()
            .manifest
            .authorize_browser_url("https://docs.example/")
            .is_err());
    }

    #[test]
    fn an_invalid_replacement_keeps_the_applied_manifest_and_reports_why() {
        let (_dir, path, store) = store(NARROW);
        let applied = store.current();
        replace(
            &path,
            "version: 3\nallow:\n  tools: [not_a_reviewed_tool]\n",
        );
        let after = store.current();
        assert_eq!(after.generation, applied.generation);
        assert_eq!(after.manifest.sha256(), applied.manifest.sha256());
        let status = store.status_json();
        assert!(status["last_error"]
            .as_str()
            .is_some_and(|error| error.contains("not_a_reviewed_tool")));
        // Not re-parsed while the invalid file stays the same.
        let parses = store.parses();
        store.current();
        assert_eq!(store.parses(), parses);
        // A corrected file applies and clears the error.
        replace(&path, WIDE);
        assert_eq!(store.current().generation, applied.generation + 1);
        assert!(store.status_json()["last_error"].is_null());
    }

    #[test]
    fn an_unchanged_file_is_not_read_or_parsed_again() {
        let (_dir, path, store) = store(NARROW);
        for _ in 0..3 {
            store.current();
        }
        assert_eq!(store.parses(), 1);
        // Same bytes under a new identity: read, hashed, never parsed.
        replace(&path, NARROW);
        let applied = store.current();
        assert_eq!(applied.generation, 1);
        assert_eq!(store.parses(), 1);
    }

    #[test]
    fn a_reload_never_revives_an_idle_expired_lease_or_extends_expiry() {
        let (_dir, path, store) = store(
            "version: 3\nexpires_after: 1h\nidle_timeout: 1s\nallow:\n  tools: [get_config]\n",
        );
        let first = store.current().manifest;
        *first.last_authorized_dispatch.lock().unwrap() =
            Instant::now().checked_sub(Duration::from_secs(2)).unwrap();
        // The replacement drops idle_timeout and lengthens expires_after.
        replace(
            &path,
            "version: 3\nexpires_after: 20h\nallow:\n  tools: [get_config]\n",
        );
        let after = store.current().manifest;
        assert!(after.is_idle_expired(), "idle expiry is terminal");
        assert!(after.check_lifetime().is_err());
        assert_eq!(after.expires_unix_ms(), first.expires_unix_ms());
    }

    #[test]
    fn a_reload_with_a_shorter_lease_takes_effect_from_the_original_approval() {
        let (_dir, path, store) =
            store("version: 3\nexpires_after: 2h\nallow:\n  tools: [get_config]\n");
        let first = store.current().manifest;
        replace(
            &path,
            "version: 3\nexpires_after: 1h\nallow:\n  tools: [get_config]\n",
        );
        let after = store.current().manifest;
        assert_eq!(
            after.expires_unix_ms(),
            Some(first.loaded_unix_ms + Duration::from_secs(3600).as_millis())
        );
    }

    #[tokio::test]
    async fn get_config_reports_the_manifest_the_call_is_authorized_against() {
        let (_dir, _path, store) = store(NARROW);
        let manifest = store.current().manifest;
        let context = crate::session_authorization::SessionAuthorizationRegistry::with_ceiling(
            crate::session_authorization::SessionModeCeiling::for_trusted_sessions(
                [PermissionMode::Standard],
                false,
                Duration::from_secs(60),
                Duration::from_secs(30),
            )
            .unwrap(),
        )
        .compatibility_context(PermissionMode::Standard, Some(manifest.clone()))
        .unwrap();
        let reported = crate::tool::with_test_dispatch_context(context, async {
            super::super::applied_capability_manifest_json()
        })
        .await;
        assert_eq!(reported["sha256"], manifest.sha256());
        assert!(super::super::applied_capability_manifest_json()["sha256"].is_null());
    }
}
