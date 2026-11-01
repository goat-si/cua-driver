//! Source inventory for desktop-input worker boundaries.
//!
//! Enumerate at test runtime so a source added after compilation is not
//! silently omitted from the inventory.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

fn rust_sources(root: &Path) -> io::Result<Vec<PathBuf>> {
    fn visit(root: &Path, dir: &Path, files: &mut Vec<PathBuf>) -> io::Result<()> {
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            let ty = entry.file_type()?;
            if ty.is_symlink() {
                return Err(io::Error::other(format!(
                    "source inventory cannot follow symlink: {}",
                    path.display()
                )));
            }
            if ty.is_dir() {
                visit(root, &path, files)?;
            } else if ty.is_file() && path.extension().is_some_and(|ext| ext == "rs") {
                files.push(
                    path.strip_prefix(root)
                        .expect("source under root")
                        .to_owned(),
                );
            }
        }
        Ok(())
    }

    let mut files = Vec::new();
    visit(root, root, &mut files)?;
    files.sort();
    Ok(files)
}

#[test]
fn source_manifest_covers_all_rust_sources() {
    let fixture = tempfile::tempdir().unwrap();
    fs::create_dir(fixture.path().join("nested")).unwrap();
    fs::write(fixture.path().join("first.rs"), "").unwrap();
    fs::write(fixture.path().join("nested/new.rs"), "").unwrap();
    fs::write(fixture.path().join("nested/ignored.txt"), "").unwrap();
    assert_eq!(
        rust_sources(fixture.path()).unwrap(),
        [PathBuf::from("first.rs"), PathBuf::from("nested/new.rs")]
    );

    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let files = rust_sources(&root).unwrap();
    for expected in [
        "lib.rs",
        "input/worker_context.rs",
        "tools/get_window_state.rs",
        "ax/actionability.rs",
        "ax/vocabulary.rs",
        "tools/lease_wiring_tests/source_inventory_tests.rs",
    ] {
        assert!(
            files.binary_search(&PathBuf::from(expected)).is_ok(),
            "missing {expected}"
        );
    }
    assert!(!files.is_empty());
}
