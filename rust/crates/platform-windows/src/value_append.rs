#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AppendOutcome {
    Written { before: String, inserted: String },
    ReplacedSecure,
    Refused(&'static str),
    NoValuePattern,
    SecureWriteFailed { hresult: i32 },
    WriteFailed { hresult: i32 },
}

pub(crate) fn run_value_append<V>(
    read_is_password: impl FnOnce() -> Option<bool>,
    acquire_value_pattern: impl FnOnce() -> Option<V>,
    read_value: impl FnOnce(&V) -> Result<String, i32>,
    set_value: impl FnOnce(&V, &str) -> Result<(), i32>,
    text: &str,
) -> AppendOutcome {
    let password = read_is_password();
    let pattern = acquire_value_pattern();
    match (password, pattern) {
        (None, _) => AppendOutcome::Refused("secure_state_unknown"),
        (Some(true), None) => AppendOutcome::Refused("secure_no_value_pattern"),
        (Some(true), Some(pattern)) => match set_value(&pattern, text) {
            Ok(()) => AppendOutcome::ReplacedSecure,
            Err(hresult) => AppendOutcome::SecureWriteFailed { hresult },
        },
        (Some(false), None) => AppendOutcome::NoValuePattern,
        (Some(false), Some(pattern)) => {
            let Ok(before) = read_value(&pattern) else {
                return AppendOutcome::Refused("value_unreadable");
            };
            let inserted = format!("{before}{text}");
            match set_value(&pattern, &inserted) {
                Ok(()) => AppendOutcome::Written { before, inserted },
                Err(hresult) => AppendOutcome::WriteFailed { hresult },
            }
        }
    }
}

pub(crate) fn allows_wm_char_fallback(outcome: &AppendOutcome) -> bool {
    matches!(
        outcome,
        AppendOutcome::NoValuePattern | AppendOutcome::WriteFailed { .. }
    )
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Route {
    Continue(AppendOutcome),
    Terminal,
    WmChar,
}
pub(crate) struct WorkerFailed;
pub(crate) fn route_after_worker(result: Result<Option<AppendOutcome>, WorkerFailed>) -> Route {
    match result {
        Ok(Some(outcome)) if allows_wm_char_fallback(&outcome) => Route::WmChar,
        Ok(Some(outcome)) => Route::Continue(outcome),
        // None: the admitted element is not a UIA element, so nothing was
        // written and the typing route is the only one it ever had.
        Ok(None) => Route::WmChar,
        Err(_) => Route::Terminal,
    }
}
pub(crate) fn read_value_gated(
    read_is_password: impl FnOnce() -> Option<bool>,
    read_value: impl FnOnce() -> Option<String>,
    read_text: impl FnOnce() -> Option<String>,
) -> Option<String> {
    if read_is_password() != Some(false) {
        return None;
    }
    read_value().or_else(read_text)
}
pub(crate) fn focused_document_target_ok(
    pid_matches: bool,
    is_document: bool,
    enabled: bool,
    has_focus: bool,
    is_password: Option<bool>,
    hwnd_ok: bool,
) -> bool {
    pid_matches && is_document && enabled && has_focus && is_password == Some(false) && hwnd_ok
}
#[cfg(test)]
#[path = "value_append_tests.rs"]
mod tests;
