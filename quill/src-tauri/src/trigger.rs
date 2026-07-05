// Global, chord-safe "bare right-Option" trigger → context-aware rewrite / compose, IN PLACE.
// Writes via clipboard-free synthetic typing (inject.rs). Requires Accessibility / Input Monitoring.
//
// Two modes, decided by the model from the raw field value:
//   • Field has the user's text → REWRITE it in their tone/structure (Mode 1) + learn it as style.
//   • Field empty / placeholder → COMPOSE the next message from the on-screen conversation (Mode 2).
// Pressing right-Option AGAIN on quill's own unedited output RE-ROLLS a different variant in place.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use core_foundation::runloop::CFRunLoop;
use core_graphics::event::{
    CGEventFlags, CGEventTap, CGEventTapLocation, CGEventTapOptions, CGEventTapPlacement,
    CGEventType, CallbackResult, EventField,
};

const KEYCODE_RIGHT_OPTION: i64 = 61;
// Read the WHOLE conversation, not a thin slice. A full LinkedIn feed post + its comment thread
// measured ~30k chars, so 60k captures it whole with 2x headroom (~15k tokens — a quarter of the
// model's 64k window). DETECTION of a mentioned person must search this whole thing: truncating it
// is what kept losing a buried commenter off the end (Kennon Fleisher, Hina Arora). Mode-1 faithful
// rewrites pass empty context anyway, so this only enriches compose / mention replies.
const CONTEXT_MAX_CHARS: usize = 60000;
// What we actually SEND the model for a mention reply: a focused window around the target's comment,
// not the whole 30k feed. Keeps the reply on-topic (less noise) and bounded regardless of thread size.
const MENTION_REPLY_CHARS: usize = 8000;
const LOADER_BASE: &str = "Quilling";

static OPTION_DOWN: AtomicBool = AtomicBool::new(false);
static CHORD: AtomicBool = AtomicBool::new(false);

// One rewrite/compose at a time: a second right-Option WHILE the LLM is working is ignored, so two
// presses can't spawn two generations racing to write the same field.
static GENERATING: AtomicBool = AtomicBool::new(false);

struct BusyGuard;
impl BusyGuard {
    fn acquire() -> Option<Self> {
        if GENERATING.swap(true, Ordering::SeqCst) {
            None
        } else {
            Some(BusyGuard)
        }
    }
}
impl Drop for BusyGuard {
    fn drop(&mut self) {
        GENERATING.store(false, Ordering::SeqCst);
    }
}

// quill's last output per surface, so right-Option AGAIN on the unedited result RE-ROLLS a fresh
// variant (regenerated from the ORIGINAL input, not from quill's own text). Edit the output and it
// no longer matches → the next press rewrites your edit instead.
struct LastOut {
    surface: String,
    input: String,  // the original Mode-1 text, or "" for a Mode-2 compose
    output: String, // what quill wrote
}
static LAST_OUTPUT: Mutex<Option<LastOut>> = Mutex::new(None);

/// Unix seconds of the most recent ⌥ trigger — the capture loop defers batch cognify while the
/// user is actively writing (batch LLM work starves interactive recall; observed all session).
static LAST_TRIGGER_TS: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(0);

/// Seconds since the user last triggered (i64::MAX if never).
pub fn secs_since_last_trigger() -> i64 {
    let t = LAST_TRIGGER_TS.load(Ordering::Relaxed);
    if t == 0 {
        return i64::MAX;
    }
    crate::db::now_secs() - t
}

fn remember_output(surface: &str, input: &str, output: &str) {
    if let Ok(mut g) = LAST_OUTPUT.lock() {
        *g = Some(LastOut {
            surface: surface.to_string(),
            input: input.to_string(),
            output: output.to_string(),
        });
    }
    // Inbox feed: every delivered draft is a reviewable event (best-effort, post-injection).
    let profile = crate::profile::key_for(surface);
    let preview: String = output.chars().take(140).collect();
    crate::db::insert_event("draft", &format!("draft delivered in {profile}"), &preview);
}

/// The (original input, output) quill last produced for this surface, if any.
fn last_for(surface: &str) -> Option<(String, String)> {
    let g = LAST_OUTPUT.lock().ok()?;
    g.as_ref()
        .filter(|l| l.surface == surface)
        .map(|l| (l.input.clone(), l.output.clone()))
}

/// Word-set Jaccard overlap between two texts (0.0–1.0). Cheap edit-vs-new-text discriminator:
/// an EDIT of quill's output shares most of its words; a fresh message shares few.
use crate::util::word_overlap;

/// improve() lane (M5): if the field now holds an EDITED version of quill's last output on this
/// surface, the user's corrections are the strongest teaching signal there is — remember the
/// before/after pair so future drafts absorb it. Fires only on genuine edits (word overlap),
/// never on unrelated new text.
fn maybe_remember_correction(surface: &str, prev_output: &str, current: &str) {
    let (p, c) = (prev_output.trim(), current.trim());
    if p.is_empty() || c.is_empty() || p == c {
        return;
    }
    if word_overlap(p, c) < 0.35 {
        return; // fresh text, not an edit of our draft
    }
    let body = format!(
        "[writing correction · {surface}]\nQuill drafted:\n{p}\n\nThe user edited it to (their preferred version — learn from the differences):\n{c}"
    );
    println!("[quill] improve: edit-delta captured on {surface}");
    std::thread::spawn(move || {
        if let Err(e) = crate::cognee::remember(&body, "corrections") {
            println!("[quill] cognee correction skipped: {e}");
        }
    });
}

/// Outlook-style reply detection: the reply body LOOKS non-empty because the client embeds the
/// quoted original (and often a signature) below the cursor — observed live: a 1,682-byte "draft"
/// that was 100% quoted email, which mis-routed to rewrite mode. If everything BEFORE the first
/// quote marker is blank, this is an EMPTY reply: return the quoted thread (it's cleaner context
/// than the AX window dump). The reply must then be INSERTED at the cursor, never replace_all —
/// replacing would destroy the quote history.
/// Mail-family app bundles: compose fields here are DOCUMENTS (WebView) where a synthetic ⌘A
/// selects the whole body including the quote below — select-all injection is forbidden
/// (observed live: Apple Mail reply-from-scratch lost its entire quote to the loader's ⌘A).
const MAIL_BUNDLE_PREFIXES: &[&str] = &[
    "com.apple.mail",
    "com.microsoft.Outlook",
    "com.readdle.smartemail", // Spark
    "com.airmail",
];

fn is_mail_surface(bundle: &str) -> bool {
    MAIL_BUNDLE_PREFIXES.iter().any(|p| bundle.starts_with(p))
}

/// Chat surfaces (Teams first): short-form conversation — no salutations, no sign-offs, the
/// visible thread is the reply context. Native bundles here; web Teams is caught by domain.
const CHAT_BUNDLE_PREFIXES: &[&str] = &[
    "com.microsoft.teams", // covers teams2 (new client) and classic
];
const CHAT_DOMAINS: &[&str] = &["teams.microsoft.com", "teams.live.com"];

fn is_chat_surface(bundle: &str) -> bool {
    CHAT_BUNDLE_PREFIXES.iter().any(|p| bundle.starts_with(p))
}

fn domain_is_chat(d: &str) -> bool {
    CHAT_DOMAINS
        .iter()
        .any(|cd| d == *cd || d.ends_with(&format!(".{cd}")))
}

/// Web Teams runs in ANY browser — the bundle can't tell it apart, the focused web area's URL
/// can. One cheap AX attribute read (anchors: title + URL, not a tree text walk); only called
/// when the bundle isn't already a known chat app and routing actually needs the answer.
fn is_chat_domain() -> bool {
    // Reuse the domain noted once at trigger start (no second AX walk).
    crate::profile::current_domain().as_deref().is_some_and(domain_is_chat)
}

/// Teams' AX tree emits every message up to FOUR times (list-item summary "msg by Sender",
/// timestamp rows, an aria-label variant "msg Sender date pm.", the bare text) plus per-message
/// chrome. Observed live: the duplication soup made the model echo the user's own earlier message
/// (the highest-frequency line wins) instead of replying to the newest one. Keep the first,
/// attributed form of each message; drop chrome, timestamps and the repeat variants.
fn clean_chat_context(raw: &str) -> String {
    const CHROME: &[&str] = &[
        "More message options",
        "Message List",
        "Actions for new message",
        "Show Formatting options",
        "Emoji, GIFs and Stickers",
        "Attach files",
        "Loop components",
        "Actions and apps",
        "Send (",
        "Select Shift+Enter",
        "starts a new line",
    ];
    let mut kept_norms: Vec<String> = Vec::new();
    let mut out = String::new();
    for line in raw.lines() {
        let t = line.trim();
        if t.is_empty() || t == "Send" || t == "Seen" || CHROME.iter().any(|c| t.starts_with(c)) {
            continue;
        }
        // Timestamp-only rows: "25/06 12:27 pm", "Yesterday 2:40 pm", "Yesterday", "Today".
        let lower = t.to_lowercase();
        if t.len() <= 26
            && (lower.ends_with(" pm")
                || lower.ends_with(" am")
                || lower.ends_with(" pm.")
                || lower.ends_with(" am.")
                || lower == "yesterday"
                || lower == "today")
        {
            continue;
        }
        // Normalize with " by " removed so "msg by Sender" and "msg Sender date" variants of the
        // SAME message become prefix-comparable; then drop any line whose normalized form
        // contains (or is contained in) a recently kept line — that's a repeat variant.
        let norm: String = t
            .replace(" by ", " ")
            .to_lowercase()
            .chars()
            .filter(|c| c.is_alphanumeric())
            .collect();
        if norm.is_empty() {
            continue;
        }
        // A repeat VARIANT shares a long prefix with the kept form ("msg sender" / "msg sender
        // date" / "msg"); unrelated messages that merely start alike don't reach 8 chars of
        // common prefix in normalized form. Exact repeats dedup at any length.
        let dup = kept_norms.iter().rev().take(8).any(|r| {
            let (short, long) =
                if r.len() <= norm.len() { (r.as_str(), norm.as_str()) } else { (norm.as_str(), r.as_str()) };
            short == long || (short.len() >= 8 && long.starts_with(short))
        });
        if dup {
            continue;
        }
        kept_norms.push(norm);
        out.push_str(t);
        out.push('\n');
    }
    out
}

/// Last `max` chars of `s`. Chat context is newest-at-the-BOTTOM: when a thread outgrows the
/// budget, the head (oldest) must be what gets dropped. Observed live: a giant pasted report
/// early in a thread exhausted the head-first budget and quill replied to the REPORT instead
/// of the newest exchange (handled by clipping from the tail).
fn tail_chars(s: &str, max: usize) -> String {
    let n = s.chars().count();
    if n <= max {
        s.to_string()
    } else {
        s.chars().skip(n - max).collect()
    }
}

/// Echo guard for chat: output that is essentially a message already present in the thread must
/// never be sent (observed live: quill retyped the user's own day-old question as the "reply").
/// Compare against the message TEXT only — the canonical cleaned line is "msg  by Sender", and
/// the sender tail dilutes the overlap score below any useful threshold.
fn echoes_thread(out: &str, ctx: &str) -> bool {
    ctx.lines().any(|l| {
        let msg = l.split(" by ").next().unwrap_or(l).trim();
        msg.len() >= 12 && word_overlap(out, msg) >= 0.6
    })
}

/// Append an email sign-off from the user's first name ("Best,\nJordan"). Used for mail composes
/// where the field carried NO client signature to close the message (Apple Mail replies usually
/// don't). When a signature IS present the client closes it, so we don't sign twice. No-op if the
/// user's name isn't set.
fn with_sign_off(body: String, surface: &str) -> String {
    // Per-profile signature first (the user's own block), then a class-aware default. This is only
    // reached on mail surfaces (the mail-compose branch), so the default is the formal full-name
    // block — a chat/social profile would never sign here.
    let key = crate::profile::key_for(surface);
    if let Some(sig) = crate::profile::signature_for(&key) {
        return format!("{}\n\n{}", body.trim_end(), sig.trim());
    }
    let Some(name) = crate::settings::user_name() else {
        return body;
    };
    format!("{}\n\nBest regards,\n{name}", body.trim_end())
}

/// Byte index where the quoted thread begins in a reply body, if any quote marker is present.
fn quote_marker_idx(value: &str) -> Option<usize> {
    // ">"-quoted block (universal plaintext quoting): two consecutive lines starting with '>'.
    let gt_block = {
        let mut idx = 0usize;
        let mut found = None;
        let mut prev_gt_start: Option<usize> = None;
        for line in value.split_inclusive('\n') {
            if line.trim_start().starts_with('>') {
                match prev_gt_start {
                    Some(start) => {
                        found = Some(start);
                        break;
                    }
                    None => prev_gt_start = Some(idx),
                }
            } else {
                prev_gt_start = None;
            }
            idx += line.len();
        }
        found
    };
    [
        value.find("-----Original Message-----"),
        value.find("Begin forwarded message:"), // Apple Mail forwards
        value.find("________________"), // Outlook's separator line (long underscore run)
        // Header block: a "From:" line with another header field nearby.
        value.find("From:").filter(|&i| {
            let tail: String = value[i..].chars().take(400).collect();
            tail.contains("Sent:") || tail.contains("To:") || tail.contains("Date:")
        }),
        // Gmail/Apple Mail style: "On <date>, <name> wrote:" — at string start or any line start.
        value.find(" wrote:").and_then(|i| {
            value[..i]
                .rfind("\nOn ")
                .map(|j| j + 1)
                .or(if value.starts_with("On ") { Some(0) } else { None })
        }),
        gt_block,
    ]
    .into_iter()
    .flatten()
    .min()
}

fn quoted_reply_split(value: &str) -> Option<&str> {
    let idx = quote_marker_idx(value)?;
    let pre = &value[..idx];
    if is_blank(pre) || is_signature_block(pre) {
        Some(value[idx..].trim())
    } else {
        None // the user has actually typed something above the quote — that's a real draft
    }
}

/// Split `pre` (the text above the quote) into (draft, signature-tail). The signature is the
/// final block starting at a line that begins with the user's name — optionally preceded by a
/// one-line closing ("Best regards,") and blank lines, which belong to the signature too.
fn split_trailing_signature(pre: &str) -> (&str, &str) {
    let Some(name) = crate::settings::user_name().filter(|n| !n.is_empty()) else {
        return (pre, "");
    };
    let lower = pre.to_lowercase();
    let lname = name.to_lowercase();
    // Last line START that begins with the user's name.
    let mut sig_start = None;
    let mut search = 0;
    while let Some(rel) = lower[search..].find(&lname) {
        let i = search + rel;
        if i == 0 || lower.as_bytes()[i - 1] == b'\n' {
            sig_start = Some(i);
        }
        search = i + lname.len();
    }
    let mut start = match sig_start {
        Some(s) => s,
        None => return (pre, ""),
    };
    // Absorb blank lines and ONE short closing line ("Best regards,") above the name.
    let mut lines_end = start;
    loop {
        let head = &pre[..lines_end];
        let Some(prev_nl) = head.trim_end_matches('\n').rfind('\n') else {
            break;
        };
        let line = pre[prev_nl + 1..lines_end].trim();
        if line.is_empty() {
            lines_end = prev_nl + 1;
            continue;
        }
        if line.len() <= 30 && line.ends_with(',') {
            start = prev_nl + 1; // the closing line joins the signature
        }
        break;
    }
    start = start.min(lines_end);
    // to_lowercase can shift byte offsets on exotic unicode — never slice off a char boundary.
    if !pre.is_char_boundary(start) {
        return (pre, "");
    }
    (&pre[..start], &pre[start..])
}

/// Is `pre` just the user's email signature? Outlook inserts it between the cursor and the quote
/// on every reply (e.g. "Jordan Lee Rivera / Product Lead | Acme…" ABOVE the
/// From:-block made the reply look like a typed draft). Heuristic: signatures BEGIN with the
/// user's own name — a real reply never does (you sign at the end) — and stay short.
fn is_signature_block(pre: &str) -> bool {
    let t = pre.trim();
    if t.is_empty() || t.chars().count() > 400 {
        return false;
    }
    match crate::settings::user_name() {
        Some(name) if !name.is_empty() => t.to_lowercase().starts_with(&name.to_lowercase()),
        _ => false,
    }
}

/// Explicit-instruction prefix (a "/g"-style shortcut): text starting with "//" or "/g " is
/// ALWAYS an instruction/query to execute against context + memory — never a draft to polish.
/// Returns the instruction with the prefix stripped. Two keystrokes buy certainty when the
/// draft-vs-instruction inference must not miss.
fn explicit_instruction(value: &str) -> Option<String> {
    let t = value.trim_start();
    for p in ["//", "/g "] {
        if let Some(rest) = t.strip_prefix(p) {
            let rest = rest.trim();
            if !rest.is_empty() {
                return Some(rest.to_string());
            }
        }
    }
    None
}

/// Loader debris left by a focus-guard abort: "Quilling", bare dots, or both. A suppressed
/// session can't clean the original field (keystrokes only reach the focused app), so the next
/// ⌥ press in that field sweeps the debris and treats it as empty.
fn is_loader_remnant(value: &str) -> bool {
    let t = value.trim();
    if t.is_empty() {
        return false;
    }
    let stripped = t.strip_prefix(LOADER_BASE).unwrap_or(t).trim();
    let is_dots = |s: &str| s.chars().all(|c| c == '.' || c == '…');
    (t.starts_with(LOADER_BASE) || stripped.is_empty() || is_dots(t))
        && is_dots(stripped)
        && stripped.chars().count() <= 5
}

/// Is `s` a short ALL-CAPS instruction (e.g. "MAKE IT CONCISE")?
fn is_caps_instruction(s: &str) -> bool {
    let s = s.trim();
    if s.is_empty() || s.chars().count() > 80 {
        return false;
    }
    let letters: Vec<char> = s.chars().filter(|c| c.is_alphabetic()).collect();
    letters.len() >= 3 && letters.iter().all(|c| c.is_uppercase())
}

/// A stable, lowercased handle for locating a person inside a big thread: their first two
/// name-words with surrounding punctuation stripped ("Adrian Vale, PhD, DBA" → "adrian vale").
/// Credentials/extra words are dropped so the key matches however the thread renders the name.
fn name_key(target: &str) -> String {
    target
        .split_whitespace()
        .map(|w| w.trim_matches(|c: char| !c.is_alphanumeric()))
        .filter(|w| !w.is_empty())
        .take(2)
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// Slice a ~`budget`-char window of `thread` centred on where `target` is mentioned (their comment),
/// so the model sees THEIR comment instead of the truncated top of a long feed. Detection happens
/// against the full thread by the caller; this just keeps what we send focused + within budget.
fn focus_on_target(thread: &str, target: &str, budget: usize) -> String {
    let total = thread.chars().count();
    if total <= budget {
        return thread.to_string();
    }
    let key = name_key(target);
    // First occurrence of the name-key = the person's own comment header ("View X's profile / X /
    // …<their comment>"). The pre-filled @mention in the reply box is the LAST occurrence, so `find`
    // lands on the comment, not the box.
    let char_pos = (!key.is_empty())
        .then(|| thread.to_lowercase().find(&key))
        .flatten()
        .map(|byte| thread[..byte].chars().count())
        .unwrap_or(0);
    // Start a little before the name to catch the "View X's profile" lead-in; clamp so the window
    // stays inside the string.
    let start = char_pos.saturating_sub(300).min(total - budget);
    thread.chars().skip(start).take(budget).collect()
}

/// Is `s` just a person's NAME (2–6 capitalised words, allowing trailing punctuation + credentials
/// like "PhD,"/"DBA")? Detects a reply box pre-filled with an @mention (LinkedIn "Reply" inserts
/// "Adrian Vale, PhD, DBA"). Pair with a context check so a short capitalised message isn't misread.
fn is_name_mention(s: &str) -> bool {
    let s = s.trim();
    if s.is_empty() || s.chars().count() > 50 {
        return false;
    }
    let words: Vec<&str> = s.split_whitespace().collect();
    if words.len() < 2 || words.len() > 6 {
        return false;
    }
    // Real names don't follow English title-case ("Fernandez Marco del rio" broke the old
    // every-word-capitalized rule — observed live). New rule: the FIRST TWO words must start
    // uppercase (given name + surname), later words may be lowercase; every word must be purely
    // alphanumeric once surrounding punctuation is stripped ("Adrian Vale, PhD, DBA" still passes,
    // and typed sentences like "Thanks for sharing" still fail on the lowercase second word).
    words.iter().enumerate().all(|(i, w)| {
        let w = w.trim_matches(|c: char| !c.is_alphanumeric()); // strip surrounding . , etc.
        if w.is_empty() || !w.chars().all(|c| c.is_alphanumeric()) {
            return false;
        }
        // "Capitalized" = uppercase OR caseless script (Arabic/CJK names have no case at all —
        // requiring is_uppercase() made every Arabic-named mention undetectable).
        i >= 2 || w.chars().next().is_some_and(|c| !c.is_lowercase())
    })
}

/// If the field IS (or ends with) an ALL-CAPS instruction relative to quill's previous output,
/// return that instruction — a directed edit (type "MAKE IT CONCISE" after the draft, then ⌥).
fn caps_instruction(value: &str, prev: &str) -> Option<String> {
    let v = value.trim();
    let p = prev.trim();
    if !p.is_empty() && v.len() > p.len() && v.starts_with(p) {
        let tail = v[p.len()..].trim();
        if is_caps_instruction(tail) {
            return Some(tail.to_string());
        }
    }
    if is_caps_instruction(v) {
        return Some(v.to_string());
    }
    None
}


/// RAII: marks "quill is injecting" for the duration of a write so the capture loop skips
/// recording our own loader / output — AND arms the focus guard: keystrokes only land while
/// the app that owned the field at trigger time is still frontmost (a PID-guard).
struct InjectGuard;
impl InjectGuard {
    fn new() -> Self {
        crate::inject::set_injecting(true);
        if let Some(pid) = crate::app::frontmost_pid() {
            crate::inject::set_target_pid(pid);
        }
        InjectGuard
    }
}
impl Drop for InjectGuard {
    fn drop(&mut self) {
        crate::inject::clear_target_pid();
        crate::inject::set_injecting(false);
    }
}


/// Visually empty? Only whitespace / control / zero-width / format chars remain. Discord's "empty"
/// input is a couple of invisible Slate chars, not a zero count.
pub(crate) fn is_blank(s: &str) -> bool {
    s.chars().all(|c| {
        c.is_whitespace()
            || c.is_control()
            || matches!(c,
                '\u{00A0}' | '\u{200B}'..='\u{200F}' | '\u{2028}'..='\u{202F}'
                | '\u{205F}'..='\u{206F}' | '\u{FEFF}')
    })
}

/// True if `v` is genuine user-authored text — not blank, not a common UI placeholder. This gates
/// STYLE LEARNING only (so we never learn "Add a comment" as the user's tone); the placeholder
/// list is small + app-agnostic, and a miss just means one stray sample (pruned), never a wrong
/// rewrite — the MODEL still decides rewrite-vs-compose from the raw value.
fn looks_like_user_text(v: &str) -> bool {
    if is_blank(v) {
        return false;
    }
    let t = v.trim().to_lowercase();
    const PLACEHOLDERS: &[&str] = &[
        "add a comment",
        "write a comment",
        "write a reply",
        "leave a comment",
        "reply",
        "message",
        "what's on your mind",
        "start a post",
        "write something",
        "type a message",
        "send a message",
        "add a reply",
        "say something",
    ];
    !PLACEHOLDERS.iter().any(|p| t == *p || t.starts_with(p))
}

/// Personalization injected into the prompt: WHO the user is (identity) + their learned per-app
/// style. Identity makes replies role-correct (thank vs congratulate yourself).
fn persona_for(surface: &str) -> String {
    // The DOMAIN-AWARE profile key (LinkedIn-in-any-browser is one profile) — identity, voice and
    // signature all hang off it, so the user reads as a different person per platform.
    persona_for_key(&crate::profile::key_for(surface))
}

/// The persona block for a specific profile key. Split out from `persona_for` so the Profiles
/// "preview voice" command can render the same voice for any platform without a live surface.
pub fn persona_for_key(key: &str) -> String {
    let mut p = String::new();
    let name = crate::settings::user_name();
    if let Some(ref n) = name {
        p.push_str(&format!(
            "You are writing AS \"{n}\" (the user). Reply from THEIR perspective — if someone \
congratulates or thanks them, respond as the person being congratulated/thanked (thank them back); \
never congratulate the user on their own post or address them by their own name.\n"
        ));
    }

    // Ground the reply/post in WHO the user is on THIS platform (per-profile identity, falling
    // back to the global one when a platform-specific profile isn't set yet).
    let id_block = crate::identity::for_prompt_keyed(key);
    if !id_block.is_empty() {
        p.push_str(&id_block);
        p.push('\n');
    }

    let bullets = crate::style::bullets_for(key);
    if !bullets.is_empty() {
        p.push_str("Match the user's own writing style in this app:\n");
        for b in &bullets {
            p.push_str(&format!("- {b}\n"));
        }
        // Voice ≠ sloppiness: learned bullets may describe the user's fast typing (lowercase,
        // typos) — emulate the TONE, never the mechanics (observed live: drafts came out with
        // lowercase first letters because the user types that way).
        p.push_str(
            "(Emulate the tone and energy — but ALWAYS use standard capitalization, spelling and \
punctuation, regardless of any style rule above. If you use an emoji, pick one that fits THIS \
content — never repeat the same emoji across a text or fall back to a signature emoji like 🚀.)\n",
        );
    }
    if name.is_some() || !bullets.is_empty() {
        println!(
            "[quill] persona: identity={} style={} rule(s) for profile {key}",
            name.is_some(),
            bullets.len()
        );
    }
    p
}

/// Show the in-field "Quilling…" loader, run `gen` (the LLM call), then replace the field with
/// the result — or restore `restore_to` on failure. Returns the generated output on success.
fn run_with_loader(restore_to: &str, gen: impl FnOnce() -> Result<String, String>) -> Option<String> {
    let _guard = InjectGuard::new();
    crate::inject::replace_all(LOADER_BASE);

    // Animate ONLY the trailing dots — the base word is laid down ONCE above and never touched
    // again. The previous version re-asserted the FULL string every frame via replace_all, whose
    // select-all flashed the whole "Quilling" word highlighted on every tick (the loader must
    // read as calm, not as a blinking selection). Relative dot edits stay quiet. Capped at TWO
    // dots: three periods get auto-substituted to "…" on some fields (WhatsApp/Catalyst) and would
    // desync a fixed backspace — two never form an ellipsis. We only ever remove dots WE added, so
    // the base can't be eaten, and the final replace below resets the field regardless of any
    // dropped keystroke.
    let running = Arc::new(AtomicBool::new(true));
    let running2 = running.clone();
    let anim = std::thread::spawn(move || {
        let mut dots = 0usize;
        while running2.load(Ordering::SeqCst) {
            std::thread::sleep(Duration::from_millis(300));
            if !running2.load(Ordering::SeqCst) {
                break;
            }
            if dots < 2 {
                crate::inject::type_text(".");
                dots += 1;
            } else {
                crate::inject::backspace(dots); // remove exactly the dots we added, never the base
                dots = 0;
            }
        }
    });

    let result = gen();
    running.store(false, Ordering::SeqCst);
    let _ = anim.join();

    match result {
        Ok(out) if !looks_like_user_text(&out) => {
            // Output-side sanity: under thin context the model can echo the field's placeholder
            // ("Add a comment…") back as the "draft" — observed live. Never inject that.
            crate::inject::replace_all(restore_to);
            eprintln!("[quill] output looked like a placeholder — dropped, field restored");
            None
        }
        Ok(out) => {
            crate::inject::replace_all(&out);
            Some(out)
        }
        Err(e) => {
            crate::inject::replace_all(restore_to); // restore the field's prior content
            eprintln!("[quill] FAILED: {e}");
            None
        }
    }
}

/// Like `run_with_loader`, but never select-alls: types the loader OVER the current selection
/// (or at the cursor), and on completion backspaces it and types the result in place. Used when
/// a precise region is already selected (rewrite-above-quote) so the rest of the field stays
/// physically untouched. On failure the original `restore_to` text is typed back.
fn run_with_loader_typed(
    restore_to: &str,
    gen: impl FnOnce() -> Result<String, String>,
) -> Option<String> {
    use std::sync::atomic::AtomicUsize;

    let _guard = InjectGuard::new();
    crate::inject::type_text(LOADER_BASE); // replaces the selection with the loader
    let base_len = LOADER_BASE.chars().count();
    let running = Arc::new(AtomicBool::new(true));
    let dots = Arc::new(AtomicUsize::new(0));
    let (r2, d2) = (running.clone(), dots.clone());
    let anim = std::thread::spawn(move || {
        let mut n = 0usize;
        while r2.load(Ordering::SeqCst) {
            std::thread::sleep(Duration::from_millis(300));
            if !r2.load(Ordering::SeqCst) {
                break;
            }
            if n < 3 {
                crate::inject::type_text(".");
                n += 1;
            } else {
                crate::inject::backspace(3);
                n = 0;
            }
            d2.store(n, Ordering::SeqCst);
        }
    });

    let result = gen();
    running.store(false, Ordering::SeqCst);
    let _ = anim.join();
    crate::inject::backspace(base_len + dots.load(Ordering::SeqCst));

    match result {
        Ok(out) if looks_like_user_text(&out) => {
            crate::inject::type_text(out.trim());
            Some(out.trim().to_string())
        }
        Ok(_) => {
            crate::inject::type_text(restore_to);
            eprintln!("[quill] output looked like a placeholder — restored the draft");
            None
        }
        Err(e) => {
            crate::inject::type_text(restore_to);
            eprintln!("[quill] FAILED: {e}");
            None
        }
    }
}

/// On a COMPOSE, enrich the on-screen context with relevant earlier activity via cognee's
/// graph+vector recall. `surface` = the current app's bundle id (its screen is already the
/// primary context).
fn augment_context(context: String, surface: &str) -> String {
    match crate::retrieve::memory_block(&context, surface) {
        Some(block) => format!("{context}\n\n{block}"),
        None => context,
    }
}

fn handle_trigger() {
    // Never act in excluded apps; fail CLOSED if we can't identify the app.
    let surface = match crate::app::frontmost_bundle_id() {
        Some(bid) if crate::app::is_excluded(&bid) => {
            println!("[quill] skipped — excluded app: {bid}");
            return;
        }
        None => {
            println!("[quill] skipped — could not identify frontmost app");
            return;
        }
        Some(bid) => bid,
    };

    // One generation at a time (a second press while working is ignored).
    let Some(_busy) = BusyGuard::acquire() else {
        println!("[quill] busy — ignoring trigger");
        return;
    };
    LAST_TRIGGER_TS.store(crate::db::now_secs(), Ordering::Relaxed);

    // Note the focused window's domain ONCE for this trigger (a bounded anchors read, not the
    // heavy tree walk) so persona/style/sign-off resolve the same DOMAIN-AWARE profile key —
    // LinkedIn in any browser is one profile, distinct from other tabs and native apps. gen()
    // runs on this thread, so the cached value is visible wherever the persona is built.
    crate::profile::note_domain(crate::ax::read_anchors().domain);

    // LATENCY: no heavy AX work before the loader. The whole-window context walk costs SECONDS on
    // big windows (observed: 40KB Outlook reads = thousands of AX IPC round-trips) — every handler
    // now reads it lazily AFTER its loader is showing. Only cheap field-level reads happen here.
    let selection = crate::ax::read_selected_text().unwrap_or_default();
    if !selection.trim().is_empty() {
        handle_selection(selection, surface);
        return;
    }

    // Don't risk overwriting unread text: a flaky AX read must NOT look "empty" and overwrite the
    // user's real draft. Abort if we can't read the field.
    let mut value = match crate::ax::read_focused_value() {
        Ok(v) => v,
        Err(e) => {
            println!("[quill] skipped — couldn't read focused field: {e} (front={surface})");
            return;
        }
    };

    // Sweep loader debris from a previous focus-guard abort ("Quilling…" / bare dots) — the
    // suppressed session couldn't clean the field it left. Clear it and treat as empty.
    if is_loader_remnant(&value) {
        println!("[quill] cleared loader debris from an aborted generation");
        let _debris_guard = InjectGuard::new();
        crate::inject::replace_all("");
        value = String::new();
    }

    let placeholder = crate::ax::read_placeholder().unwrap_or_default();
    if std::env::var("QUILL_AX_DEBUG").is_ok() {
        crate::ax::dump_focused_ancestry(); // debug only: this walk costs seconds on big windows
    }

    // A reply box pre-filled with someone's @mention (LinkedIn "Reply" inserts the name) is a
    // reply TARGET, not text to rewrite — checked BEFORE the caps path ("CARLO VEGA REYES" is
    // all-caps). MIXED-case names route straight to the mention flow (context read lazily behind
    // its loader). ALL-CAPS name-shapes are ambiguous with instructions like "MAKE IT CONCISE" —
    // only they pay an eager context read to disambiguate.
    if is_name_mention(&value) {
        if !is_caps_instruction(&value) {
            handle_mention_reply(value, surface);
            return;
        }
        let key = name_key(&value);
        let context = crate::ax::read_window_context(CONTEXT_MAX_CHARS);
        if !key.is_empty() && context.to_lowercase().contains(&key) {
            let focused = focus_on_target(&context, &value, MENTION_REPLY_CHARS);
            println!(
                "[quill] mention reply to \"{}\" in {surface} (all-caps, thread {}b → focus {}b)",
                value.trim(),
                context.len(),
                focused.len()
            );
            handle_mention_reply_with(value, focused, surface);
            return;
        }
        // all-caps + not in thread → likely a directed-edit instruction; fall through.
    }

    // If quill produced output here, ⌥-again means either an ALL-CAPS instruction → directed
    // revision (e.g. "MAKE IT CONCISE"), or the unedited output → re-roll a different variant.
    if let Some((input, prev)) = last_for(&surface) {
        if let Some(instr) = caps_instruction(&value, &prev) {
            println!("[quill] revise in {surface}: {instr}");
            handle_revise(prev, instr, surface);
            return;
        }
        if value.trim() == prev.trim() {
            println!("[quill] re-roll in {surface}");
            handle_reroll(input, prev, surface, placeholder);
            return;
        }
        // Neither re-roll nor CAPS edit: if the field holds an EDIT of our last output, that's
        // the improve() signal — capture the before/after, then fall through (the edited text is
        // treated as the user's own writing below, exactly as before).
                maybe_remember_correction(&surface, &prev, &value);
    }

    println!(
        "[quill] trigger in {surface} (field {}b, ph={:?})",
        value.len(),
        placeholder
    );
    handle_whole_field(value, surface, placeholder);
}

/// Read the window context AFTER a loader is already showing (the walk costs seconds on big
/// windows). The read includes our own loader text — strip it so it never reaches the prompt.
fn read_context_lazy() -> String {
    crate::ax::read_window_context(CONTEXT_MAX_CHARS).replace(LOADER_BASE, "")
}

/// Mention flow, lazy-context variant (the common path): loader first, THEN the expensive window
/// read, then focus the thread on the target if we can find them.
fn handle_mention_reply(value: String, surface: String) {
    handle_mention_reply_inner(value, None, surface);
}

/// Mention flow when the caller already paid for a context read (all-caps disambiguation).
fn handle_mention_reply_with(value: String, focused_context: String, surface: String) {
    handle_mention_reply_inner(value, Some(focused_context), surface);
}

/// Reply box pre-filled with an @mention → compose a reply from the thread and APPEND it after the
/// mention (don't replace it — that would destroy the tag). No in-field loader for the same reason.
fn handle_mention_reply_inner(value: String, pre_context: Option<String>, surface: String) {
    use std::sync::atomic::AtomicUsize;

    let _guard = InjectGuard::new();
    let lead = if value.ends_with(char::is_whitespace) { "" } else { " " };

    // Append the "Quilling…" loader AFTER the @mention (don't replace — that destroys the tag),
    // animated like the other surfaces. We remove exactly what we typed when the reply is ready.
    let base = format!("{lead}{LOADER_BASE}");
    crate::inject::type_text(&base);
    let base_len = base.chars().count();
    let running = Arc::new(AtomicBool::new(true));
    let dots = Arc::new(AtomicUsize::new(0));
    let (r2, d2) = (running.clone(), dots.clone());
    let anim = std::thread::spawn(move || {
        let mut n = 0usize;
        while r2.load(Ordering::SeqCst) {
            std::thread::sleep(Duration::from_millis(300));
            if !r2.load(Ordering::SeqCst) {
                break;
            }
            if n < 3 {
                crate::inject::type_text(".");
                n += 1;
            } else {
                crate::inject::backspace(3);
                n = 0;
            }
            d2.store(n, Ordering::SeqCst);
        }
    });

    let persona = persona_for(&surface);
    // Lazy path: the loader is showing, so the expensive window walk is hidden latency now.
    // Focus the thread on the target's own comment when we can find them; else use the full read.
    let context = pre_context.unwrap_or_else(|| {
        let full = read_context_lazy();
        let key = name_key(&value);
        if !key.is_empty() && full.to_lowercase().contains(&key) {
            let focused = focus_on_target(&full, &value, MENTION_REPLY_CHARS);
            println!(
                "[quill] mention reply to \"{}\" in {surface} (thread {}b → focus {}b)",
                value.trim(),
                full.len(),
                focused.len()
            );
            focused
        } else {
            println!(
                "[quill] mention reply to \"{}\" in {surface} (target not in context read — full thread)",
                value.trim()
            );
            full
        }
    });
    // Reply specifically to the TARGET (the pre-filled @mention) — the model finds THEIR comment in
    // the thread and replies to it, instead of grabbing the loudest comment.
    let result = crate::llm::reply_to(value.trim(), &context, &persona);

    running.store(false, Ordering::SeqCst);
    let _ = anim.join();
    crate::inject::backspace(base_len + dots.load(Ordering::SeqCst)); // remove loader, keep mention

    match result {
        Ok(out) => {
            crate::inject::type_text(&format!("{lead}{}", out.trim()));
            println!("[quill] mention reply appended after \"{}\"", value.trim());
        }
        Err(e) => eprintln!("[quill] mention reply FAILED: {e}"),
    }
}

/// Outlook-style empty reply over a quoted thread: compose from the QUOTE (cleaner than the AX
/// window dump) + memory, and INSERT at the cursor (top of body) — never replace_all, which would
/// destroy the quote history the recipient expects. Window context is read lazily behind the loader.
fn handle_quoted_reply(quote: String, surface: String) {
    use std::sync::atomic::AtomicUsize;

    let _guard = InjectGuard::new();
    println!(
        "[quill] quoted-reply compose in {surface} (quote {}b)",
        quote.len()
    );

    // Loader typed at the cursor (top of the reply body), removed exactly when done.
    crate::inject::type_text(LOADER_BASE);
    let base_len = LOADER_BASE.chars().count();
    let running = Arc::new(AtomicBool::new(true));
    let dots = Arc::new(AtomicUsize::new(0));
    let (r2, d2) = (running.clone(), dots.clone());
    let anim = std::thread::spawn(move || {
        let mut n = 0usize;
        while r2.load(Ordering::SeqCst) {
            std::thread::sleep(Duration::from_millis(300));
            if !r2.load(Ordering::SeqCst) {
                break;
            }
            if n < 3 {
                crate::inject::type_text(".");
                n += 1;
            } else {
                crate::inject::backspace(3);
                n = 0;
            }
            d2.store(n, Ordering::SeqCst);
        }
    });

    let persona = persona_for(&surface);
    // Email-shaped prompt: salutation from the thread's sender, NO sign-off (the client appends
    // the signature below the insertion point). Window + memory are background only — both read
    // lazily now that the loader is showing.
    let background = augment_context(read_context_lazy(), &surface);
    let result = crate::llm::email_reply(&quote, &background, &persona);

    running.store(false, Ordering::SeqCst);
    let _ = anim.join();
    crate::inject::backspace(base_len + dots.load(Ordering::SeqCst));

    match result {
        // Floor catches true garbage (empty, "Dear", bare dots) — but legitimately brief replies
        // exist ("Noted, thank you." on an FYI thread = 17 chars, observed live being wrongly
        // rejected at a 25-char floor). 12 chars = shortest plausible salutation-plus-word.
        // smells_meta catches the model talking ABOUT the task instead of doing it.
        Ok(out)
            if looks_like_user_text(&out)
                && out.trim().chars().count() >= 12
                && !crate::llm::smells_meta(&out) =>
        {
            // Reply above the quote, one blank line between (standard top-posting shape).
            crate::inject::type_text(&format!("{}\n", out.trim()));
            remember_output(&surface, "", &out);
            println!("[quill] quoted-reply OK ({} chars, quote preserved)", out.len());
        }
        Ok(out) => eprintln!(
            "[quill] quoted-reply output rejected ({} chars — stub/placeholder), nothing typed",
            out.trim().chars().count()
        ),
        Err(e) => eprintln!("[quill] quoted-reply FAILED: {e}"),
    }
}

fn handle_whole_field(value: String, surface: String, placeholder: String) {
    // Explicit instruction ("// …" / "/g …"): certainty override — ALWAYS
    // executed against context + memory, never polished as a draft. Checked first: two
    // keystrokes of prefix beat any inference. (Structured email fields handle it below so
    // signature/quote semantics stay correct.)
    if quote_marker_idx(&value).is_none() && split_trailing_signature(&value).1.trim().is_empty() {
        if let Some(instr) = explicit_instruction(&value) {
            println!("[quill] explicit instruction in {surface} ({}b)", instr.len());
            let persona = persona_for(&surface);
            if let Some(out) = run_with_loader(&value, || {
                let ctx = augment_context(read_context_lazy(), &surface);
                crate::llm::complete(&instr, &ctx, &persona, &placeholder)
            }) {
                remember_output(&surface, &instr, &out);
                println!("[quill] instruction OK ({} chars)", out.len());
            }
            return;
        }
    }

    // Outlook scar: an "empty" reply body that contains only the quoted thread (+signature space)
    // must compose a reply, not rewrite the quote — and must insert above it, not replace it.
    if let Some(quote) = quoted_reply_split(&value) {
        let quote = quote.to_string();
        handle_quoted_reply(quote, surface);
        return;
    }

    // The user TYPED (or quill drafted) a reply above the quote and pressed ⌥ to improve it.
    // Rewrite ONLY the draft. Preferred: AX-select exactly the draft region and type over it —
    // the signature + quote below stay PHYSICALLY untouched (retyping them was slow, alarming,
    // and flattened Outlook's rich formatting). Fallback (selection not honored by the app):
    // reconstruct the whole field.
    if let Some(idx) = quote_marker_idx(&value) {
        let (draft, sig) = split_trailing_signature(&value[..idx]);
        if !is_blank(draft) {
            let draft = draft.trim_end().to_string();
            let persona = persona_for(&surface);

            // ⌥ on quill's own UNEDITED draft = re-roll (a different variant), matching the
            // whole-field semantics — the plain reroll check can't fire here because the field
            // value includes signature+quote and never equals the bare draft.
            let reroll_prev = last_for(&surface)
                .filter(|(_, prev)| prev.trim() == draft.trim())
                .map(|(_, prev)| prev);

            let quote_text = value[idx..].to_string();
            // "// …" above a quote = explicit instruction executed with the thread as context.
            let explicit = explicit_instruction(&draft);
            let gen = || match (&explicit, &reroll_prev) {
                (Some(instr), _) => {
                    println!("[quill] explicit instruction (quoted field) in {surface}");
                    let ctx = format!("## The email thread below the cursor:\n{quote_text}");
                    crate::llm::email_from_seed(instr, &ctx, &persona)
                }
                // Unedited quill draft → regenerate a DIFFERENT variant grounded in the thread.
                (None, Some(prev)) => {
                    println!("[quill] re-roll (quoted field) in {surface}");
                    crate::llm::reroll("", &quote_text, &persona, prev, "Reply to this email")
                }
                (None, None) => crate::llm::polish_email_draft(&draft, &persona),
            };

            if crate::ax::select_char_range(0, draft.encode_utf16().count(), &draft) {
                // Precise path: the draft is now selected; the loader replaces IT only —
                // run_with_loader_typed never select-alls, so the tail stays untouched.
                println!(
                    "[quill] rewrite-above-quote (precise) in {surface} (draft {}b)",
                    draft.len()
                );
                if let Some(out) = run_with_loader_typed(&draft, gen) {
                    remember_output(&surface, "", &out);
                    println!("[quill] rewrite-above-quote OK (precise — tail untouched)");
                }
                return;
            }

            // Fallback: rebuild draft + signature + quote and retype the field.
            let tail = format!("{}{}", sig, &quote_text);
            println!(
                "[quill] rewrite-above-quote (reconstruct) in {surface} (draft {}b, tail {}b)",
                draft.len(),
                tail.len()
            );
            if let Some(out) =
                run_with_loader(&value, || gen().map(|o| format!("{}\n\n{}", o.trim(), tail.trim_start())))
            {
                remember_output(&surface, "", &out);
                println!("[quill] rewrite-above-quote OK (tail preserved)");
            }
            return;
        }
    }

    // NEW-MAIL signature handling (no quote marker, but the body carries the auto-inserted
    // signature): an "empty" new email is signature-only — compose and insert ABOVE it; a seed +
    // signature means polish/expand ONLY the seed. Without this, ⌥ on a fresh Outlook compose
    // rewrote the user's SIGNATURE (found by edge-case audit before it burned a demo).
    {
        let (draft, sig) = split_trailing_signature(&value);
        if !sig.trim().is_empty() {
            if is_blank(draft) {
                // Empty new mail: compose ABOUT the on-screen To/Subject, grounded in memory —
                // "write the update email about the project quill watched me build" is the
                // memory-native moment. Typed at the cursor above the signature.
                println!("[quill] new-mail compose in {surface} (signature preserved)");
                let persona = persona_for(&surface);
                if let Some(out) = run_with_loader_typed("", || {
                    let ctx = augment_context(read_context_lazy(), &surface);
                    crate::llm::email_compose(&ctx, &persona).and_then(|o| {
                        if crate::llm::smells_meta(&o) {
                            Err("model produced meta-commentary — dropped".into())
                        } else {
                            Ok(o)
                        }
                    })
                }) {
                    remember_output(&surface, "", &out);
                    println!("[quill] new-mail OK ({} chars, signature preserved)", out.len());
                }
                return;
            }
            // Seed + signature: the seed is either a rough draft (polish) or an INSTRUCTION
            // ("write about the changes in X since June") — the model decides, and instructions
            // execute AGAINST MEMORY: the seed leads the recall query, because "what changed in
            // the legal assistant since June 24" is exactly what the graph is for.
            let draft = draft.trim_end().to_string();
            if crate::ax::select_char_range(0, draft.encode_utf16().count(), &draft) {
                println!("[quill] new-mail seed (precise) in {surface} ({}b)", draft.len());
                let persona = persona_for(&surface);
                // "// …" prefix = explicit instruction; strip it (email_from_seed executes
                // instructions natively — the prefix just removes the draft-vs-instruction guess).
                let seed = explicit_instruction(&draft).unwrap_or_else(|| draft.clone());
                if let Some(out) = run_with_loader_typed(&draft, || {
                    // Seed FIRST in the query text so retrieval is about the user's ask,
                    // not the window chrome.
                    let combined = format!("{seed}\n\n{}", read_context_lazy());
                    let ctx = augment_context(combined, &surface);
                    crate::llm::email_from_seed(&seed, &ctx, &persona).and_then(|o| {
                        if crate::llm::smells_meta(&o) {
                            Err("model produced meta-commentary — dropped".into())
                        } else {
                            Ok(o)
                        }
                    })
                }) {
                    remember_output(&surface, "", &out);
                    println!("[quill] new-mail seed OK (precise — signature untouched)");
                }
                return;
            }
            // Selection refused → fall through to the generic paths below (whole-field rewrite
            // may touch the signature; rarer and visible, not silent corruption).
            println!("[quill] new-mail: precise selection refused — generic path");
        }
    }

    // Is the field GENUINE user text, or empty / a literal placeholder like "Add a reply..."?
    // is_blank misses placeholder TEXT (it's visible chars), which made a reply box try to
    // "rewrite" the placeholder and ask for content. looks_like_user_text knows both cases.
    let is_user_text = looks_like_user_text(&value);
    if is_user_text && !is_name_mention(&value) && quote_marker_idx(&value).is_none() {
        // Mode-1 input is the ONLY style signal (never quill's output) — never a bare @mention
        // name, and never a body containing a quoted email thread (that's not the user's voice).
        crate::style::record_user_writing(&surface, &value);
    }

    let persona = persona_for(&surface);
    // Mode 1 (user text) → rewrite/expand FAITHFULLY, NO thread context (it leaks unrelated names —
    // rewriting "thank you bro" pulled "Aman" from another comment). Mode 2 (empty/placeholder) →
    // compose a reply FROM the thread context, treating the placeholder text as empty.
    let input: &str = if is_user_text { value.as_str() } else { "" };

    // Mail compose (empty reply; quote & signature already ruled out above) is email-shaped, not a
    // bare chat line: this Apple Mail reply read empty (the quote lives in the WebView doc, not the
    // field value, so it never reached the quoted-reply path) and came out with no salutation. Route
    // it through email_reply for the salutation, and add a sign-off unless the field already carried
    // a client signature to close it.
    let mail_compose = is_mail_surface(&surface) && !is_user_text;
    let has_signature = !split_trailing_signature(&value).1.trim().is_empty();
    // Chat surface? (Teams &co). Both chat paths need the answer — empty box composes a
    // MESSAGE from the thread; typed text gets a faithful chat POLISH (the generic prompt's
    // "rewrite or expand" latitude continued a typed line into an invented reply — observed
    // live). Mail is checked first so mail surfaces never pay the anchors read.
    let is_chat = !is_mail_surface(&surface)
        && (is_chat_surface(&surface) || is_chat_domain());
    let chat_compose = !mail_compose && !is_user_text && is_chat;
    if chat_compose {
        println!("[quill] chat compose in {surface}");
    } else if is_chat && is_user_text {
        println!("[quill] chat polish in {surface} ({}b)", value.len());
    }

    // augment_context (relevance-ranked retrieval: embed + DB + ANN search) runs INSIDE the
    // loader closure, not before it — it can be slow (first-run model load, cold-start index
    // build), and the user must see the "Quilling…" loader immediately on press, not a frozen
    // field while retrieval runs silently.
    let gen = || {
        if mail_compose {
            // The visible thread IS the reply context here (no clean field-quote to hand over).
            let ctx = augment_context(read_context_lazy(), &surface);
            let body = crate::llm::email_reply(&ctx, "", &persona)?;
            return Ok(if has_signature { body } else { with_sign_off(body, &surface) });
        }
        if chat_compose {
            // The visible conversation IS the context — sanitized (Teams' AX duplication soup),
            // then TAIL-clipped (newest messages live at the END; the head is what a giant old
            // message may fill). The newest slice also leads the memory recall query, and the
            // ctx-tail log makes "what did the model actually see as newest" a one-glance check.
            let cleaned = clean_chat_context(&read_context_lazy());
            let thread = tail_chars(&cleaned, 5500);
            println!(
                "[quill] chat ctx: {} cleaned chars, tail: {:?}",
                cleaned.chars().count(),
                tail_chars(&thread, 90)
            );
            let ctx = match crate::retrieve::memory_block(&tail_chars(&thread, 1200), &surface) {
                Some(mem) => format!("{thread}\n\n{mem}"),
                None => thread,
            };
            let first = crate::llm::chat_reply(&ctx, &persona)?;
            if !echoes_thread(&first, &ctx) {
                return Ok(first);
            }
            // Corrective retry, house style (mirrors email_compose's meta-guard): show the
            // rejected echo and demand the NEXT message.
            println!("[quill] chat reply echoed the thread — corrective retry");
            let corrected = format!(
                "{ctx}\n\n## YOUR PREVIOUS ATTEMPT (REJECTED — it repeats a message that is \
already in the thread):\n{first}\n\nWrite the user's NEXT message: reply to the NEWEST \
messages at the very end of the thread. Never repeat an existing message."
            );
            let second = crate::llm::chat_reply(&corrected, &persona)?;
            return if echoes_thread(&second, &ctx) {
                Err("chat reply kept echoing the thread — dropped".into())
            } else {
                Ok(second)
            };
        }
        if is_chat && is_user_text {
            // Faithful in-place polish — same message, cleaner, never continued.
            return crate::llm::chat_polish(&value, &persona);
        }
        let ctx = if is_user_text {
            String::new()
        } else {
            // Lazy: the loader is showing — the window walk + retrieval are hidden latency now.
            augment_context(read_context_lazy(), &surface)
        };
        crate::llm::complete(input, &ctx, &persona, &placeholder)
    };

    // Mail-family surfaces: a synthetic ⌘A selects the WHOLE compose document (quote included —
    // observed live in Apple Mail: reply-from-scratch lost its quote to the loader's select-all).
    // Compose types at the cursor; rewrites replace exactly the typed text via VERIFIED AX
    // selection, or refuse rather than risk the document.
    let result = if is_mail_surface(&surface) {
        if is_user_text
            && !crate::ax::select_char_range(0, value.encode_utf16().count(), &value)
        {
            eprintln!(
                "[quill] mail surface: precise selection refused — aborting (never select-all in mail)"
            );
            return;
        }
        run_with_loader_typed(&value, gen)
    } else {
        run_with_loader(&value, gen)
    };

    if let Some(out) = result {
        remember_output(&surface, input, &out);
        println!("[quill] OK ({} chars)", out.len());
    }
}

/// Re-roll: regenerate a DIFFERENT variant from the ORIGINAL input, told to differ from the last
/// attempt. Does NOT re-record style (the input was already recorded on the first trigger).
fn handle_reroll(input: String, prev_output: String, surface: String, placeholder: String) {
    let persona = persona_for(&surface);
    let needs_context = is_blank(&input);
    // Same rule as handle_whole_field: retrieval runs inside the loader closure so the loader is
    // already showing before the (potentially slow) embed/DB/ANN work starts.
    if let Some(out) = run_with_loader(&prev_output, || {
        let ctx = if needs_context {
            augment_context(read_context_lazy(), &surface)
        } else {
            String::new() // re-rolling a rewrite stays faithful too — no thread context
        };
        crate::llm::reroll(&input, &ctx, &persona, &prev_output, &placeholder)
    }) {
        remember_output(&surface, &input, &out);
        println!("[quill] re-rolled ({} chars)", out.len());
    }
}

/// Directed edit: apply an ALL-CAPS instruction to quill's previous output (e.g. "MAKE IT
/// CONCISE"). Further caps-edits chain on the revised text.
fn handle_revise(prev: String, instruction: String, surface: String) {
    let persona = persona_for(&surface);
    if let Some(out) =
        run_with_loader(&prev, || crate::llm::revise(&prev, &instruction, &persona))
    {
        remember_output(&surface, &out, &out);
        println!("[quill] revised ({} chars)", out.len());
    }
}

fn handle_selection(original: String, surface: String) {
    let _guard = InjectGuard::new();
    let persona = persona_for(&surface);
    let context = read_context_lazy();
    // Selection stays highlighted as the "pending" cue; typing replaces it.
    match crate::llm::complete(&original, &context, &persona, "") {
        Ok(out) => {
            crate::inject::type_text(&out); // types over the still-active selection
            println!("[quill] OK (selection)");
        }
        Err(e) => eprintln!("[quill] FAILED (selection): {e}"),
    }
}

/// Install the global key tap on a background thread.
pub fn install() {
    std::thread::spawn(|| {
        let result = CGEventTap::with_enabled(
            CGEventTapLocation::Session,
            CGEventTapPlacement::HeadInsertEventTap,
            CGEventTapOptions::ListenOnly,
            vec![CGEventType::FlagsChanged, CGEventType::KeyDown],
            |_proxy, event_type, event| {
                match event_type {
                    CGEventType::KeyDown => {
                        if OPTION_DOWN.load(Ordering::SeqCst) {
                            CHORD.store(true, Ordering::SeqCst);
                        }
                    }
                    CGEventType::FlagsChanged => {
                        let keycode =
                            event.get_integer_value_field(EventField::KEYBOARD_EVENT_KEYCODE);
                        if keycode == KEYCODE_RIGHT_OPTION {
                            let alt_down = event
                                .get_flags()
                                .contains(CGEventFlags::CGEventFlagAlternate);
                            if alt_down {
                                OPTION_DOWN.store(true, Ordering::SeqCst);
                                CHORD.store(false, Ordering::SeqCst);
                            } else {
                                let was_chord = CHORD.swap(false, Ordering::SeqCst);
                                OPTION_DOWN.store(false, Ordering::SeqCst);
                                if !was_chord {
                                    println!("[quill] >>> trigger!");
                                    std::thread::spawn(handle_trigger);
                                }
                            }
                        }
                    }
                    _ => {}
                }
                CallbackResult::Keep
            },
            || {
                println!("[quill] event tap installed — press RIGHT Option (alone) to trigger.");
                CFRunLoop::run_current();
            },
        );

        if result.is_err() {
            eprintln!(
                "[quill] FAILED to create event tap. Grant Accessibility / Input Monitoring \
                 permission, then restart."
            );
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chat_surface_covers_teams_native_and_web_only() {
        // Native clients (new teams2 and classic share the prefix).
        assert!(is_chat_surface("com.microsoft.teams2"));
        assert!(is_chat_surface("com.microsoft.teams"));
        // Mail and browsers are NOT chat surfaces by bundle.
        assert!(!is_chat_surface("com.microsoft.Outlook"));
        assert!(!is_chat_surface("ai.perplexity.comet"));
        // Web Teams (any browser) is recognized by domain, subdomains included.
        assert!(domain_is_chat("teams.microsoft.com"));
        assert!(domain_is_chat("teams.live.com"));
        assert!(domain_is_chat("emea.teams.microsoft.com"));
        // Near-misses must not fire.
        assert!(!domain_is_chat("linkedin.com"));
        assert!(!domain_is_chat("myteams.live.com"));
        assert!(!domain_is_chat("teams.microsoft.com.evil.example"));
    }

    #[test]
    fn chat_context_collapses_teams_duplication_soup() {
        // Fixture modeled on a Teams window: every message
        // arrives up to 4× (attributed, timestamped aria-label, bare) plus chrome rows. The
        // duplication can bias the model into echoing the user's own message.
        let raw = "Message List\n\
isme mention karo  by Robin Chen\n\
25 June 2026 12:27 pm.\n\
25/06 12:27 pm\n\
isme mention karo Robin Chen 25 June 2026 12:27 pm.\n\
More message options\n\
isme mention karo\n\
can we host this on our own GPU? by Jordan Lee Rivera\n\
Yesterday 2:40 pm\n\
can we host this on our own GPU? Jordan Lee Rivera Yesterday at 2:40 pm.\n\
More message options\n\
can we host this on our own GPU?\n\
ek baar check karo  by Robin Chen\n\
Yesterday 2:41 pm\n\
Seen\n\
More message options\n\
ek baar check karo\n\
Send (⌘ Return)\n";
        let cleaned = clean_chat_context(raw);
        // Each message survives exactly once, in its attributed form; chrome/timestamps gone.
        assert_eq!(cleaned.matches("isme mention karo").count(), 1);
        assert_eq!(cleaned.matches("can we host this on our own GPU?").count(), 1);
        assert!(cleaned.contains("ek baar check karo  by Robin Chen"));
        assert!(!cleaned.contains("More message options"));
        assert!(!cleaned.contains("Message List"));
        assert!(!cleaned.contains("12:27"));
        assert!(!cleaned.contains("Send ("));
        // Distinct short messages must NOT be eaten by the dedup ("ok" vs "ok thanks…").
        let two = clean_chat_context("ok  by Robin Chen\nok thanks, is this hosted somewhere? by Jordan Lee Rivera\n");
        assert!(two.contains("ok  by Robin Chen"));
        assert!(two.contains("ok thanks, is this hosted somewhere?"));
    }

    #[test]
    fn echo_guard_catches_thread_repeats_but_not_real_replies() {
        let ctx = "can we host this on our own GPU? by Jordan Lee Rivera\n\
ek baar check karo  by Robin Chen\n";
        // The failure mode: the output was a near-copy of the user's own earlier message.
        assert!(echoes_thread("can we host it on our own GPU?", ctx));
        // A genuine next message sails through.
        assert!(!echoes_thread("Sure — I'll check once and confirm today.", ctx));
    }

    // word_overlap moved to util.rs (shared with capture's dwell-merge) — tested there.

    /// Serializes tests that mutate the global user_name (parallel test threads would race).
    static NAME_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn trailing_signature_splits_draft_from_sig() {
        let _l = NAME_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        crate::settings::set_user_name("Jordan Lee Rivera".to_string());
        // Draft + closing + signature (the live Outlook shape).
        let pre = "Noted — I'll verify the updated timeline and confirm by EOD.\n\nBest regards,\n\nJordan Lee Rivera\nProduct Lead | Acme Advanced Solutions";
        let (draft, sig) = split_trailing_signature(pre);
        assert!(draft.trim().starts_with("Noted"));
        assert!(draft.trim().ends_with("EOD."));
        assert!(sig.contains("Best regards,"), "closing line belongs to the signature");
        assert!(sig.contains("Acme"));
        // No signature present → everything is draft.
        let (d2, s2) = split_trailing_signature("just a draft, no sig");
        assert_eq!(d2, "just a draft, no sig");
        assert_eq!(s2, "");
        crate::settings::set_user_name(String::new());
    }

    #[test]
    fn loader_remnants_are_recognized() {
        assert!(is_loader_remnant("..."));
        assert!(is_loader_remnant("Quilling"));
        assert!(is_loader_remnant("Quilling.."));
        assert!(is_loader_remnant("  .  "));
        assert!(!is_loader_remnant(""));
        assert!(!is_loader_remnant("real draft text"));
        assert!(!is_loader_remnant("......."));  // >5 dots = probably intentional typing
        assert!(!is_loader_remnant("Quilling is a hobby")); // real sentence
    }

    #[test]
    fn quoted_reply_detects_empty_reply_over_quote() {
        // Classic Outlook header block below an empty cursor area.
        let v = "\n\nFrom: Alex Morgan <alex@example.com>\nSent: Wednesday, July 2\nTo: Jordan\nSubject: Q3 report\n\nPlease share the testing status.";
        assert!(quoted_reply_split(v).is_some());
        assert!(quoted_reply_split(v).unwrap().starts_with("From: Alex"));

        // Outlook separator line variant.
        assert!(quoted_reply_split("\n________________________________\nFrom: A\nTo: B\nhello").is_some());

        // Original Message variant.
        assert!(quoted_reply_split("\n-----Original Message-----\nFrom: X\nbody").is_some());

        // Gmail/Apple "On … wrote:" variant.
        assert!(quoted_reply_split("\nOn Jul 2, 2026, Alex wrote:\n> please test").is_some());

        // Apple Mail forward marker.
        assert!(quoted_reply_split("\n\nBegin forwarded message:\nFrom: X\nbody").is_some());

        // Universal ">"-quoted block (two consecutive quoted lines).
        assert!(quoted_reply_split("\n\n> first quoted line\n> second quoted line\n").is_some());
        // A single ">" inline is NOT a quote block (could be a typed comparison).
        assert!(quoted_reply_split("count > 3 is fine here").is_none());

        // The user actually TYPED above the quote → a real draft, not an empty reply.
        assert!(quoted_reply_split("Thanks, will do!\n\nFrom: Alex\nSent: today\nbody").is_none());

        // SIGNATURE above the quote (the live Outlook case): counts as an empty reply.
        let _l = NAME_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        crate::settings::set_user_name("Jordan Lee Rivera".to_string());
        let sig = "Jordan Lee Rivera\nProduct Lead | Acme Advanced Solutions\nemail: jordan@example.com\n\nFrom: Sam Taylor <sam@example.net>\nDate: Thursday\nTo: Team\nSubject: Vendor onboarding\n\nDear Team, the timeline has changed.";
        let q = quoted_reply_split(sig);
        assert!(q.is_some(), "signature-above-quote must count as empty reply");
        assert!(q.unwrap().starts_with("From: Sam"));
        // But typed text that does NOT start with the user's name stays a draft.
        assert!(quoted_reply_split("Noted, thanks!\n\nFrom: Sam T\nTo: Team\nbody").is_none());
        crate::settings::set_user_name(String::new());

        // Plain drafts with no quote markers are untouched.
        assert!(quoted_reply_split("just a normal draft about the From line of a form").is_none());
        assert!(quoted_reply_split("").is_none());
    }

    #[test]
    fn name_mention_handles_real_world_names() {
        // Lowercase later words in a real name.
        assert!(is_name_mention("Fernandez Marco del rio"));
        assert!(is_name_mention("Lin Wei"));
        assert!(is_name_mention("CARLO VEGA REYES"));
        assert!(is_name_mention("Adrian Vale, PhD, DBA"));
        assert!(is_name_mention("AI For Search"));
        // Caseless scripts: Arabic names have no upper/lowercase — must still detect.
        assert!(is_name_mention("سمير حسن"));
        assert!(is_name_mention("ليلى قاسم"));
        // Typed text must NOT be mistaken for a name.
        assert!(!is_name_mention("Thanks for sharing"));
        assert!(!is_name_mention("great work man really needed ths"));
        assert!(!is_name_mention("ok"));
        assert!(!is_name_mention("This is a full sentence."));
    }

    #[test]
    fn blank_detects_invisible_only_fields() {
        assert!(is_blank(""));
        assert!(is_blank("   \n\t"));
        // Discord's "empty" Slate editor = zero-width / format chars, not a zero count.
        assert!(is_blank("\u{200B}\u{FEFF}\u{2060}"));
        assert!(!is_blank("hi"));
        assert!(!is_blank("  x  "));
    }

    #[test]
    fn user_text_excludes_blanks_and_placeholders() {
        assert!(looks_like_user_text("I went to the market and bought apples"));
        assert!(!looks_like_user_text("")); // blank
        assert!(!looks_like_user_text("\u{200B}\u{FEFF}")); // invisible-only
        assert!(!looks_like_user_text("Add a comment")); // placeholder
        assert!(!looks_like_user_text("Reply")); // placeholder
    }

    #[test]
    fn name_mention_detects_prefilled_tags_not_messages() {
        assert!(is_name_mention("CARLO VEGA REYES")); // LinkedIn reply tag (all caps)
        assert!(is_name_mention("Carlo Vega Reyes")); // title case
        assert!(is_name_mention("Adrian Vale, PhD, DBA")); // commas + credentials
        assert!(is_name_mention("Dr. Adrian Vale, PhD, DBA")); // honorific + credentials
        assert!(!is_name_mention("thank you bro")); // lowercase message
        assert!(!is_name_mention("ai is doing a great job")); // a statement
        assert!(!is_name_mention("Thanks")); // single word — not a mention
        assert!(!is_name_mention("")); // empty
    }

    #[test]
    fn caps_instruction_detects_replaced_and_appended() {
        let prev = "AI is doing a great job in many fields.";
        // replaced: field is just the caps instruction
        assert_eq!(
            caps_instruction("MAKE IT CONCISE", prev).as_deref(),
            Some("MAKE IT CONCISE")
        );
        // appended after the previous output
        assert_eq!(
            caps_instruction(&format!("{prev} MAKE IT SHORTER"), prev).as_deref(),
            Some("MAKE IT SHORTER")
        );
        // a normal (mixed-case) draft is NOT an instruction
        assert_eq!(caps_instruction("make it concise", prev), None);
        assert_eq!(caps_instruction(prev, prev), None);
    }

}
