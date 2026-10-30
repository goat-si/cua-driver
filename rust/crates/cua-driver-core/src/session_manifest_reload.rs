//! Live reload of the trusted, host-owned capability manifest file.
//!
//! The host that launched the driver owns the manifest file and replaces it
//! when its grants change. Every tool admission asks
//! [`ConfiguredManifest::current`] for the manifest to authorize against;
//! the file's bytes are read and hashed every time (a manifest is small), and
//! re-parsed only when the hash differs from the applied version and from a
//! version already found invalid. File metadata never decides on its own: a
//! same-size rewrite within one modification-time tick, or on a platform
//! without a stable file id, is still seen. An invalid replacement keeps the
//! previously applied manifest and is reported through
//! [`ConfiguredManifest::status_json`]. Nothing an agent can call triggers a
//! reload except a change to that host-owned file.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use sha2::{Digest, Sha256};

use super::{parse_manifest_bytes, SessionManifest};
use crate::authorization::PermissionMode;

/// One applied manifest version. `generation` increases by one for every
/// version applied by the same store, so holders can order versions.
#[derive(Debug, Clone)]
pub struct AppliedManifest {
    pub generation: u64,
    pub manifest: Arc<SessionManifest>,
}

#[derive(Debug)]
struct ReloadState {
    /// SHA-256 of the applied version's file bytes.
    file_sha256: String,
    /// SHA-256 of the latest version found invalid, so it is not re-parsed
    /// at every call while it stays in place.
    rejected_sha256: Option<String>,
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
        let bytes = std::fs::read(&path).map_err(|error| {
            format!(
                "failed to read capability manifest {}: {error}",
                path.display()
            )
        })?;
        let manifest = parse_manifest_bytes(&path, &bytes)?;
        Ok(Self {
            state: Mutex::new(ReloadState {
                file_sha256: sha256_hex(&bytes),
                rejected_sha256: None,
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
        let bytes = match std::fs::read(&self.path) {
            Ok(bytes) => bytes,
            Err(error) => {
                // For example mid-replacement: keep the applied version and
                // look again on the next call.
                state.last_error = Some(format!(
                    "failed to read capability manifest {}: {error}",
                    self.path.display()
                ));
                return;
            }
        };
        let file_sha256 = sha256_hex(&bytes);
        if file_sha256 == state.file_sha256 {
            // The applied version is in place: no error applies.
            state.last_error = None;
            state.rejected_sha256 = None;
            return;
        }
        if state.rejected_sha256.as_deref() == Some(file_sha256.as_str()) {
            return;
        }
        state.parses += 1;
        let parsed = parse_manifest_bytes(&self.path, &bytes).and_then(|mut manifest| {
            manifest.inherit_lease(&state.applied.manifest);
            if let Some(mode) = self.mode {
                manifest.validate_for_mode(mode)?;
            }
            Ok(manifest)
        });
        match parsed {
            Ok(manifest) => {
                state.applied = AppliedManifest {
                    generation: state.applied.generation + 1,
                    manifest: Arc::new(manifest),
                };
                state.file_sha256 = file_sha256;
                state.rejected_sha256 = None;
                state.last_error = None;
            }
            Err(error) => {
                state.rejected_sha256 = Some(file_sha256);
                state.last_error = Some(error);
            }
        }
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
    fn an_unchanged_file_is_not_parsed_again() {
        let (_dir, path, store) = store(NARROW);
        for _ in 0..3 {
            store.current();
        }
        assert_eq!(store.parses(), 1);
        // Same bytes under a new file: hashed, never parsed.
        replace(&path, NARROW);
        let applied = store.current();
        assert_eq!(applied.generation, 1);
        assert_eq!(store.parses(), 1);
    }

    #[test]
    fn a_same_size_rewrite_in_place_applies_at_the_next_call() {
        // Same length, same file, same whole-second modification time (as a
        // coarse clock records two quick writes): only the bytes differ.
        let tick = std::time::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let pin = |path: &std::path::Path| {
            std::fs::File::options()
                .write(true)
                .open(path)
                .unwrap()
                .set_modified(tick)
                .unwrap();
        };
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("capability-manifest.yaml");
        std::fs::write(
            &path,
            "version: 3\nexpires_after: 2h\nallow:\n  tools: [get_config]\n",
        )
        .unwrap();
        pin(&path);
        let store = ConfiguredManifest::load(path.clone(), Some(PermissionMode::Standard)).unwrap();
        let before = std::fs::metadata(&path).unwrap();
        let first = store.current();
        std::fs::write(
            &path,
            "version: 3\nexpires_after: 1h\nallow:\n  tools: [get_config]\n",
        )
        .unwrap();
        pin(&path);
        let rewritten = std::fs::metadata(&path).unwrap();
        assert_eq!(rewritten.len(), before.len());
        assert_eq!(rewritten.modified().unwrap(), before.modified().unwrap());

        let after = store.current();
        assert_eq!(after.generation, first.generation + 1);
        // The narrowed lease counts from the original approval.
        assert_eq!(
            after.manifest.expires_unix_ms(),
            Some(first.manifest.loaded_unix_ms + Duration::from_secs(3600).as_millis())
        );
    }

    #[test]
    fn a_read_failure_is_reported_only_while_it_lasts() {
        let (dir, path, store) = store(NARROW);
        let aside = dir.path().join("aside.yaml");
        std::fs::rename(&path, &aside).unwrap();
        store.current();
        assert!(store.status_json()["last_error"].is_string());
        // The same file is back (same identity): the failure no longer applies.
        std::fs::rename(&aside, &path).unwrap();
        assert!(store.status_json()["last_error"].is_null());
        // An invalid version's error stays while that version stays.
        replace(
            &path,
            "version: 3\nallow:\n  tools: [not_a_reviewed_tool]\n",
        );
        store.current();
        store.current();
        assert!(store.status_json()["last_error"].is_string());
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
