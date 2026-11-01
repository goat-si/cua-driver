//! Source inventory for desktop-input worker boundaries.
//!
//! Enumerate at test runtime so a source added after compilation is not
//! silently omitted from the inventory.

use super::source_exemptions::{Category, Entry, ENTRIES};
use super::source_scanner::{scan, test_only_files, Site};
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

fn inventory() -> Vec<Site> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let files = rust_sources(&root).unwrap();
    let tests = test_only_files(&root, &files).unwrap();
    let mut sites = Vec::new();
    for file in files {
        if tests.contains(&file) {
            continue;
        }
        let source = fs::read_to_string(root.join(&file)).unwrap();
        sites.extend(
            scan(&file.to_string_lossy(), &source)
                .unwrap_or_else(|e| panic!("{}: {e}", file.display())),
        );
    }
    sites
}

#[test]
fn all_in_scope_mutating_spawns_propagate_worker_context() {
    let sites = inventory();
    let unclassified = unclassified(&sites, ENTRIES);
    let entries: Vec<_> = ENTRIES.iter().collect();
    let errors = entry_errors(&entries, &sites);
    assert!(
        unclassified.is_empty() && errors.is_empty(),
        "unclassified spawn sites: {unclassified:#?}; exemption errors: {errors:#?}"
    );
}

fn unclassified<'a>(sites: &'a [Site], entries: &[Entry]) -> Vec<&'a Site> {
    sites
        .iter()
        .filter(|site| !entries.iter().any(|entry| entry.matches(site)))
        .collect()
}

fn entry_errors(entries: &[&Entry], sites: &[Site]) -> Vec<String> {
    let mut errors = Vec::new();
    for (index, entry) in entries.iter().enumerate() {
        if entry.reason.is_empty() {
            errors.push(format!("empty reason: {entry:?}"));
        }
        if entries[..index].iter().any(|other| {
            other.file == entry.file
                && other.item == entry.item
                && other.primitive == entry.primitive
                && other.fingerprint == entry.fingerprint
                && other.occurrence == entry.occurrence
        }) {
            errors.push(format!("duplicate exception: {entry:?}"));
        }
        let matches = sites.iter().filter(|site| entry.matches(site)).count();
        if matches != 1 {
            errors.push(format!("expected one site, found {matches}: {entry:?}"));
        }
    }
    errors
}

#[test]
fn read_only_exemptions_match_exact_call_sites() {
    let sites = inventory();
    let entries: Vec<_> = ENTRIES
        .iter()
        .filter(|e| e.category != Category::Deferred)
        .collect();
    let errors = entry_errors(&entries, &sites);
    assert!(errors.is_empty(), "{errors:#?}");
}

#[test]
fn deferred_mutations_match_explicit_obligations() {
    let sites = inventory();
    let entries: Vec<_> = ENTRIES
        .iter()
        .filter(|e| e.category == Category::Deferred)
        .collect();
    assert_eq!(entries.len(), 4);
    let errors = entry_errors(&entries, &sites);
    assert!(errors.is_empty(), "{errors:#?}");
}

#[test]
fn scanner_handles_comments_strings_aliases_and_multiline_calls() {
    let sites = scan(
        "fixture.rs",
        r###"
        use tokio::task::spawn_blocking as launch;
        fn f() {
            // tokio::spawn(async {});
            let _ = r#"std::thread::spawn(|| {})"#;
            #[cfg(test)] let _hidden = tokio::spawn(async {});
            #[cfg(test)] { tokio::spawn(async {}); }
            launch
              (|| {});
        }
        #[cfg(test)] mod tests { fn hidden() { tokio::spawn(async {}); } }
        #[cfg(test)] const HIDDEN: () = { tokio::spawn(async {}); };
    "###,
    )
    .unwrap();
    assert_eq!(sites.len(), 1, "{sites:#?}");
    assert_eq!(sites[0].primitive, "tokio::task::spawn_blocking");
    assert_eq!(sites[0].item, "f");

    let fixture = tempfile::tempdir().unwrap();
    fs::write(
        fixture.path().join("lib.rs"),
        "#[cfg(test)] mod helper; mod live;",
    )
    .unwrap();
    fs::write(
        fixture.path().join("helper.rs"),
        "#[path = \"child.rs\"] mod child;",
    )
    .unwrap();
    fs::write(
        fixture.path().join("child.rs"),
        "fn f() { tokio::spawn(async {}); }",
    )
    .unwrap();
    fs::write(
        fixture.path().join("live.rs"),
        "fn f() { tokio::spawn(async {}); }",
    )
    .unwrap();
    let files = rust_sources(fixture.path()).unwrap();
    let tests = test_only_files(fixture.path(), &files).unwrap();
    assert!(tests.contains(&PathBuf::from("helper.rs")));
    assert!(tests.contains(&PathBuf::from("child.rs")));
    assert!(!tests.contains(&PathBuf::from("live.rs")));

    let entry = Entry {
        file: "fixture.rs",
        item: "f",
        primitive: "tokio::task::spawn_blocking",
        fingerprint: sites[0].fingerprint,
        occurrence: 1,
        category: Category::Read,
        reason: "fixture",
    };
    assert!(entry_errors(&[&entry], &sites).is_empty());
    assert!(!entry_errors(&[&entry, &entry], &sites).is_empty());
    assert!(!entry_errors(&[&entry], &[]).is_empty());
    let extra = scan("fixture.rs", "fn f() { tokio::spawn(async {}); }").unwrap();
    let mut repeated = sites[0].clone();
    repeated.occurrence = 2;
    assert_eq!(unclassified(&extra, std::slice::from_ref(&entry)).len(), 1);
    assert_eq!(
        unclassified(&[repeated], std::slice::from_ref(&entry)).len(),
        1
    );

    let aliases = scan(
        "aliases.rs",
        r#"
        use std::thread as worker;
        fn g() { worker::spawn(|| {}); let _f = tokio::spawn; }
    "#,
    )
    .unwrap();
    assert_eq!(aliases.len(), 2, "{aliases:#?}");
    assert!(aliases
        .iter()
        .any(|site| site.primitive == "std::thread::spawn"));
    assert!(aliases
        .iter()
        .any(|site| site.primitive == "spawn-reference"));

    let later_import = scan(
        "later.rs",
        r#"
        mod nested {
            fn g() { launch(|| {}); }
            use tokio::task::spawn_blocking as launch;
        }
    "#,
    )
    .unwrap();
    assert_eq!(later_import.len(), 1, "{later_import:#?}");
    assert_eq!(later_import[0].primitive, "tokio::task::spawn_blocking");
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
