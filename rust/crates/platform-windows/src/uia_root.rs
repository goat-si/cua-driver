use std::sync::Mutex;

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum RootVerdict {
    Ok,
    WindowGone,
    NoAnswer,
}

pub(crate) fn root_verdict(is_window: bool, responds: bool) -> RootVerdict {
    match (is_window, responds) {
        (false, _) => RootVerdict::WindowGone,
        (true, false) => RootVerdict::NoAnswer,
        (true, true) => RootVerdict::Ok,
    }
}

pub(crate) fn probe_with(is_window: bool, responds: impl FnOnce() -> bool) -> RootVerdict {
    if !is_window {
        return root_verdict(false, false);
    }
    root_verdict(true, responds())
}

pub(crate) fn warm_up_with(
    flag: &Mutex<bool>,
    call: impl FnOnce() -> Result<(), i32>,
) -> Result<(), i32> {
    let mut warm = flag
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if *warm {
        return Ok(());
    }
    let result = call();
    *warm = result != Err(0x80004005u32 as i32);
    result
}

#[cfg(test)]
#[path = "uia_root_tests.rs"]
mod tests;
