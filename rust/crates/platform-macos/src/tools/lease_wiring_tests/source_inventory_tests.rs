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
    check_reviewed_glob_targets(&root);
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

// A previously reviewed glob must not silently gain a worker re-export when
// its target changes. The AX bindings and macro-generated contract Role have
// a pinned source fingerprint; the remaining reviewed globs target enums.
fn check_reviewed_glob_targets(root: &Path) {
    for (path, expected) in [
        (root.join("ax/bindings.rs"), 14739359237517523625_u64),
        (
            root.join("../../cua-driver-contract/src/vocabulary.rs"),
            7383688377390068226_u64,
        ),
    ] {
        let contents =
            fs::read(&path).unwrap_or_else(|error| panic!("{}: {error}", path.display()));
        let actual = contents.iter().fold(0xcbf29ce484222325_u64, |hash, byte| {
            (hash ^ u64::from(*byte)).wrapping_mul(0x100000001b3)
        });
        assert_eq!(
            actual,
            expected,
            "glob target changed; re-audit its exports: {}",
            path.display()
        );
    }
    for (file, name) in [
        ("ax/vocabulary.rs", "Observation"),
        ("input/gesture.rs", "Preparation"),
        ("input/mouse.rs", "BackgroundDragEventKind"),
        ("tools/actionability.rs", "Route"),
    ] {
        let syntax = syn::parse_file(&fs::read_to_string(root.join(file)).unwrap()).unwrap();
        assert_eq!(
            syntax
                .items
                .iter()
                .filter(|item| matches!(item, syn::Item::Enum(item) if item.ident == name))
                .count(),
            1,
            "reviewed glob target must remain an enum: {file}::{name}"
        );
    }
}

#[test]
fn all_in_scope_mutating_spawns_propagate_worker_context() {
    let sites = inventory();
    let raw_imports: Vec<_> = sites.iter().filter(|site| site.raw_import).collect();
    println!(
        "production raw-primitive import sites: {} {raw_imports:#?}",
        raw_imports.len()
    );
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
    assert_eq!(sites.len(), 2, "{sites:#?}");
    assert!(sites.iter().any(|site| site.raw_import
        && site.item == "<file>"
        && site.primitive == "tokio::task::spawn_blocking"));
    let call = sites
        .iter()
        .find(|site| {
            !site.raw_import && site.item == "f" && site.primitive == "tokio::task::spawn_blocking"
        })
        .expect("raw aliased call");

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
        fingerprint: call.fingerprint,
        occurrence: 1,
        category: Category::Read,
        reason: "fixture",
    };
    assert!(entry_errors(&[&entry], &sites).is_empty());
    assert!(!entry_errors(&[&entry, &entry], &sites).is_empty());
    assert!(!entry_errors(&[&entry], &[]).is_empty());
    let extra = scan("fixture.rs", "fn f() { tokio::spawn(async {}); }").unwrap();
    let mut repeated = call.clone();
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
    assert_eq!(later_import.len(), 2, "{later_import:#?}");
    assert!(later_import.iter().any(|site| site.raw_import
        && site.item == "nested"
        && site.primitive == "tokio::task::spawn_blocking"));
    assert!(later_import.iter().any(|site| !site.raw_import
        && site.item == "nested::g"
        && site.primitive == "tokio::task::spawn_blocking"));

    let builders = scan(
        "builders.rs",
        r#"
        fn run() {
            std::thread::Builder::new().name("x".into())
                .spawn(crate::input::worker_context::wrap_blocking(|| {}));
            unknown.spawn(crate::input::worker_context::wrap_blocking(|| {}));
            std::thread::Builder::new().spawn(|| {});
        }
    "#,
    )
    .unwrap();
    assert_eq!(builders.len(), 2, "{builders:#?}");

    let collision = scan(
        "collision.rs",
        r#"
        mod a { use tokio::task::spawn_blocking as run; pub fn f() { run(native_mutation); } }
        mod b { use crate::goat::action_latency::spawn_blocking as run; }
    "#,
    )
    .unwrap();
    let shadow = scan(
        "shadow.rs",
        r#"
        use crate::goat::action_latency::spawn_blocking as run;
        fn f() { use tokio::task::spawn_blocking as run; run(native_mutation); }
    "#,
    )
    .unwrap();
    assert!(
        !collision.is_empty() && !shadow.is_empty(),
        "raw spawns hidden by alias collisions: modules={collision:#?}, local={shadow:#?}"
    );
    assert!(collision
        .iter()
        .any(|site| site.item == "a::f" && site.primitive == "tokio::task::spawn_blocking"));
    assert!(shadow
        .iter()
        .any(|site| site.item == "f" && site.primitive == "tokio::task::spawn_blocking"));

    let restored = scan(
        "scopes.rs",
        r#"
        use crate::goat::action_latency::spawn_blocking as run;
        fn f() {
            { use tokio::task::spawn_blocking as run; run(native_mutation); }
            run(safe_action);
        }
    "#,
    )
    .unwrap();
    assert_eq!(restored.len(), 3, "{restored:#?}");
    assert!(restored.iter().any(|site| site.raw_import
        && site.item == "<file>"
        && site.primitive == "tokio::task::spawn_blocking"));
    assert!(restored
        .iter()
        .any(|site| site.primitive == "tokio::task::spawn_blocking"));
    assert!(restored
        .iter()
        .any(|site| site.primitive == "crate::goat::action_latency::spawn_blocking"));

    let ambiguous = scan(
        "ambiguous.rs",
        r#"
        use tokio::task::spawn_blocking as run;
        use crate::goat::action_latency::spawn_blocking as run;
        fn f() { run(native_mutation); }
    "#,
    )
    .unwrap();
    assert!(
        ambiguous
            .iter()
            .any(|site| site.primitive.starts_with("ambiguous-alias:")),
        "{ambiguous:#?}"
    );

    let shadowed_binding = scan(
        "binding.rs",
        r#"
        use crate::goat::action_latency::spawn_blocking as run;
        fn f(run: fn()) { run(); }
        fn g() { let run = obtain(); run(); }
        fn h() { let _ = |run| run(); }
        fn i(value: Option<fn()>) { if let Some(run) = value { run(); } }
    "#,
    )
    .unwrap();
    assert!(
        shadowed_binding.iter().any(|site| site.item == "f"),
        "{shadowed_binding:#?}"
    );
    assert!(
        shadowed_binding.iter().any(|site| site.item == "g"),
        "{shadowed_binding:#?}"
    );
    assert!(
        shadowed_binding.iter().any(|site| site.item == "h"),
        "{shadowed_binding:#?}"
    );
    assert!(
        shadowed_binding.iter().any(|site| site.item == "i"),
        "{shadowed_binding:#?}"
    );

    let ambiguous_builder = scan("builder_alias.rs", r#"
        mod a {
            use other as worker;
            fn f() { worker::Builder::new().spawn(crate::input::worker_context::wrap_blocking(work)); }
        }
        mod b { use std::thread as worker; }
    "#).unwrap();
    assert!(
        !ambiguous_builder.is_empty(),
        "ambiguous builder was trusted"
    );

    let ambiguous_wrapper = scan(
        "wrapper_alias.rs",
        r#"
        use crate::input::worker_context::wrap_blocking as wrap;
        use other::wrap_blocking as wrap;
        fn f() { std::thread::Builder::new().spawn(wrap(work)); }
    "#,
    )
    .unwrap();
    assert!(
        ambiguous_wrapper
            .iter()
            .any(|site| site.primitive == ".spawn"),
        "{ambiguous_wrapper:#?}"
    );

    let glob = scan("glob.rs", "use hidden::*; use crate::goat::action_latency::spawn_blocking as run; fn f() { run(native_mutation); }").unwrap();
    assert!(!glob.is_empty(), "glob can hide a worker alias");
}

#[test]
fn raw_primitive_renames_and_reexports_are_inventory_sites() {
    let fixtures = [
        ("renamed module reexport", "mod workers { pub use tokio::spawn as run; } fn mutate() { workers::run(async { native_mutation() }); }", "workers", "tokio::spawn"),
        ("unrenamed module reexport", "mod workers { pub use tokio::task::spawn; } fn mutate() { workers::spawn(async { native_mutation() }); }", "workers", "tokio::task::spawn"),
        ("renamed Builder", "mod workers { use std::thread::Builder as Worker; }", "workers", "std::thread::Builder"),
        ("nested grouped use", "mod a { mod b { pub(crate) use tokio::{task::{spawn_blocking as run}}; } }", "a::b", "tokio::task::spawn_blocking"),
        ("unknown prefix alias", "mod workers { use tokio as rt; pub use rt::spawn as run; }", "workers", "rt::spawn"),
        ("function local import", "mod workers { fn f() { use tokio::spawn as run; } }", "workers", "tokio::spawn"),
    ];
    let mut missing = Vec::new();
    for (label, source, module, path) in fixtures {
        let sites = scan("fixture.rs", source).unwrap();
        if !sites
            .iter()
            .any(|site| site.raw_import && site.item == module && site.primitive == path)
        {
            missing.push(format!("{label}: expected {module} {path}; got {sites:#?}"));
        }
    }
    for path in [
        "tokio::spawn",
        "tokio::task::spawn",
        "tokio::task::spawn_blocking",
        "tokio::task::spawn_local",
        "std::thread::spawn",
        "std::thread::Builder",
        "std::thread::scope",
    ] {
        for declaration in [
            format!("use {path} as renamed;"),
            format!("pub use {path};"),
        ] {
            let sites = scan("matrix.rs", &format!("mod workers {{ {declaration} }}")).unwrap();
            if !sites
                .iter()
                .any(|site| site.raw_import && site.item == "workers" && site.primitive == path)
            {
                missing.push(format!("{declaration}: {sites:#?}"));
            }
        }
    }
    for path in ["tokio", "tokio::task", "std::thread"] {
        let sites = scan("glob.rs", &format!("mod workers {{ use {path}::*; }}")).unwrap();
        if !sites.iter().any(|site| {
            site.raw_import && site.item == "workers" && site.primitive == format!("{path}::*")
        }) {
            missing.push(format!("{path}::*: {sites:#?}"));
        }
    }
    assert!(
        missing.is_empty(),
        "raw primitive import sites missing:\n{}",
        missing.join("\n")
    );
    for path in [
        "tokio::spawn",
        "tokio::task::spawn",
        "tokio::task::spawn_blocking",
        "tokio::task::spawn_local",
        "std::thread::spawn",
        "std::thread::Builder",
        "std::thread::scope",
    ] {
        let original = path.rsplit("::").next().unwrap();
        for declaration in [format!("use {path};"), format!("use {path} as {original};")] {
            let sites = scan("private.rs", &format!("mod workers {{ {declaration} }}")).unwrap();
            assert!(
                !sites.iter().any(|site| site.raw_import),
                "unrenamed private import changed handling: {declaration}: {sites:#?}"
            );
        }
    }
    let safe = scan(
        "safe.rs",
        "pub use crate::goat::action_latency::spawn_blocking as run;",
    )
    .unwrap();
    assert!(
        !safe.iter().any(|site| site.raw_import),
        "trusted helper is not a raw primitive: {safe:#?}"
    );
    let test_only = scan("test_only.rs", "#[cfg(test)] pub use tokio::spawn as run;").unwrap();
    assert!(
        test_only.is_empty(),
        "test-only import was scanned: {test_only:#?}"
    );
    let duplicates = scan(
        "duplicate.rs",
        "mod workers { use tokio::{spawn as a, spawn as b}; }",
    )
    .unwrap();
    let occurrences: Vec<_> = duplicates
        .iter()
        .filter(|site| site.raw_import && site.primitive == "tokio::spawn")
        .map(|site| site.occurrence)
        .collect();
    assert_eq!(occurrences, [1, 2]);
}

#[test]
fn trait_method_parameter_shadow_rejects_wrapped_builder() {
    let sites = scan(
        "trait_shadow.rs",
        r#"
        use crate::input::worker_context::wrap_blocking as wrap;
        trait Work {
            fn run(&self, wrap: impl FnOnce()) {
                std::thread::Builder::new().spawn(wrap(work));
            }
        }
    "#,
    )
    .unwrap();
    assert!(
        sites.iter().any(|site| site.primitive == ".spawn"),
        "trait parameter hid raw Builder spawn: {sites:#?}"
    );
    assert!(
        sites.iter().any(|site| site.item == "Work::run"),
        "trait method name missing: {sites:#?}"
    );
}

#[test]
fn body_binding_sweep_rejects_shadowed_worker_helpers() {
    let imported = "use crate::input::worker_context::wrap_blocking as wrap;";
    let cases = [
        ("trait const", "trait T { const C: () = { let wrap = identity; std::thread::Builder::new().spawn(wrap(work)); }; }"),
        ("impl const", "impl T { const C: () = { let wrap = identity; std::thread::Builder::new().spawn(wrap(work)); }; }"),
        ("nested fn item", "fn f() { fn wrap(x: T) -> T { x } std::thread::Builder::new().spawn(wrap(work)); }"),
        ("nested mod item", "fn f() { let wrap = identity; mod m { use crate::input::worker_context::wrap_blocking as wrap; fn g() { std::thread::Builder::new().spawn(wrap(work)); } } }"),
        ("nested trait item", "fn f() { let wrap = identity; trait T { const C: () = { std::thread::Builder::new().spawn(wrap(work)); }; } }"),
        ("nested impl item", "fn f() { let wrap = identity; impl T { const C: () = { std::thread::Builder::new().spawn(wrap(work)); }; } }"),
        ("closure parameter", "fn f() { let _ = |wrap| { std::thread::Builder::new().spawn(wrap(work)); }; }"),
        ("async closure parameter", "fn f() { let _ = async |wrap| { std::thread::Builder::new().spawn(wrap(work)); }; }"),
        ("async block", "fn f() { let _ = async { let wrap = identity; std::thread::Builder::new().spawn(wrap(work)); }; }"),
        ("const block", "fn f() { let _ = const { let wrap = identity; std::thread::Builder::new().spawn(wrap(work)); }; }"),
        ("let else", "fn f() { let Some(wrap) = value else { return; }; std::thread::Builder::new().spawn(wrap(work)); }"),
        ("match arm", "fn f() { match value { Some(wrap) => { std::thread::Builder::new().spawn(wrap(work)); }, _ => {} } }"),
        ("if let", "fn f() { if let Some(wrap) = value { std::thread::Builder::new().spawn(wrap(work)); } }"),
        ("while let", "fn f() { while let Some(wrap) = next() { std::thread::Builder::new().spawn(wrap(work)); } }"),
        ("for pattern", "fn f() { for wrap in values { std::thread::Builder::new().spawn(wrap(work)); } }"),
        ("function parameter", "fn f(wrap: F) { std::thread::Builder::new().spawn(wrap(work)); }"),
        ("generic parameter", "fn f<const wrap: usize>() { std::thread::Builder::new().spawn(wrap(work)); }"),
        ("trait generic parameter", "trait T<const wrap: usize> { fn f() { std::thread::Builder::new().spawn(wrap(work)); } }"),
        ("impl generic parameter", "impl<const wrap: usize> T { const C: () = { std::thread::Builder::new().spawn(wrap(work)); }; }"),
        ("foreign item", "fn f() { unsafe extern \"C\" { fn wrap(); } std::thread::Builder::new().spawn(wrap(work)); }"),
        ("extern crate item", "fn f() { extern crate other as wrap; std::thread::Builder::new().spawn(wrap(work)); }"),
        ("trait alias item", "fn f() { trait wrap = Bound; std::thread::Builder::new().spawn(wrap(work)); }"),
        ("other cfg", "#[cfg(feature = \"x\")] trait T { fn f(&self, wrap: F) { std::thread::Builder::new().spawn(wrap(work)); } }"),
        ("cfg trait method", "trait T { #[cfg(not(test))] fn f(wrap: F) { std::thread::Builder::new().spawn(wrap(work)); } }"),
        ("cfg trait const", "trait T { #[cfg(feature = \"x\")] const C: () = { let wrap = identity; std::thread::Builder::new().spawn(wrap(work)); }; }"),
        ("cfg impl const", "impl T { #[cfg(feature = \"x\")] const C: () = { let wrap = identity; std::thread::Builder::new().spawn(wrap(work)); }; }"),
        ("elsewhere in enclosing fn", "fn f() { { let wrap = identity; } std::thread::Builder::new().spawn(wrap(work)); }"),
        ("import elsewhere in enclosing fn", "fn f() { { use other::identity as wrap; } std::thread::Builder::new().spawn(wrap(work)); }"),
    ];
    for (label, body) in cases {
        let sites = scan("sweep.rs", &format!("{imported} {body}")).unwrap();
        assert!(
            sites.iter().any(|site| site.primitive == ".spawn"),
            "{label} omitted its spawn: {sites:#?}"
        );
        if label == "trait const" || label == "impl const" {
            assert!(
                sites.iter().any(|site| site.item == "T::C"),
                "{label} missing associated item name: {sites:#?}"
            );
        }
    }

    let call = scan(
        "call.rs",
        r#"
        use crate::goat::action_latency::spawn_blocking as run;
        fn f() { { let run = identity; } run(work); }
    "#,
    )
    .unwrap();
    assert!(
        !call.is_empty(),
        "binding elsewhere in fn hid helper-call shadow: {call:#?}"
    );

    let macros = scan(
        "macro.rs",
        "macro_rules! m { () => { let wrap = identity; std::thread::spawn(wrap(work)) }; } m! { let wrap = identity; tokio::spawn(wrap(work)) }",
    )
    .unwrap();
    assert_eq!(
        macros
            .iter()
            .filter(|site| site.primitive == "macro-spawn")
            .count(),
        2,
        "{macros:#?}"
    );
    let alias_macro = scan("macro_alias.rs", "use tokio::task::spawn_blocking as run; macro_rules! m { () => { run(work) }; } m! { run(work) }").unwrap();
    assert_eq!(
        alias_macro
            .iter()
            .filter(|site| site.primitive == "macro-spawn")
            .count(),
        2,
        "{alias_macro:#?}"
    );
    let nested_alias_macro = scan("nested_macro.rs", "fn f() { { use tokio::task::spawn_blocking as run; } macro_rules! m { () => { run(work) }; } }").unwrap();
    assert!(
        nested_alias_macro
            .iter()
            .any(|site| site.primitive == "macro-spawn"),
        "nested macro alias omitted: {nested_alias_macro:#?}"
    );
    let tests_only = scan("test_only.rs", &format!("{imported} #[cfg(test)] trait T {{ fn f(wrap: F) {{ std::thread::Builder::new().spawn(wrap(work)); }} }}")).unwrap();
    assert!(
        tests_only.is_empty(),
        "test-only trait was scanned: {tests_only:#?}"
    );
    let test_method = scan("test_method.rs", &format!("{imported} trait T {{ #[cfg(test)] fn f(wrap: F) {{ std::thread::Builder::new().spawn(wrap(work)); }} }}")).unwrap();
    assert!(
        test_method.is_empty(),
        "test-only trait method was scanned: {test_method:#?}"
    );
    let test_consts = scan("test_consts.rs", &format!("{imported} trait T {{ #[cfg(test)] const C: () = {{ let wrap = identity; std::thread::Builder::new().spawn(wrap(work)); }}; }} impl T {{ #[cfg(test)] const C: () = {{ let wrap = identity; std::thread::Builder::new().spawn(wrap(work)); }}; }}")).unwrap();
    assert!(
        test_consts.is_empty(),
        "test-only associated const was scanned: {test_consts:#?}"
    );
    let test_macro = scan(
        "test_macro.rs",
        "fn f() { #[cfg(test)] fake! { tokio::spawn(work) }; }",
    )
    .unwrap();
    assert!(
        test_macro.is_empty(),
        "test-only statement macro was scanned: {test_macro:#?}"
    );
    let foreign_macro = scan(
        "foreign_macro.rs",
        "unsafe extern \"C\" { fake! { std::thread::spawn(work) } }",
    )
    .unwrap();
    assert!(
        foreign_macro
            .iter()
            .any(|site| site.primitive == "macro-spawn"),
        "foreign macro missing: {foreign_macro:#?}"
    );
    let safe_local = scan("safe_local.rs", "fn f() { use crate::input::worker_context::wrap_blocking as wrap; std::thread::Builder::new().spawn(wrap(work)); }").unwrap();
    assert!(
        safe_local.is_empty(),
        "unique helper alias was rejected: {safe_local:#?}"
    );
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
