use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};

const E_FAIL: i32 = 0x80004005u32 as i32;

#[test]
fn uia_root_probe_short_circuits_and_classifies() {
    assert_eq!(root_verdict(false, true), RootVerdict::WindowGone);
    assert_eq!(root_verdict(false, false), RootVerdict::WindowGone);
    assert_eq!(root_verdict(true, false), RootVerdict::NoAnswer);
    assert_eq!(root_verdict(true, true), RootVerdict::Ok);
    let calls = AtomicUsize::new(0);
    assert_eq!(
        probe_with(false, || {
            calls.fetch_add(1, Ordering::SeqCst);
            true
        }),
        RootVerdict::WindowGone
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert_eq!(probe_with(true, || true), RootVerdict::Ok);
    assert_eq!(probe_with(true, || false), RootVerdict::NoAnswer);
}

#[test]
fn uia_root_warm_up_retries_only_e_fail() {
    let flag = Mutex::new(false);
    let calls = AtomicUsize::new(0);
    assert_eq!(
        warm_up_with(&flag, || {
            calls.fetch_add(1, Ordering::SeqCst);
            Err(E_FAIL)
        }),
        Err(E_FAIL)
    );
    assert!(!*flag.lock().unwrap());
    assert_eq!(
        warm_up_with(&flag, || {
            calls.fetch_add(1, Ordering::SeqCst);
            Err(42)
        }),
        Err(42)
    );
    assert!(*flag.lock().unwrap());
    assert_eq!(
        warm_up_with(&flag, || {
            calls.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }),
        Ok(())
    );
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    let flag = Mutex::new(false);
    assert_eq!(warm_up_with(&flag, || Ok(())), Ok(()));
    assert!(*flag.lock().unwrap());
}

#[test]
fn uia_root_warm_up_recovers_poison() {
    let flag = Mutex::new(false);
    std::thread::scope(|s| {
        let _ = s
            .spawn(|| {
                let _guard = flag.lock().unwrap();
                panic!("poison");
            })
            .join();
    });
    assert_eq!(warm_up_with(&flag, || Ok(())), Ok(()));
    assert!(*flag
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner));
}
