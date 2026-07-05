// Accessibility helpers: read / write the focused element's value & selection,
// and read the surrounding window's visible text (context). Requires Accessibility.

use std::ptr;
use std::sync::{Mutex, MutexGuard};

use accessibility_sys::{
    kAXChildrenAttribute, kAXDescriptionAttribute, kAXErrorAPIDisabled, kAXErrorSuccess,
    kAXFocusedUIElementAttribute, kAXNumberOfCharactersAttribute, kAXParentAttribute,
    kAXSelectedTextAttribute, kAXSelectedTextRangeAttribute, kAXStringForRangeParameterizedAttribute,
    kAXTitleAttribute, kAXTrustedCheckOptionPrompt, kAXValueAttribute, kAXValueTypeCFRange,
    AXIsProcessTrustedWithOptions, AXUIElementCopyAttributeValue,
    AXUIElementCopyParameterizedAttributeValue, AXUIElementCreateApplication,
    AXUIElementCreateSystemWide, AXUIElementRef, AXUIElementSetAttributeValue, AXValueCreate,
};
use core_foundation_sys::number::{
    kCFNumberSInt64Type, CFNumberGetTypeID, CFNumberGetValue, CFNumberRef,
};
use core_foundation_sys::base::CFRange;
use core_foundation::base::{CFRelease, CFTypeRef, TCFType};
use core_foundation::boolean::CFBoolean;
use core_foundation::dictionary::CFDictionary;
use core_foundation::string::{CFString, CFStringRef};
use core_foundation::url::{CFURL, CFURLRef};
use core_foundation_sys::array::{CFArrayGetCount, CFArrayGetValueAtIndex, CFArrayRef};
use core_foundation_sys::base::{CFGetTypeID, CFRetain};
use core_foundation_sys::string::CFStringGetTypeID;
use core_foundation_sys::url::CFURLGetTypeID;

/// Serializes ALL Accessibility reads: macOS AX isn't thread-safe, and capture + the trigger
/// + edit-watchers can call it concurrently. Recovers from poisoning (AX traversal can panic).
static AX_LOCK: Mutex<()> = Mutex::new(());
fn ax_guard() -> MutexGuard<'static, ()> {
    AX_LOCK.lock().unwrap_or_else(|p| p.into_inner())
}

/// Returns true if already trusted; otherwise pops the system Accessibility prompt.
pub fn ensure_accessibility_prompt() -> bool {
    unsafe {
        let prompt_key = CFString::wrap_under_get_rule(kAXTrustedCheckOptionPrompt);
        let options = CFDictionary::from_CFType_pairs(&[(
            prompt_key.as_CFType(),
            CFBoolean::true_value().as_CFType(),
        )]);
        AXIsProcessTrustedWithOptions(options.as_concrete_TypeRef())
    }
}

/// Silent trusted check (no system prompt) — the settings Permissions card polls this.
pub fn is_trusted() -> bool {
    unsafe {
        let prompt_key = CFString::wrap_under_get_rule(kAXTrustedCheckOptionPrompt);
        let options = CFDictionary::from_CFType_pairs(&[(
            prompt_key.as_CFType(),
            CFBoolean::false_value().as_CFType(),
        )]);
        AXIsProcessTrustedWithOptions(options.as_concrete_TypeRef())
    }
}

/// Open System Settings at the Accessibility pane so the user can (re-)grant permission.
/// macOS revokes this grant on every binary change (rebuild / app update), so re-granting is
/// routine — sending the user straight to the right pane beats a silent, permission-dead app.
pub fn open_accessibility_settings() {
    let _ = std::process::Command::new("open")
        .arg("x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility")
        .spawn();
}

/// Fetch the system-wide focused UI element. Caller must `CFRelease` it.
unsafe fn copy_focused_element() -> Result<AXUIElementRef, String> {
    // 1) System-wide focused element — works for native apps and many web views.
    let system_wide = AXUIElementCreateSystemWide();
    if system_wide.is_null() {
        return Err("no system-wide element".into());
    }
    let focused_attr = CFString::new(kAXFocusedUIElementAttribute);
    let mut focused: CFTypeRef = ptr::null();
    let err =
        AXUIElementCopyAttributeValue(system_wide, focused_attr.as_concrete_TypeRef(), &mut focused);
    CFRelease(system_wide as CFTypeRef);
    if err == kAXErrorAPIDisabled {
        return Err("Accessibility not granted — enable quill under System Settings → \
                    Privacy & Security → Accessibility"
            .into());
    }
    if err == kAXErrorSuccess && !focused.is_null() {
        return Ok(focused as AXUIElementRef);
    }

    // 2) Embedded-web fallback: Electron apps (Claude, Discord, Slack, VS Code) AND WebView2 apps
    //    (new Teams) don't expose a focused element until an AX client asks. Enable the per-runtime
    //    "expose your tree" toggle on the frontmost app, let it build, then read ITS focused element.
    if let Some(el) = embedded_web_focused_element() {
        return Ok(el);
    }

    Err(format!("no focused element (AXError {err})"))
}

/// Force an embedded-web app to expose its AX tree, then return its focused element (retained;
/// caller releases). Covers BOTH runtimes Quill meets: Electron/Chromium (Discord, Claude, Slack,
/// VS Code), which reveal their tree via `AXManualAccessibility`; AND Microsoft's WebView2 host used
/// by new Teams (`com.microsoft.teams2`, see `MSWebView2.framework`), which IGNORES that Chromium
/// hook and instead exposes its web tree only when it believes an assistive client is present — i.e.
/// `AXEnhancedUserInterface`, the attribute VoiceOver sets. We set BOTH, then poll while the tree
/// builds. None if it isn't an embedded-web app or the tree still isn't ready. Both toggles persist
/// for the app's lifetime, so the cost is paid at most once per app per launch.
unsafe fn embedded_web_focused_element() -> Option<AXUIElementRef> {
    let pid = crate::app::frontmost_pid()?;
    let app = AXUIElementCreateApplication(pid);
    if app.is_null() {
        return None;
    }
    // Chromium/Electron hook (Discord, Claude, Slack, VS Code).
    let manual = CFString::new("AXManualAccessibility");
    let _ = AXUIElementSetAttributeValue(
        app,
        manual.as_concrete_TypeRef(),
        CFBoolean::true_value().as_concrete_TypeRef() as CFTypeRef,
    );
    // AppKit/WebView2 hook — new Teams isn't Chromium, so it needs THIS one to publish its web tree.
    let enhanced = CFString::new("AXEnhancedUserInterface");
    let _ = AXUIElementSetAttributeValue(
        app,
        enhanced.as_concrete_TypeRef(),
        CFBoolean::true_value().as_concrete_TypeRef() as CFTypeRef,
    );
    // Chromium builds the tree asynchronously, so a single fixed wait races it: the FIRST trigger
    // after enabling manual-AX (or after the app rebuilds its tree) can miss with kAXErrorNoValue
    // (-25212). Read immediately — instant on repeat triggers where the tree is already up — then
    // poll briefly (≤~600ms) if it isn't ready yet, instead of betting on one 220ms guess.
    let focused_attr = CFString::new(kAXFocusedUIElementAttribute);
    for attempt in 0..6 {
        if attempt > 0 {
            let ms = if attempt == 1 { 200 } else { 100 };
            std::thread::sleep(std::time::Duration::from_millis(ms));
        }
        let mut focused: CFTypeRef = ptr::null();
        let err =
            AXUIElementCopyAttributeValue(app, focused_attr.as_concrete_TypeRef(), &mut focused);
        if err == kAXErrorSuccess && !focused.is_null() {
            CFRelease(app as CFTypeRef);
            return Some(focused as AXUIElementRef);
        }
    }
    CFRelease(app as CFTypeRef);
    None
}

/// Copy a string-valued attribute (None if missing or not a string).
unsafe fn copy_attr_string(el: AXUIElementRef, attr: &str) -> Option<String> {
    let a = CFString::new(attr);
    let mut value: CFTypeRef = ptr::null();
    let err = AXUIElementCopyAttributeValue(el, a.as_concrete_TypeRef(), &mut value);
    if err != kAXErrorSuccess || value.is_null() {
        return None;
    }
    if CFGetTypeID(value) != CFStringGetTypeID() {
        CFRelease(value);
        return None;
    }
    Some(CFString::wrap_under_create_rule(value as CFStringRef).to_string())
}

/// Copy an element-valued attribute. Caller must `CFRelease` the result.
unsafe fn copy_attr_element(el: AXUIElementRef, attr: &str) -> Option<AXUIElementRef> {
    let a = CFString::new(attr);
    let mut value: CFTypeRef = ptr::null();
    let err = AXUIElementCopyAttributeValue(el, a.as_concrete_TypeRef(), &mut value);
    if err != kAXErrorSuccess || value.is_null() {
        return None;
    }
    Some(value as AXUIElementRef)
}

/// Copy the children of an element (each retained; caller releases each).
unsafe fn copy_children(el: AXUIElementRef) -> Vec<AXUIElementRef> {
    let a = CFString::new(kAXChildrenAttribute);
    let mut value: CFTypeRef = ptr::null();
    let err = AXUIElementCopyAttributeValue(el, a.as_concrete_TypeRef(), &mut value);
    if err != kAXErrorSuccess || value.is_null() {
        return Vec::new();
    }
    let array = value as CFArrayRef;
    let count = CFArrayGetCount(array);
    let mut out = Vec::with_capacity(count.max(0) as usize);
    for i in 0..count {
        let child = CFArrayGetValueAtIndex(array, i) as AXUIElementRef;
        if !child.is_null() {
            CFRetain(child as CFTypeRef);
            out.push(child);
        }
    }
    CFRelease(value);
    out
}

/// Read the focused text field's current value — tolerant of apps that don't expose the text via
/// `AXValue` on the focused element itself. Catalyst apps (e.g. WhatsApp) return kAXErrorNoValue
/// (-25212) for AXValue and keep the real text on a nested text view; some UIKit fields surface
/// it only as AXSelectedText. Order: AXValue → AXSelectedText → first AXTextArea/AXTextField
/// descendant's value. Only genuine text-entry roles are read on the fallback path, so pressing ⌥
/// on a non-field still skips (no misfire). On a miss it logs the focused + child roles once so an
/// unrecognised field is diagnosable, then errors — the trigger degrades to skip, exactly as before.
pub fn read_focused_value() -> Result<String, String> {
    let _ax = ax_guard();
    unsafe {
        let el = copy_focused_element()?;
        let out = read_editable_text(el);
        CFRelease(el as CFTypeRef);
        out
    }
}

/// AXValue → AXSelectedText → nested text-entry descendant. See `read_focused_value`.
unsafe fn read_editable_text(el: AXUIElementRef) -> Result<String, String> {
    // 1) AXValue on the focused element — the common, fast path (empty string = an empty field).
    if let Some(s) = copy_attr_string(el, kAXValueAttribute) {
        return Ok(s);
    }
    // 2) AXSelectedText — some UIKit/Catalyst fields surface typed text here, not AXValue.
    if let Some(s) = copy_attr_string(el, kAXSelectedTextAttribute).filter(|s| !s.is_empty()) {
        println!("[quill] field read via AXSelectedText ({} chars)", s.chars().count());
        return Ok(s);
    }
    // 3) Parameterized text: Catalyst/UIKit AXTextAreas (WhatsApp) expose their content ONLY via
    //    the AXStringForRange parameterized attribute + AXNumberOfCharacters — never AXValue.
    if let Some(s) = read_string_for_range(el) {
        println!("[quill] field read via AXStringForRange ({} chars)", s.chars().count());
        return Ok(s);
    }
    // 4) Some apps nest the real text view under a container — descend to the first
    //    AXTextArea/AXTextField and read ITS value (empty = empty field).
    if let Some(s) = descend_for_textfield(el, 0) {
        println!("[quill] field read via nested text-field ({} chars)", s.chars().count());
        return Ok(s);
    }
    // Nothing text-like: log what the focused element exposes (once) so an unrecognised field
    // role is diagnosable, then bail — the caller degrades to skip exactly as before.
    let role = copy_attr_string(el, "AXRole").unwrap_or_default();
    let child_roles: Vec<String> = copy_children(el)
        .into_iter()
        .map(|c| {
            let r = copy_attr_string(c, "AXRole").unwrap_or_default();
            CFRelease(c as CFTypeRef);
            r
        })
        .collect();
    println!("[quill] field unreadable — focused role='{role}', child roles={child_roles:?}");
    Err(format!("no readable text on focused field (role='{role}')"))
}

/// Bounded DFS for the first `AXTextArea`/`AXTextField` descendant; returns its `AXValue`
/// (possibly empty, so an empty compose field reads as "" rather than an error). `None` when the
/// subtree has no text-entry element — so a non-field focus still skips.
unsafe fn descend_for_textfield(el: AXUIElementRef, depth: u32) -> Option<String> {
    if depth > 6 {
        return None;
    }
    for child in copy_children(el) {
        let role = copy_attr_string(child, "AXRole").unwrap_or_default();
        let found = if role == "AXTextArea" || role == "AXTextField" {
            Some(copy_attr_string(child, kAXValueAttribute).unwrap_or_default())
        } else {
            descend_for_textfield(child, depth + 1)
        };
        CFRelease(child as CFTypeRef);
        if found.is_some() {
            return found;
        }
    }
    None
}

/// Read an AXTextArea's content via the parameterized `AXStringForRange` attribute — the only way
/// to read text from Catalyst/UIKit text views (e.g. WhatsApp) that don't expose `AXValue`. Uses
/// `AXNumberOfCharacters` for the range; returns Some("") for an empty field so compose still fires.
unsafe fn read_string_for_range(el: AXUIElementRef) -> Option<String> {
    let n = copy_attr_i64(el, kAXNumberOfCharactersAttribute)?;
    if n <= 0 {
        return Some(String::new());
    }
    let range = CFRange { location: 0, length: n as isize };
    let range_val = AXValueCreate(kAXValueTypeCFRange, &range as *const CFRange as *const _);
    if range_val.is_null() {
        return None;
    }
    let attr = CFString::new(kAXStringForRangeParameterizedAttribute);
    let mut out: CFTypeRef = ptr::null();
    let err = AXUIElementCopyParameterizedAttributeValue(
        el,
        attr.as_concrete_TypeRef(),
        range_val as CFTypeRef,
        &mut out,
    );
    CFRelease(range_val as CFTypeRef);
    if err != kAXErrorSuccess || out.is_null() {
        return None;
    }
    if CFGetTypeID(out) != CFStringGetTypeID() {
        CFRelease(out);
        return None;
    }
    Some(CFString::wrap_under_create_rule(out as CFStringRef).to_string())
}

/// Read an integer-valued AX attribute (e.g. `AXNumberOfCharacters`).
unsafe fn copy_attr_i64(el: AXUIElementRef, attr: &str) -> Option<i64> {
    let a = CFString::new(attr);
    let mut value: CFTypeRef = ptr::null();
    let err = AXUIElementCopyAttributeValue(el, a.as_concrete_TypeRef(), &mut value);
    if err != kAXErrorSuccess || value.is_null() {
        return None;
    }
    if CFGetTypeID(value) != CFNumberGetTypeID() {
        CFRelease(value);
        return None;
    }
    let mut n: i64 = 0;
    let ok = CFNumberGetValue(
        value as CFNumberRef,
        kCFNumberSInt64Type,
        &mut n as *mut i64 as *mut std::ffi::c_void,
    );
    CFRelease(value);
    if ok {
        Some(n)
    } else {
        None
    }
}

/// Read the focused element's currently selected text (empty string if none).
pub fn read_selected_text() -> Result<String, String> {
    let _ax = ax_guard();
    unsafe {
        let el = copy_focused_element()?;
        let attr = CFString::new(kAXSelectedTextAttribute);
        let mut value: CFTypeRef = ptr::null();
        let err = AXUIElementCopyAttributeValue(el, attr.as_concrete_TypeRef(), &mut value);
        CFRelease(el as CFTypeRef);
        if err != kAXErrorSuccess || value.is_null() {
            return Ok(String::new());
        }
        Ok(CFString::wrap_under_create_rule(value as CFStringRef).to_string())
    }
}

/// Recursively collect visible text from an element subtree (bounded).
unsafe fn collect_text(
    el: AXUIElementRef,
    out: &mut String,
    max_chars: usize,
    elements_left: &mut i32,
    depth: u32,
) {
    if out.len() >= max_chars || *elements_left <= 0 || depth > 22 {
        return;
    }
    *elements_left -= 1;

    // Take only the FIRST usable attribute per node. Apps like Discord expose the same
    // message as AXValue *and* AXTitle *and* AXDescription (each with its own timestamp
    // readout), so grabbing all three triples the noise and pushes the newest messages
    // off the end of the budget. One readout per node fits far more real conversation.
    for attr in [kAXValueAttribute, kAXTitleAttribute, kAXDescriptionAttribute] {
        if let Some(s) = copy_attr_string(el, attr) {
            let t = s.trim();
            if t.is_empty() || t.len() >= 4000 {
                continue; // try the next attribute
            }
            if !out.ends_with(&format!("{t}\n")) {
                out.push_str(t);
                out.push('\n');
                if out.len() >= max_chars {
                    return;
                }
            }
            break; // got this node's text from one attribute; don't triple-count it
        }
    }

    for child in copy_children(el) {
        collect_text(child, out, max_chars, elements_left, depth + 1);
        CFRelease(child as CFTypeRef);
    }
}

/// Best-effort: read the conversation/content text NEAR the focused field as context.
///
/// We do NOT scan from the window root: in Discord/Slack/etc. that hits the server +
/// channel + member sidebars first and exhausts the budget before ever reaching the
/// messages. Instead we climb ancestors from the focused element (the cursor sits inside
/// the message region) and stop at the first ancestor that holds real content — which
/// captures the conversation and skips the sidebars (they're branches we never visit).
pub fn read_window_context(max_chars: usize) -> String {
    const MIN_CONTEXT_CHARS: usize = 400;
    let _ax = ax_guard();
    unsafe {
        let focused = match copy_focused_element() {
            Ok(e) => e,
            Err(_) => return String::new(),
        };

        let mut current = focused; // owned (+1)
        let mut result = String::new();
        for _ in 0..8 {
            let parent = match copy_attr_element(current, kAXParentAttribute) {
                Some(p) => p,
                None => break, // reached the top of the tree
            };
            let mut out = String::new();
            // Element budget scaled to the larger char budget so the walk reaches the full thread
            // instead of stalling on element count before it fills `max_chars`.
            let mut elements_left: i32 = 5000;
            collect_text(parent, &mut out, max_chars, &mut elements_left, 0);
            CFRelease(current as CFTypeRef);
            current = parent;
            result = out;
            if result.len() >= MIN_CONTEXT_CHARS {
                break; // first ancestor with real content = the conversation region
            }
        }
        CFRelease(current as CFTypeRef);
        result
    }
}

/// DEBUG instrumentation: log the focused element's ancestor chain — role, subrole, name, the
/// subtree's text length + a snippet — so we can design STRUCTURAL context scoping (which ancestor
/// is "the comment being replied to") from the real tree instead of hardcoding heuristics.
pub fn dump_focused_ancestry() {
    let _ax = ax_guard();
    unsafe {
        let focused = match copy_focused_element() {
            Ok(e) => e,
            Err(e) => {
                println!("[ax-dump] no focused element: {e}");
                return;
            }
        };
        println!("[ax-dump] === focused ancestry (innermost → outermost) ===");
        let mut current = focused; // owned (+1)
        for depth in 0..10 {
            let role = copy_attr_string(current, "AXRole").unwrap_or_default();
            let subrole = copy_attr_string(current, "AXSubrole").unwrap_or_default();
            let name = copy_attr_string(current, "AXTitle")
                .or_else(|| copy_attr_string(current, "AXDescription"))
                .unwrap_or_default();
            let mut txt = String::new();
            let mut left: i32 = 1500;
            collect_text(current, &mut txt, 100_000, &mut left, 0);
            let snippet: String = txt.replace('\n', " / ").chars().take(110).collect();
            println!(
                "[ax-dump] [{depth}] role={role} sub={subrole} name=\"{:.34}\" len={} | {}",
                name,
                txt.len(),
                snippet
            );
            let parent = match copy_attr_element(current, kAXParentAttribute) {
                Some(p) => p,
                None => break,
            };
            CFRelease(current as CFTypeRef);
            current = parent;
        }
        CFRelease(current as CFTypeRef);
        println!("[ax-dump] === end ===");
    }
}

// ── Capture anchors (C1/C2) ──────────────────────────────────────────────────
// Each capture stores ANCHORS — window title, url+domain, and the focused element's
// name/role/path — so clean drafts come from anchoring, not cleaner text. These enrich every
// snapshot and every cognee `remember`.

/// Anchor metadata for the current focus (all best-effort; missing pieces stay None).
#[derive(Debug, Default, Clone)]
pub struct Anchors {
    pub window_title: Option<String>,
    pub focused_role: Option<String>,
    pub focused_name: Option<String>,
    /// Ancestor breadcrumb, innermost→outermost, e.g. "AXTextArea > AXGroup(Comments) > AXWebArea".
    pub focused_path: Option<String>,
    pub url: Option<String>,
    pub domain: Option<String>,
}

/// A string- OR url-valued attribute as text (browsers expose AXURL as CFURL, AXDocument as CFString).
unsafe fn copy_attr_string_or_url(el: AXUIElementRef, attr: &str) -> Option<String> {
    let a = CFString::new(attr);
    let mut value: CFTypeRef = ptr::null();
    let err = AXUIElementCopyAttributeValue(el, a.as_concrete_TypeRef(), &mut value);
    if err != kAXErrorSuccess || value.is_null() {
        return None;
    }
    let tid = CFGetTypeID(value);
    if tid == CFStringGetTypeID() {
        return Some(CFString::wrap_under_create_rule(value as CFStringRef).to_string());
    }
    if tid == CFURLGetTypeID() {
        let url = CFURL::wrap_under_create_rule(value as CFURLRef);
        return Some(url.get_string().to_string());
    }
    CFRelease(value);
    None
}

/// The registrable host of a URL: scheme/creds/port/path stripped, leading "www." dropped.
/// Pure — unit-tested. "" if the input has no recognizable host.
pub fn domain_of(url: &str) -> String {
    let rest = url.split("://").nth(1).unwrap_or(url);
    let host = rest
        .split(['/', '?', '#'])
        .next()
        .unwrap_or("")
        .rsplit('@')
        .next()
        .unwrap_or("")
        .split(':')
        .next()
        .unwrap_or("");
    let host = host.to_lowercase();
    let host = host.strip_prefix("www.").unwrap_or(&host);
    if host.contains('.') && !host.contains(' ') {
        host.to_string()
    } else {
        String::new()
    }
}

/// Read the current focus anchors in ONE locked pass: focused element role/name, an ancestor
/// breadcrumb, the containing window's title, and (browsers) the web area's URL → domain.
pub fn read_anchors() -> Anchors {
    let _ax = ax_guard();
    let mut anchors = Anchors::default();
    unsafe {
        let focused = match copy_focused_element() {
            Ok(e) => e,
            Err(_) => return anchors,
        };

        anchors.focused_role = copy_attr_string(focused, "AXRole");
        anchors.focused_name = copy_attr_string(focused, kAXTitleAttribute)
            .or_else(|| copy_attr_string(focused, kAXDescriptionAttribute))
            .or_else(|| copy_attr_string(focused, "AXPlaceholderValue"))
            .filter(|s| !s.trim().is_empty());

        // Climb ancestors: build the breadcrumb, find the window (title) + web area (URL).
        let mut path_parts: Vec<String> = Vec::new();
        let mut current = focused; // owned (+1)
        for _ in 0..14 {
            let role = copy_attr_string(current, "AXRole").unwrap_or_default();
            if path_parts.len() < 6 && !role.is_empty() {
                let name = copy_attr_string(current, kAXTitleAttribute).unwrap_or_default();
                let name: String = name.trim().chars().take(24).collect();
                path_parts.push(if name.is_empty() {
                    role.clone()
                } else {
                    format!("{role}({name})")
                });
            }
            if role == "AXWebArea" && anchors.url.is_none() {
                anchors.url = copy_attr_string_or_url(current, "AXURL")
                    .or_else(|| copy_attr_string_or_url(current, "AXDocument"))
                    .filter(|u| u.starts_with("http"));
            }
            if role == "AXWindow" {
                anchors.window_title = copy_attr_string(current, kAXTitleAttribute)
                    .filter(|s| !s.trim().is_empty());
                // URL not found on the ancestor path (focus in a toolbar / omnibox)? Shallow-scan
                // the window for its web area before giving up.
                if anchors.url.is_none() {
                    anchors.url = find_web_area_url(current, 5, &mut 400);
                }
                break; // window is the outermost thing we care about
            }
            let parent = match copy_attr_element(current, kAXParentAttribute) {
                Some(p) => p,
                None => break,
            };
            CFRelease(current as CFTypeRef);
            current = parent;
        }
        CFRelease(current as CFTypeRef);

        if !path_parts.is_empty() {
            anchors.focused_path = Some(path_parts.join(" > "));
        }
    }
    if let Some(u) = &anchors.url {
        let d = domain_of(u);
        if !d.is_empty() {
            anchors.domain = Some(d);
        }
    }
    anchors
}

/// Shallow BFS below `el` for the first AXWebArea with a usable URL (bounded by depth + element budget).
unsafe fn find_web_area_url(el: AXUIElementRef, depth: u32, budget: &mut i32) -> Option<String> {
    if depth == 0 || *budget <= 0 {
        return None;
    }
    for child in copy_children(el) {
        *budget -= 1;
        let role = copy_attr_string(child, "AXRole").unwrap_or_default();
        let found = if role == "AXWebArea" {
            copy_attr_string_or_url(child, "AXURL")
                .or_else(|| copy_attr_string_or_url(child, "AXDocument"))
                .filter(|u| u.starts_with("http"))
        } else {
            find_web_area_url(child, depth - 1, budget)
        };
        CFRelease(child as CFTypeRef);
        if found.is_some() {
            return found;
        }
    }
    None
}

/// Select `len_utf16` code units starting at `pos_utf16` in the focused element, and VERIFY the
/// selection took by reading it back and comparing to `expect`. Some apps silently ignore
/// kAXSelectedTextRange writes (the per-app scar) — verification means the caller can trust a
/// `true` and type over the selection precisely, or fall back safely on `false`.
/// AX ranges are UTF-16 code units (CFString semantics) — callers use s.encode_utf16().count().
pub fn select_char_range(pos_utf16: usize, len_utf16: usize, expect: &str) -> bool {
    let _ax = ax_guard();
    unsafe {
        let Ok(el) = copy_focused_element() else {
            return false;
        };
        let range = CFRange {
            location: pos_utf16 as isize,
            length: len_utf16 as isize,
        };
        let value = AXValueCreate(kAXValueTypeCFRange, &range as *const CFRange as *const _);
        if value.is_null() {
            CFRelease(el as CFTypeRef);
            return false;
        }
        let attr = CFString::new(kAXSelectedTextRangeAttribute);
        let err = AXUIElementSetAttributeValue(el, attr.as_concrete_TypeRef(), value as CFTypeRef);
        CFRelease(value as CFTypeRef);
        if err != kAXErrorSuccess {
            CFRelease(el as CFTypeRef);
            return false;
        }
        // Read back what's actually selected — a silent no-op must not fool the caller.
        let sel_attr = CFString::new(kAXSelectedTextAttribute);
        let mut sel: CFTypeRef = ptr::null();
        let err = AXUIElementCopyAttributeValue(el, sel_attr.as_concrete_TypeRef(), &mut sel);
        CFRelease(el as CFTypeRef);
        if err != kAXErrorSuccess || sel.is_null() {
            return false;
        }
        let got = CFString::wrap_under_create_rule(sel as CFStringRef).to_string();
        got == expect
    }
}

/// The focused field's placeholder text, if any (some apps expose it as AXValue).
pub fn read_placeholder() -> Option<String> {
    let _ax = ax_guard();
    unsafe {
        let el = copy_focused_element().ok()?;
        let s = copy_attr_string(el, "AXPlaceholderValue");
        CFRelease(el as CFTypeRef);
        s
    }
}

#[cfg(test)]
mod tests {
    use super::domain_of;

    #[test]
    fn domain_of_strips_scheme_www_port_path() {
        assert_eq!(domain_of("https://www.linkedin.com/feed/"), "linkedin.com");
        assert_eq!(domain_of("http://outlook.office.com:443/mail?x=1"), "outlook.office.com");
        assert_eq!(domain_of("https://teams.microsoft.com#/chat"), "teams.microsoft.com");
        assert_eq!(domain_of("https://user:pass@example.com/x"), "example.com");
    }

    #[test]
    fn domain_of_rejects_non_urls() {
        assert_eq!(domain_of("not a url"), "");
        assert_eq!(domain_of("file:///Users/x/doc.txt"), "");
        assert_eq!(domain_of(""), "");
    }

    #[test]
    fn domain_of_lowercases() {
        assert_eq!(domain_of("https://WWW.LinkedIn.COM/in/jordan"), "linkedin.com");
    }
}
