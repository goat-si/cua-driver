use super::*;
use std::fs::{self, FileTimes, OpenOptions};
use std::io::Write;
use std::process::Command;

fn load(path: &Path, field: &str) -> SessionManifest {
    let dir = tempfile::tempdir().unwrap();
    let manifest_path = dir.path().join("manifest.yaml");
    let granted_path = super::canonical_manifest_path(path).unwrap();
    fs::write(
        &manifest_path,
        format!(
            "version: 3\nallow:\n  tools: [get_window_state, click, kill_app]\nresources:\n  apps:\n    - {field}: {}\n      windows: all\n      terminate: any\n",
            serde_json::to_string(&granted_path).unwrap(),
        ),
    )
    .unwrap();
    super::load_manifest(&manifest_path).unwrap()
}

fn allowed(manifest: &SessionManifest, path: &Path, field: &str, expected: bool) {
    let executable = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    for (adapter, kind) in [
        ("private_observation", "window"),
        ("private_observation", "application"),
        ("desktop_input", "window_input"),
        ("desktop_input", "application_input"),
        ("process_control", "process"),
    ] {
        if adapter == "process_control" && field == "bundle_id" {
            continue;
        }
        let resource = serde_json::json!({
            "kind": kind,
            "pid": 42,
            "window_id": 7,
            "bundle_id": path.to_string_lossy(),
            "fingerprint": {"pid": 42, "start_time": 1, "executable": executable},
        });
        assert_eq!(
            manifest
                .authorize_protected_resource(adapter, &resource)
                .is_ok(),
            expected,
            "{field}: {adapter}/{kind}",
        );
    }
}

#[test]
fn windows_executable_replacement_requires_a_new_manifest() {
    for field in ["bundle_id", "executable"] {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("dummy.exe");
        let replacement = dir.path().join("replacement.exe");
        fs::write(&target, b"dummy A").unwrap();
        fs::write(&replacement, b"dummy B").unwrap();
        let manifest = load(&target, field);
        let clone = manifest.clone();
        allowed(&manifest, &target, field, true);
        fs::remove_file(&target).unwrap();
        fs::rename(&replacement, &target).unwrap();
        allowed(&manifest, &target, field, false);
        allowed(&clone, &target, field, false);
        allowed(&load(&target, field), &target, field, true);
    }
}

#[test]
fn windows_executable_in_place_change_with_restored_metadata_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("dummy.exe");
    fs::write(&target, b"dummy A").unwrap();
    let manifest = load(&target, "bundle_id");
    let modified = fs::metadata(&target).unwrap().modified().unwrap();
    {
        let mut file = OpenOptions::new().write(true).open(&target).unwrap();
        file.write_all(b"dummy B").unwrap();
        file.set_times(FileTimes::new().set_modified(modified))
            .unwrap();
    }
    allowed(&manifest, &target, "bundle_id", false);
}

#[test]
fn windows_executable_unavailable_identity_never_becomes_a_path_only_grant() {
    for field in ["bundle_id", "executable"] {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("dummy.exe");
        let missing = load(&target, field);
        fs::write(&target, b"dummy A").unwrap();
        allowed(&missing, &target, field, false);
        let manifest = load(&target, field);
        allowed(&manifest, &target, field, true);
        fs::remove_file(&target).unwrap();
        allowed(&manifest, &target, field, false);
    }
}

#[test]
fn windows_executable_identical_bytes_in_a_new_file_are_not_the_approved_file() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("dummy.exe");
    let replacement = dir.path().join("replacement.exe");
    fs::write(&target, b"dummy A").unwrap();
    fs::write(&replacement, b"dummy A").unwrap();
    let manifest = load(&target, "bundle_id");
    let modified = fs::metadata(&target).unwrap().modified().unwrap();
    OpenOptions::new()
        .write(true)
        .open(&replacement)
        .unwrap()
        .set_times(FileTimes::new().set_modified(modified))
        .unwrap();
    fs::remove_file(&target).unwrap();
    fs::rename(&replacement, &target).unwrap();
    allowed(&manifest, &target, "bundle_id", false);
}

#[test]
fn windows_case_sensitive_paths_do_not_alias_distinct_files() {
    let dir = tempfile::tempdir().unwrap();
    let enabled = Command::new("fsutil.exe")
        .args(["file", "setCaseSensitiveInfo"])
        .arg(dir.path())
        .arg("enable")
        .status()
        .is_ok_and(|status| status.success());
    if !enabled {
        eprintln!(
            "skipping case-sensitive directory regression: could not enable the directory flag"
        );
        return;
    }

    let approved = dir.path().join("dummy.exe");
    let replacement = dir.path().join("DUMMY.exe");
    fs::write(&approved, b"approved A").unwrap();
    fs::write(&replacement, b"replacement B").unwrap();
    assert_eq!(
        super::normalize_windows_executable_path(&approved.to_string_lossy()),
        super::normalize_windows_executable_path(&replacement.to_string_lossy())
    );

    let manifest = load(&approved, "bundle_id");
    allowed(&manifest, &approved, "bundle_id", true);
    allowed(&manifest, &replacement, "bundle_id", false);
}
