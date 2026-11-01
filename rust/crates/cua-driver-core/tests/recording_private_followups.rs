#![cfg(unix)]

use std::os::unix::fs::{symlink, MetadataExt, PermissionsExt};
use std::path::Path;
use std::process::Command;

use cua_driver_core::recording::RecordingSession;
use cua_driver_core::recording_loader::{load, LoadError};
use cua_driver_core::recording_render::{render, RenderOptions};
use cua_driver_core::video::{set_video_backend_factory, VideoBackend, VideoBackendFactory};

struct FailingVideo;

impl VideoBackendFactory for FailingVideo {
    fn start(&self, output: &Path) -> anyhow::Result<Box<dyn VideoBackend>> {
        assert!(output.is_file(), "video must be precreated before start");
        anyhow::bail!("injected video start failure")
    }
}

#[test]
fn failed_video_start_removes_precreated_output() {
    set_video_backend_factory(Box::new(FailingVideo));
    let root = tempfile::tempdir().unwrap();
    let session = RecordingSession::new();
    session
        .start(root.path().to_str().unwrap(), true, None)
        .unwrap();
    assert!(!root.path().join("recording.mp4").exists());
    assert!(matches!(load(root.path()), Err(LoadError::VideoMissing(_))));
    session.stop_owner(None).unwrap();
}

fn render_fixture(planted_link: bool) {
    const CHILD: &str = "CUA_PRIVATE_RENDER_TEST_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let bin = tempfile::tempdir().unwrap();
        let ffmpeg = bin.path().join("ffmpeg");
        std::fs::write(&ffmpeg, b"#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(&ffmpeg, std::fs::Permissions::from_mode(0o700)).unwrap();
        let test_name = if planted_link {
            "render_dump_refuses_symlink"
        } else {
            "render_dump_is_owner_only"
        };
        let child = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", test_name, "--nocapture"])
            .env(CHILD, "1")
            .env("PATH", bin.path())
            .output()
            .unwrap();
        assert!(
            child.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&child.stdout),
            String::from_utf8_lossy(&child.stderr)
        );
        return;
    }

    unsafe {
        libc::umask(0o022);
    }
    let root = tempfile::tempdir().unwrap();
    let recording = root.path().join("recording");
    std::fs::create_dir(&recording).unwrap();
    std::fs::set_permissions(&recording, std::fs::Permissions::from_mode(0o700)).unwrap();
    std::fs::write(recording.join("recording.mp4"), b"video").unwrap();
    std::fs::write(
        recording.join("session.json"),
        br#"{"video":{"width":100,"height":100}}"#,
    )
    .unwrap();
    let turn = recording.join("turn-00001");
    std::fs::create_dir(&turn).unwrap();
    std::fs::write(
        turn.join("action.json"),
        br#"{"tool":"click","arguments":{"x":10,"y":20},"t_ms_from_session_start":1000}"#,
    )
    .unwrap();
    let dump = recording.join("render.expression.txt");
    let target = root.path().join("target");
    if planted_link {
        std::fs::write(&target, b"untouched").unwrap();
        symlink(&target, &dump).unwrap();
    }
    let result = render(
        &recording,
        &root.path().join("out.mp4"),
        &RenderOptions::default(),
    )
    .unwrap();
    assert_eq!(result.zoom_region_count, 1);
    if planted_link {
        assert_eq!(std::fs::read(&target).unwrap(), b"untouched");
        assert!(std::fs::symlink_metadata(&dump)
            .unwrap()
            .file_type()
            .is_symlink());
    } else {
        assert_eq!(std::fs::metadata(&dump).unwrap().mode() & 0o777, 0o600);
    }
}

#[test]
fn render_dump_is_owner_only() {
    render_fixture(false);
}

#[test]
fn render_dump_refuses_symlink() {
    render_fixture(true);
}
