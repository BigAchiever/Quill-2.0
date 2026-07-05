// Identify the frontmost application and decide whether quill should act there.
// Used to keep quill out of security-sensitive / dangerous surfaces.

use objc::runtime::Object;
use objc::{class, msg_send, sel, sel_impl};

/// Bundle-id prefixes where quill must NEVER act: terminals (typed text could run),
/// password managers / password dialogs, and quill itself.
const EXCLUDED_PREFIXES: &[&str] = &[
    "com.danishalisiddiqui.quill", // ourselves
    "com.apple.Terminal",
    "com.googlecode.iterm2",
    "dev.warp",
    "com.github.wez.wezterm",
    "net.kovidgoyal.kitty",
    "io.alacritty",
    "com.1password",
    "com.agilebits",           // 1Password 7
    "com.apple.keychainaccess",
    "com.apple.SecurityAgent", // system password dialogs
    "com.apple.loginwindow",   // the lock screen (was captured + cognified — pure noise)
];

/// Bundle identifier of the frontmost application (the app the user is typing in).
pub fn frontmost_bundle_id() -> Option<String> {
    unsafe {
        // frontmostApplication / bundleIdentifier return autoreleased objects; without
        // an autorelease pool on this (non-main) thread they just leak. Drain after.
        let pool: *mut Object = msg_send![class!(NSAutoreleasePool), new];
        let result = bundle_id_inner();
        let _: () = msg_send![pool, drain];
        result
    }
}

unsafe fn bundle_id_inner() -> Option<String> {
    let workspace: *mut Object = msg_send![class!(NSWorkspace), sharedWorkspace];
    if workspace.is_null() {
        return None;
    }
    let app: *mut Object = msg_send![workspace, frontmostApplication];
    if app.is_null() {
        return None;
    }
    let bid: *mut Object = msg_send![app, bundleIdentifier];
    if bid.is_null() {
        return None;
    }
    let utf8: *const std::os::raw::c_char = msg_send![bid, UTF8String];
    if utf8.is_null() {
        return None;
    }
    // into_owned() copies to the heap BEFORE the pool drains, so it stays valid.
    Some(std::ffi::CStr::from_ptr(utf8).to_string_lossy().into_owned())
}

/// PID of the frontmost application — needed for app-level AX queries (e.g. forcing an Electron
/// app to expose its accessibility tree). None if it can't be determined.
pub fn frontmost_pid() -> Option<i32> {
    unsafe {
        let pool: *mut Object = msg_send![class!(NSAutoreleasePool), new];
        let result = pid_inner();
        let _: () = msg_send![pool, drain];
        result
    }
}

unsafe fn pid_inner() -> Option<i32> {
    let workspace: *mut Object = msg_send![class!(NSWorkspace), sharedWorkspace];
    if workspace.is_null() {
        return None;
    }
    let app: *mut Object = msg_send![workspace, frontmostApplication];
    if app.is_null() {
        return None;
    }
    let pid: i32 = msg_send![app, processIdentifier];
    if pid > 0 {
        Some(pid)
    } else {
        None
    }
}

/// Should quill skip acting in this app? True for built-in defaults OR a
/// user-added exclusion (Phase 2.3). Gates both capture and the Option loop.
pub fn is_excluded(bundle_id: &str) -> bool {
    EXCLUDED_PREFIXES.iter().any(|p| bundle_id.starts_with(p))
        || crate::settings::is_user_excluded(bundle_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn excludes_self_terminals_and_password_managers() {
        assert!(is_excluded("com.danishalisiddiqui.quill"));
        assert!(is_excluded("com.apple.Terminal"));
        assert!(is_excluded("com.googlecode.iterm2"));
        // prefix match: real 1Password bundle ids extend the prefix
        assert!(is_excluded("com.1password.1password"));
        assert!(is_excluded("com.apple.SecurityAgent"));
    }

    #[test]
    fn allows_normal_apps() {
        assert!(!is_excluded("com.tinyspeck.slackmacgap"));
        assert!(!is_excluded("com.hnc.Discord"));
        assert!(!is_excluded("com.apple.mail"));
        // empty string is "not excluded" by this fn; callers gate empty/None separately.
        assert!(!is_excluded(""));
    }
}
