// Clipboard-free text insertion via synthesized keyboard events (CGEvent unicode strings).
// Nothing ever touches the system clipboard. Newlines are sent as Shift+Return so chat apps
// (Discord/WhatsApp/Slack) insert a line break instead of SENDING the half-written message.

use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::thread;
use std::time::Duration;

use core_graphics::event::{CGEvent, CGEventFlags, CGEventTapLocation};
use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};

const KEY_A: u16 = 0x00; // kVK_ANSI_A
const KEY_RETURN: u16 = 0x24; // kVK_Return
const KEY_DELETE: u16 = 0x33; // kVK_Delete (backspace)

/// True while quill is actively typing into a field — the capture loop skips these
/// moments so it doesn't record our own loader / output back as "memory."
static INJECTING: AtomicBool = AtomicBool::new(false);

/// The PID of the app that owned the field when the trigger fired (0 = no guard active).
/// The "PID mismatch … abort" hazard: the user switched tabs mid-generation
/// and the loader dots + output typed into the WRONG app (Claude). Every injection primitive
/// checks this before posting keystrokes — if focus moved, the keystroke is silently dropped.
static TARGET_PID: AtomicI32 = AtomicI32::new(0);

pub fn set_injecting(b: bool) {
    INJECTING.store(b, Ordering::SeqCst);
}

pub fn is_injecting() -> bool {
    INJECTING.load(Ordering::SeqCst)
}

/// Arm the focus guard for the app that owns the trigger's field (resets the lost-latch).
pub fn set_target_pid(pid: i32) {
    SESSION_LOST.store(false, Ordering::SeqCst);
    TARGET_PID.store(pid, Ordering::SeqCst);
}

/// Disarm the focus guard (end of an injection session).
pub fn clear_target_pid() {
    TARGET_PID.store(0, Ordering::SeqCst);
}

/// Latch: once focus leaves the target app during an injection session, the WHOLE session stays
/// suppressed — even if the user returns before generation finishes. Otherwise the loader
/// animation resumes typing orphan dots into the field the user just came back to (observed
/// live: a field containing only "..."). Cleared when the guard re-arms for the next trigger.
static SESSION_LOST: AtomicBool = AtomicBool::new(false);

/// May we type right now? True when no guard is armed, OR the target app is still frontmost
/// AND focus never left during this session.
fn focus_ok() -> bool {
    let target = TARGET_PID.load(Ordering::SeqCst);
    if target == 0 {
        return true;
    }
    if SESSION_LOST.load(Ordering::SeqCst) {
        return false; // latched: this session already lost focus once
    }
    match crate::app::frontmost_pid() {
        Some(pid) if pid == target => true,
        _ => {
            if !SESSION_LOST.swap(true, Ordering::SeqCst) {
                println!("[quill] focus left the target app — this generation is dropped (press ⌥ there again)");
            }
            false
        }
    }
}

fn source() -> Option<CGEventSource> {
    CGEventSource::new(CGEventSourceStateID::CombinedSessionState).ok()
}

/// Post a key (down + up) with the given modifier flags.
fn post_key(keycode: u16, flags: CGEventFlags) {
    let Some(src) = source() else {
        return;
    };
    if let Ok(e) = CGEvent::new_keyboard_event(src.clone(), keycode, true) {
        e.set_flags(flags);
        e.post(CGEventTapLocation::HID);
    }
    if let Ok(e) = CGEvent::new_keyboard_event(src, keycode, false) {
        e.set_flags(flags);
        e.post(CGEventTapLocation::HID);
    }
}

/// Insert a single line of text via a unicode keyboard event (no clipboard).
fn type_line(s: &str) {
    if s.is_empty() {
        return;
    }
    let Some(src) = source() else {
        return;
    };
    // Clear any inherited modifier (e.g. ⌘ left over from select-all) so the text
    // doesn't come out as ⌘-key shortcuts.
    if let Ok(e) = CGEvent::new_keyboard_event(src.clone(), 0, true) {
        e.set_flags(CGEventFlags::empty());
        e.set_string(s);
        e.post(CGEventTapLocation::HID);
    }
    if let Ok(e) = CGEvent::new_keyboard_event(src, 0, false) {
        e.set_flags(CGEventFlags::empty());
        e.set_string(s);
        e.post(CGEventTapLocation::HID);
    }
}

/// Type `text` as if the user typed it. Newlines → Shift+Return (line break, not send).
/// No-op if the user has switched away from the trigger's app (focus guard).
pub fn type_text(text: &str) {
    if !focus_ok() {
        return;
    }
    let mut first = true;
    for line in text.split('\n') {
        if !first {
            post_key(KEY_RETURN, CGEventFlags::CGEventFlagShift);
            thread::sleep(Duration::from_millis(6));
        }
        first = false;
        type_line(line);
        thread::sleep(Duration::from_millis(6));
    }
}

/// Select the whole field (Cmd+A).
fn select_all() {
    post_key(KEY_A, CGEventFlags::CGEventFlagCommand);
    thread::sleep(Duration::from_millis(25));
}

/// Replace the whole field: select all, then type over it — or CLEAR it if empty
/// (typing "" wouldn't remove the selected loader text; a Backspace does).
/// No-op if the user has switched away from the trigger's app (focus guard) — a select-all
/// in the WRONG app would clobber that app's field.
pub fn replace_all(text: &str) {
    if !focus_ok() {
        return;
    }
    select_all();
    if text.is_empty() {
        backspace(1); // delete the selection so the field ends empty
    } else {
        type_text(text);
    }
}

/// Press Backspace `n` times (used to animate the loader dots).
/// No-op if the user has switched away from the trigger's app (focus guard).
pub fn backspace(n: usize) {
    if !focus_ok() {
        return;
    }
    for _ in 0..n {
        post_key(KEY_DELETE, CGEventFlags::empty());
        thread::sleep(Duration::from_millis(8));
    }
}
