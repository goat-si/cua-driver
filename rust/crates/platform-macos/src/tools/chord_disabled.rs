use crate::ax::bindings::AXError;

pub(super) fn lookup_native_menu(native: bool, _frontmost: bool) -> bool {
    native
}

pub(super) fn press_before_save(is_save: bool, press: impl FnOnce() -> AXError) -> Option<AXError> {
    if is_save {
        None
    } else {
        Some(press())
    }
}

pub(super) fn try_press(_enabled: Option<bool>, press: impl FnOnce() -> AXError) -> AXError {
    press()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frontmost_native_chord_looks_up_menu() {
        assert!(lookup_native_menu(true, true));
        assert!(lookup_native_menu(true, false));
        assert!(!lookup_native_menu(false, true));
    }

    #[test]
    fn save_goes_to_verified_route_without_pressing() {
        let called = std::cell::Cell::new(false);
        let result = press_before_save(true, || {
            called.set(true);
            0
        });
        assert_eq!(result, None);
        assert!(!called.get(), "Save was pressed before its verified route");
    }

    #[test]
    fn disabled_item_still_attempts_ax_press() {
        let calls = std::cell::Cell::new(0);
        let result = try_press(Some(false), || {
            calls.set(calls.get() + 1);
            42
        });
        assert_eq!(calls.get(), 1);
        assert_eq!(result, 42);
    }

    #[test]
    fn enabled_and_unreadable_items_are_also_pressed() {
        for enabled in [Some(true), None] {
            assert_eq!(try_press(enabled, || 42), 42);
        }
    }
}
