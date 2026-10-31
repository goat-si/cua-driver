//! Exact executable identities for processes whose forced termination can end a desktop session.

#[derive(Clone, Copy)]
pub(crate) enum TargetOs {
    Macos,
    Windows,
    Linux,
}

pub(crate) fn build_target_os() -> Option<TargetOs> {
    if cfg!(target_os = "macos") {
        Some(TargetOs::Macos)
    } else if cfg!(target_os = "windows") {
        Some(TargetOs::Windows)
    } else if cfg!(target_os = "linux") {
        Some(TargetOs::Linux)
    } else {
        None
    }
}

const MACOS: &[&str] = &[
    "/System/Library/CoreServices/Finder.app/Contents/MacOS/Finder",
    "/System/Library/CoreServices/Dock.app/Contents/MacOS/Dock",
    "/System/Library/CoreServices/SystemUIServer.app/Contents/MacOS/SystemUIServer",
    "/System/Library/CoreServices/loginwindow.app/Contents/MacOS/loginwindow",
    "/System/Library/CoreServices/ControlCenter.app/Contents/MacOS/ControlCenter",
    "/System/Library/PrivateFrameworks/SkyLight.framework/Resources/WindowServer",
    "/System/Library/CoreServices/WindowManager.app/Contents/MacOS/WindowManager",
];
const WINDOWS: &[&str] = &[
    "explorer.exe",
    "csrss.exe",
    "winlogon.exe",
    "dwm.exe",
    "lsass.exe",
    "services.exe",
    "smss.exe",
    "wininit.exe",
    "sihost.exe",
    "fontdrvhost.exe",
    "logonui.exe",
];
const LINUX: &[&str] = &[
    "gnome-shell",
    "kwin_wayland",
    "kwin_x11",
    "Xorg",
    "Xwayland",
    "mutter",
    "plasmashell",
    "sway",
    "Hyprland",
    "weston",
    "gnome-session-binary",
    "systemd",
    "i3",
    "xfwm4",
    "openbox",
    "xfce4-session",
    "cinnamon",
    "muffin",
    "labwc",
    "river",
    "niri",
    "wayfire",
    "ksmserver",
    "lxsession",
    "budgie-wm",
];

pub(crate) fn is_protected_executable(path: &str, os: TargetOs) -> bool {
    match os {
        TargetOs::Macos => MACOS.contains(&path),
        TargetOs::Windows => {
            let basename = path.rsplit(['/', '\\']).next().unwrap_or(path);
            WINDOWS
                .iter()
                .any(|name| basename.eq_ignore_ascii_case(name))
        }
        TargetOs::Linux => {
            let path = path.strip_suffix(" (deleted)").unwrap_or(path);
            LINUX.contains(&path.rsplit('/').next().unwrap_or(path))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn macos_exact_paths_and_negatives() {
        for path in MACOS {
            assert!(is_protected_executable(path, TargetOs::Macos));
        }
        for path in [
            "Finder",
            "/tmp/Finder",
            "/System/Library/CoreServices/Finder.app/Contents/MacOS/MyFinder",
            "/system/Library/CoreServices/Finder.app/Contents/MacOS/Finder",
        ] {
            assert!(!is_protected_executable(path, TargetOs::Macos), "{path}");
        }
    }
    #[test]
    fn windows_exact_basenames_and_negatives() {
        for name in WINDOWS {
            assert!(is_protected_executable(
                &format!(r"C:\\Windows\\{}", name.to_ascii_uppercase()),
                TargetOs::Windows
            ));
        }
        for path in [
            "explorer2.exe",
            "myexplorer.exe",
            "explorer.exe.bak",
            "explorer.exe ",
        ] {
            assert!(!is_protected_executable(path, TargetOs::Windows), "{path}");
        }
    }
    #[test]
    fn linux_exact_basenames_and_negatives() {
        for name in LINUX {
            assert!(is_protected_executable(
                &format!("/usr/bin/{name}"),
                TargetOs::Linux
            ));
        }
        for path in [
            "/usr/bin/i3bar",
            "/usr/bin/xfwm4-helper",
            "/usr/bin/GNOME-shell",
            "/usr/bin/sway (deleted) (deleted)",
            "/usr/bin/sway (deleted) ",
        ] {
            assert!(!is_protected_executable(path, TargetOs::Linux), "{path}");
        }
    }
    #[test]
    fn deleted_linux_executable_is_still_protected() {
        assert!(is_protected_executable(
            "/usr/bin/gnome-shell (deleted)",
            TargetOs::Linux
        ));
        assert!(!is_protected_executable(
            "/usr/bin/gnome-shell-helper (deleted)",
            TargetOs::Linux
        ));
    }
    #[test]
    fn i3_and_xfwm4_are_protected() {
        assert!(is_protected_executable("/usr/bin/i3", TargetOs::Linux));
        assert!(is_protected_executable("/usr/bin/xfwm4", TargetOs::Linux));
    }
}
