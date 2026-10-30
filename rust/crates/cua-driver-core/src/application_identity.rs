//! What the capability manifest needs to know about a running app, kept for
//! as long as the process instance lives.
//!
//! Every call under a manifest asks the platform which bundle id and launch
//! path a pid has, by listing every running app. For one process instance the
//! answer does not change, so it is kept per pid together with the process
//! fingerprint (pid, start time, executable) it was read for. A lookup is
//! served only while the live fingerprint still proves the same instance:
//! a quit and relaunch, or a pid reused by another process, changes the start
//! time and reads the app list again. The cache holds no grant and no
//! decision; the manifest is evaluated on every call from what it returns.

use std::collections::HashMap;
use std::sync::Mutex;

use serde_json::Value;

use crate::browser::ProcessFingerprint;

/// The most pids kept; the cache is cleared when it would grow past this.
const MAX_ENTRIES: usize = 512;

struct Entry {
    fingerprint: ProcessFingerprint,
    bundle_id: Option<Value>,
    launch_path: Option<Value>,
}

/// The identity read for one process instance.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ApplicationIdentity {
    pub(crate) bundle_id: Option<Value>,
    pub(crate) launch_path: Option<Value>,
}

#[derive(Default)]
pub(crate) struct ApplicationIdentityCache {
    entries: Mutex<HashMap<i64, Entry>>,
}

impl ApplicationIdentityCache {
    /// The identity read for this exact process instance, when there is one.
    /// A fingerprint without a start time cannot prove the instance, so it is
    /// never served from the cache.
    pub(crate) fn lookup(
        &self,
        pid: i64,
        fingerprint: &ProcessFingerprint,
    ) -> Option<ApplicationIdentity> {
        fingerprint.start_time?;
        let entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        let entry = entries.get(&pid)?;
        entry
            .fingerprint
            .matches(fingerprint)
            .then(|| ApplicationIdentity {
                bundle_id: entry.bundle_id.clone(),
                launch_path: entry.launch_path.clone(),
            })
    }

    /// Keep what was read for this process instance. An answer with neither
    /// field is not kept (the app may simply not be listed yet), nor is one
    /// for an instance that cannot be proven.
    pub(crate) fn store(
        &self,
        pid: i64,
        fingerprint: &ProcessFingerprint,
        identity: ApplicationIdentity,
    ) {
        if fingerprint.start_time.is_none()
            || (identity.bundle_id.is_none() && identity.launch_path.is_none())
        {
            return;
        }
        let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        if entries.len() >= MAX_ENTRIES && !entries.contains_key(&pid) {
            entries.clear();
        }
        entries.insert(
            pid,
            Entry {
                fingerprint: fingerprint.clone(),
                bundle_id: identity.bundle_id,
                launch_path: identity.launch_path,
            },
        );
    }

    /// Drop everything (a session ended).
    pub(crate) fn clear(&self) {
        self.entries
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fingerprint(pid: i64, start: Option<u64>) -> ProcessFingerprint {
        ProcessFingerprint {
            pid,
            start_time: start,
            executable: Some("/Applications/Example.app/Contents/MacOS/Example".into()),
        }
    }

    fn identity(bundle: &str) -> ApplicationIdentity {
        ApplicationIdentity {
            bundle_id: Some(Value::String(bundle.into())),
            launch_path: Some(Value::String("/Applications/Example.app".into())),
        }
    }

    #[test]
    fn an_identity_is_served_only_to_the_instance_it_was_read_for() {
        let cache = ApplicationIdentityCache::default();
        let first = fingerprint(42, Some(10));
        assert_eq!(cache.lookup(42, &first), None);
        cache.store(42, &first, identity("com.example.app"));
        assert_eq!(cache.lookup(42, &first), Some(identity("com.example.app")));
        // The same pid, another start time: another process.
        assert_eq!(cache.lookup(42, &fingerprint(42, Some(11))), None);
        // Another pid is not the cached one.
        assert_eq!(cache.lookup(43, &fingerprint(43, Some(10))), None);
    }

    #[test]
    fn an_instance_without_a_proven_start_time_is_never_cached_or_served() {
        let cache = ApplicationIdentityCache::default();
        let unproven = fingerprint(42, None);
        cache.store(42, &unproven, identity("com.example.app"));
        assert_eq!(cache.lookup(42, &unproven), None);
        let proven = fingerprint(42, Some(10));
        cache.store(42, &proven, identity("com.example.app"));
        assert_eq!(cache.lookup(42, &unproven), None);
    }

    #[test]
    fn an_answer_with_no_identity_is_not_kept() {
        let cache = ApplicationIdentityCache::default();
        let proven = fingerprint(42, Some(10));
        cache.store(
            42,
            &proven,
            ApplicationIdentity {
                bundle_id: None,
                launch_path: None,
            },
        );
        assert_eq!(cache.lookup(42, &proven), None);
    }

    #[test]
    fn clearing_forgets_every_pid() {
        let cache = ApplicationIdentityCache::default();
        let proven = fingerprint(42, Some(10));
        cache.store(42, &proven, identity("com.example.app"));
        cache.clear();
        assert_eq!(cache.lookup(42, &proven), None);
    }
}
