// Inline rewrite / draft + style distillation via a self-hosted, OpenAI-compatible LLM.
// Config from env (loaded from src-tauri/.env via dotenvy):
//   QUILL_LLM_URL, QUILL_LLM_KEY, QUILL_LLM_MODEL

use serde_json::json;

const SYSTEM_PROMPT: &str = "You are a writing assistant embedded in the user's apps. You see the \
on-screen context (often a raw UI dump — focus on the real content and IGNORE menus, usernames, \
timestamps, reactions, ads and button labels), the current text-field contents, and the field's \
PLACEHOLDER, which hints at the surface (e.g. 'Add a comment', 'Start a post', 'Message #channel', \
'Write a reply').\n\n\
Decide what to do:\n\
1. POST / ARTICLE composer (placeholder like 'Start a post', 'What do you want to talk about', \
'Share') AND the field holds a brief idea, topic or instruction (e.g. 'ai is doing a great job', \
'write a post about X') → EXPAND it into a complete, well-structured post in the user's voice. Do \
NOT merely fix the wording — write the full post they are gesturing at.\n\
2. COMMENT on a post (placeholder like 'Add a comment') with an empty/placeholder field → write the \
user's comment as a reply to the MAIN POST's content. Do NOT reply to other people's comments \
unless the user is clearly addressing a specific person.\n\
3. The field holds the user's own FINISHED draft (not a brief seed) → improve it: grammar, spelling, \
punctuation, structure, formatting and clarity, keeping their meaning, language and casual/formal \
voice. Preserve EXACTLY who and what they wrote — do NOT add names, @mentions, recipients or facts \
that are not already in their text. Your output must be a REAL improvement, never the input echoed \
back: fix telegraphic shorthand into complete, natural sentences ('great work man really needed ths' \
→ a proper, warm sentence), correct typos, and tighten phrasing — while keeping it the same length \
class (a short comment stays a short comment, not an essay).\n\
4. Empty/placeholder chat or message field → write the user's NEXT MESSAGE as a natural reply to the \
MOST RECENT messages.\n\n\
Match the surface's tone and length; use an emoji only if it fits. Anything under 'recent activity \
in other apps' is background. NEVER ask the user what they want, NEVER offer options, NEVER explain \
yourself. Even if the context is thin, NEVER ask for content or say you need more — write a brief, \
friendly reply appropriate to what's visible (e.g. thank someone for a compliment). Output ONLY the \
final text, ready to send — no preamble, no quotes, no labels, no meta-commentary.";

/// One chat completion against the configured LLM.
pub(crate) fn chat(system: &str, user: &str, temperature: f64) -> Result<String, String> {
    let url = std::env::var("QUILL_LLM_URL").map_err(|_| "QUILL_LLM_URL not set".to_string())?;
    let key = std::env::var("QUILL_LLM_KEY").map_err(|_| "QUILL_LLM_KEY not set".to_string())?;
    let model = std::env::var("QUILL_LLM_MODEL").map_err(|_| "QUILL_LLM_MODEL not set".to_string())?;

    let body = json!({
        "model": model,
        "messages": [
            { "role": "system", "content": system },
            { "role": "user", "content": user }
        ],
        "temperature": temperature
    });

    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|e| format!("client build: {e}"))?;

    let resp = client
        .post(&url)
        .header("Authorization", format!("Bearer {key}"))
        .json(&body)
        .send()
        .map_err(|e| format!("request: {e}"))?;

    let status = resp.status();
    let val: serde_json::Value = resp.json().map_err(|e| format!("bad json: {e}"))?;
    if !status.is_success() {
        return Err(format!("LLM {status}: {val}"));
    }
    let out = val["choices"][0]["message"]["content"]
        .as_str()
        .ok_or_else(|| format!("no content in response: {val}"))?
        .trim()
        .to_string();
    Ok(out)
}

/// `input` = the user's current draft / selection (may be empty).
/// `context` = surrounding on-screen text (may be empty).
/// `style` = learned per-app style bullets to match (may be empty).
pub fn complete(input: &str, context: &str, persona: &str, placeholder: &str) -> Result<String, String> {
    let ctx = context.trim();
    let ph = placeholder.trim();
    let mut user_msg = String::new();
    if !ph.is_empty() {
        user_msg.push_str(&format!("## Field placeholder (surface hint): {ph}\n"));
    }
    if !ctx.is_empty() {
        user_msg.push_str(&format!(
            "## On-screen context (reply to the MAIN content, not other commenters):\n{ctx}\n\n"
        ));
    }
    user_msg.push_str(&format!("## The text field currently contains:\n{input}"));

    let mut system = SYSTEM_PROMPT.to_string();
    if !persona.trim().is_empty() {
        system.push_str("\n\n");
        system.push_str(persona.trim());
    }

    chat(&system, &user_msg, 0.55)
}

/// Like `complete`, but asked to produce a DIFFERENT variant than `previous` (for re-rolls):
/// higher temperature + an explicit "give a fresh alternative" instruction → genuine variation.
pub fn reroll(
    input: &str,
    context: &str,
    persona: &str,
    previous: &str,
    placeholder: &str,
) -> Result<String, String> {
    let ctx = context.trim();
    let ph = placeholder.trim();
    let mut base = String::new();
    if !ph.is_empty() {
        base.push_str(&format!("## Field placeholder (surface hint): {ph}\n"));
    }
    if !ctx.is_empty() {
        base.push_str(&format!("## On-screen context:\n{ctx}\n\n"));
    }
    base.push_str(&format!("## The text field currently contains:\n{input}"));
    let user_msg = format!(
        "{base}\n\n## Your previous attempt (the user wants a DIFFERENT option):\n{previous}\n\n\
Write a fresh alternative with the SAME intent and meaning but noticeably different wording, \
structure or angle. Do NOT repeat the previous attempt. Output ONLY the final message text."
    );

    let mut system = SYSTEM_PROMPT.to_string();
    if !persona.trim().is_empty() {
        system.push_str("\n\n");
        system.push_str(persona.trim());
    }

    chat(&system, &user_msg, 0.9)
}

/// Apply an explicit instruction (e.g. "MAKE IT CONCISE") to revise the previous output, keeping
/// the user's voice. Drives the CAPS-instruction directed edit.
pub fn revise(text: &str, instruction: &str, persona: &str) -> Result<String, String> {
    let user_msg = format!("## Text:\n{text}\n\n## Apply this instruction exactly:\n{instruction}");
    let mut system = String::from(
        "You revise the user's text per their explicit instruction, keeping their voice and intent. \
Output ONLY the revised text — no preamble, no quotes, no commentary.",
    );
    if !persona.trim().is_empty() {
        system.push_str("\n\n");
        system.push_str(persona.trim());
    }
    chat(&system, &user_msg, 0.45)
}

/// Write the USER's reply to ONE specific person's comment in a thread. `target` is the pre-filled
/// @mention (LinkedIn tells us who) — find THEIR comment in the thread and reply to what THEY said,
/// ignoring other commenters. The @mention is already in the field, so don't repeat it.
pub fn reply_to(target: &str, context: &str, persona: &str) -> Result<String, String> {
    let mut system = String::from(
        "You write the USER's reply to ONE specific person's comment in a thread. You're given the \
TARGET person and the full thread. Find the TARGET's OWN comment and reply specifically to what \
THEY said — IGNORE other people's comments. Be concise, natural and substantive (1-3 sentences); \
match the user's voice and length; avoid corporate buzzwords and clichés. Their @mention is already \
in the field, so do NOT begin with their name. Output ONLY the reply text — no preamble, quotes or \
labels.",
    );
    if !persona.trim().is_empty() {
        system.push_str("\n\n");
        system.push_str(persona.trim());
    }
    let user = format!(
        "## You are replying to this person's comment: {target}\n\n## Thread:\n{}",
        context.trim()
    );
    chat(&system, &user, 0.5)
}

/// Write the USER's reply to an email thread. Email-shaped rules — salutation, body, and
/// CRITICALLY no sign-off: the client auto-appends the user's signature below the insertion
/// point (observed live: the model added 'Best regards + name block' and the signature appeared
/// twice).
pub fn email_reply(quote: &str, background: &str, persona: &str) -> Result<String, String> {
    let mut system = String::from(
        "You write the USER's reply to the email thread below, ready to send.\n\
Rules:\n\
1. OPEN with a salutation to the person who wrote the message being replied to — take their \
name from the From:/wrote: line and use its natural short form ('Dear Sam,' in formal threads, \
'Hi Sam,' in casual ones; 'Dear Team,' when it addresses a group). Normalize the name: proper \
capitalization and spacing — a handle or glued form like 'jefreyryan' becomes 'Jefrey'; never \
reproduce usernames or email addresses as names.\n\
2. Write a COMPLETE reply that SPECIFICALLY addresses every point, question and request in the \
message being replied to — acknowledge what they told you (referencing the actual items: the \
system, the date, the document), answer what they asked, and state your next step or commitment \
where one is implied. A proper body is usually 2–6 sentences in 1–2 short paragraphs; never a \
one-line brush-off unless the thread is trivially transactional. The user refines tone/length \
afterwards with follow-up instructions — your job is the complete first draft.\n\
3. You speak ONLY for the user, first person singular. When the email asks a GROUP a question \
('who did X?', 'can someone…'), answer solely for the user personally ('I have not performed \
this change') — NEVER volunteer answers, actions or commitments on behalf of teammates or 'the \
team', and NEVER promise follow-ups ('I will check and revert') the user hasn't chosen to make. \
If the user's own position isn't known, keep their part neutral rather than inventing one.\n\
4. UNDERSTAND the thread before answering — every statement must be grounded in what it actually \
says. If something you need is AMBIGUOUS in the thread (which system, which date, what exactly \
they're asking), the reply should briefly ASK the sender to clarify rather than guess. If a point \
depends on knowledge ONLY the user has (did they do something? do they agree?), NEVER assert it \
as fact. Instead put an either/or placeholder exactly where the answer goes — at most ONE per \
email, inline, never a question, never alongside an assertion of the same fact. \
Right: 'I have [completed / not yet completed] the declaration form.' \
Wrong: '[confirm: have you submitted?] I have completed the form.' (question + contradicting claim).\n\
5. Match the thread's language and formality; quote names, dates and facts accurately; NEVER \
invent facts or commitments the user hasn't made.\n\
6. END after the final body sentence. Do NOT write ANY closing or signature — no 'Best regards', \
no 'Thanks', no name, no title — the email client automatically appends the user's signature \
block right below where this text is inserted.\n\
Output ONLY the reply text.",
    );
    if !persona.trim().is_empty() {
        system.push_str("\n\n");
        system.push_str(persona.trim());
    }
    let mut user = format!("## The email thread being replied to:\n{}", quote.trim());
    let bg = background.trim();
    if !bg.is_empty() {
        // The QUOTE is the primary context; background (window dump + memory) is garnish. Outlook
        // window reads hit 39KB live (message list + reading pane) — clip hard so the prompt stays
        // focused and fast.
        let clipped: String = bg.chars().take(4000).collect();
        user.push_str(&format!(
            "\n\n## Background from the user's other activity (context only):\n{clipped}"
        ));
    }
    chat(&system, &user, 0.5)
}

/// Compose a NEW email (no thread) from the on-screen To/Subject plus the user's memory.
/// Dedicated prompt: feeding the reply-shaped prompt a placeholder "no thread" note made the
/// model ask for the missing thread INSIDE the email body (observed live).
pub fn email_compose(window_and_memory: &str, persona: &str) -> Result<String, String> {
    let mut system = String::from(
        "You write a NEW email FROM the user, ready to send.\n\
Rules:\n\
1. The compose window below shows the recipient (To:) and Subject — open with a fitting \
salutation to that recipient and write ABOUT that subject.\n\
2. Ground the content in the user's memory/background below — what they have actually been \
working on and know. Where a key specific is missing (a date, a status, a number), write the \
email anyway and mark the gap with a short bracketed placeholder like [current status] for the \
user to fill.\n\
3. You speak ONLY for the user, first person singular; never invent facts, statuses or \
commitments.\n\
4. NEVER write meta-commentary, never mention prompts, sections, missing input or being an \
assistant — if you know little, write a brief, plausible skeleton with placeholders instead. \
Example with thin input (To: Saleh, Subject: Project Updates):\n\
'Dear Saleh,\n\nI wanted to share a quick update on [project name]. [Current status in one \
sentence]. The next step is [next step], which I expect to complete by [date].'\n\
5. END after the final body sentence — no closing, no name, no signature (the client appends it).\n\
Output ONLY the email body text.",
    );
    if !persona.trim().is_empty() {
        system.push_str("\n\n");
        system.push_str(persona.trim());
    }
    let clipped: String = window_and_memory.trim().chars().take(6000).collect();
    let user = format!("## Compose window + the user's memory/background:\n{clipped}");
    let first = chat(&system, &user, 0.5)?;
    if !smells_meta(&first) {
        return Ok(first);
    }
    // Stubborn-model retry: one corrective pass (observed live — the model asks for the missing
    // thread instead of writing the skeleton, despite the rule + example).
    let corrective = format!(
        "{user}\n\n## YOUR PREVIOUS ATTEMPT (REJECTED — this is meta-commentary, which is \
forbidden):\n{first}\n\nWrite the ACTUAL EMAIL BODY now. Use [bracketed placeholders] for \
anything you don't know. Do not describe what you need."
    );
    chat(&system, &corrective, 0.6)
}

/// A seed typed into a NEW email is either a rough draft (polish it) or an INSTRUCTION
/// ("write about the changes we did in the legal assistant since 24 June" — observed live being
/// grammar-polished instead of executed). The model decides which, and instructions are executed
/// AGAINST THE USER'S MEMORY.
pub fn email_from_seed(seed: &str, window_and_memory: &str, persona: &str) -> Result<String, String> {
    let mut system = String::from(
        "The user typed a SEED into a new email's body. Decide what it is:\n\
A) An INSTRUCTION describing the email to write ('ask alex for the status update', 'write about \
the changes we did in X since June') → WRITE THAT EMAIL: open with a salutation to the To: \
recipient, and build the content from the user's memory/background below — cite the actual \
items found there (changes, dates, systems). Where memory lacks a specific, put an either/or or value placeholder inline where the answer \
goes — at most one per email, never a question (right: 'on [date]'; wrong: '[confirm: when?]').\n\
B) A rough DRAFT of the email itself → polish it (casing, punctuation, phrasing), same meaning, \
no new facts.\n\
Always: first person singular, match formality, NEVER invent facts or commitments, NEVER write \
meta-commentary or mention prompts/missing input — write the email. END after the final body \
sentence: no closing, no name, no signature (the client appends it). Output ONLY the email body.",
    );
    if !persona.trim().is_empty() {
        system.push_str("\n\n");
        system.push_str(persona.trim());
    }
    let clipped: String = window_and_memory.trim().chars().take(6000).collect();
    let user = format!(
        "## The seed the user typed:\n{}\n\n## Compose window + the user's memory/background:\n{clipped}",
        seed.trim()
    );
    let first = chat(&system, &user, 0.5)?;
    if !smells_meta(&first) {
        return Ok(first);
    }
    let corrective = format!(
        "{user}\n\n## YOUR PREVIOUS ATTEMPT (REJECTED — meta-commentary is forbidden):\n{first}\n\n\
Write the ACTUAL EMAIL BODY now. Use [bracketed placeholders] for anything you don't know."
    );
    chat(&system, &corrective, 0.6)
}

/// Write the USER's next message in a CHAT thread (Teams & co) — message-shaped, never
/// email-shaped. Dedicated prompt: chat routed through the generic complete() has no chat
/// rules at all, and email_reply would open with a salutation and letter-shaped paragraphs
/// inside a message box. Carries the same live-earned rules as email: first-person-only
/// agency, understand-first, either/or placeholders, never inventing facts.
pub fn chat_reply(thread_and_memory: &str, persona: &str) -> Result<String, String> {
    let mut system = String::from(
        "You write the USER's next message in the chat conversation below, ready to send.\n\
Rules:\n\
1. This is a CHAT message, not an email: NO salutation line, NO sign-off, NO signature — just \
the message itself. Concise by default: 1–4 sentences unless the thread clearly needs more.\n\
2. Reply to the most recent message(s) addressed to the user — answer every question directed \
at them SPECIFICALLY (name the actual items: the system, the date, the file), never a generic \
acknowledgement. The thread includes the user's OWN earlier messages under their name: NEVER \
repeat or rephrase ANY message already in the thread — write what comes NEXT.\n\
3. You speak ONLY for the user, first person singular. When the chat asks a GROUP something, \
answer solely for the user personally — NEVER volunteer actions or commitments for teammates or \
'the team', and NEVER promise follow-ups the user hasn't chosen to make.\n\
4. UNDERSTAND the thread before answering. If the ask is ambiguous (which system? which date?), \
the best reply IS a short clarifying question. If a point depends on knowledge only the user \
has, use at most ONE inline either/or placeholder ('I have [completed / not yet completed] the \
review') — never assert it as fact, never phrase the placeholder as a question.\n\
5. Match the thread's language, tone and formality — casual threads get casual replies; use \
names, dates and facts exactly as the thread states them; NEVER invent any.\n\
Output ONLY the message text.",
    );
    if !persona.trim().is_empty() {
        system.push_str("\n\n");
        system.push_str(persona.trim());
    }
    // Clip from the TAIL: newest messages (and the appended memory block) live at the end —
    // a head-first clip silently fed the model only the oldest half of long threads.
    let t = thread_and_memory.trim();
    let n = t.chars().count();
    let clipped: String =
        if n <= 8000 { t.to_string() } else { t.chars().skip(n - 8000).collect() };
    let user = format!(
        "## The chat conversation (the newest messages are nearest the end) + background:\n{clipped}"
    );
    chat(&system, &user, 0.5)
}

/// Polish the user's TYPED chat message in place — same message, cleaner. Distinct from the
/// generic rewrite: the generic prompt's "rewrite or expand" latitude let the model continue a
/// typed chat line into an invented reply-commitment ("Theek hai, check karta hoon agar access
/// mil sake") — observed live in Teams. A chat polish NEVER adds content.
pub fn chat_polish(text: &str, persona: &str) -> Result<String, String> {
    let mut system = String::from(
        "Polish the USER's chat message below: fix casing, spelling and punctuation, smooth the \
phrasing. Rules:\n\
1. SAME message, SAME meaning, roughly the SAME length — NEVER continue it, answer it, or add \
content, commitments or questions that are not already in it.\n\
2. Keep the language and its mix exactly as written (Hinglish stays Hinglish — never translate).\n\
3. Keep names, product names and technical terms exactly as the user wrote them unless the fix \
is an unambiguous typo.\n\
4. No salutation, no sign-off — it is a chat message.\n\
Output ONLY the polished message.",
    );
    if !persona.trim().is_empty() {
        system.push_str("\n\n");
        system.push_str(persona.trim());
    }
    let user = format!("## The user's message:\n{}", text.trim());
    chat(&system, &user, 0.4)
}

/// Meta-leak detector: model output that talks ABOUT the task instead of doing it must never be
/// typed into a real field (observed live: 'Please provide the email content you wish me to reply
/// to' typed into a client email).
pub fn smells_meta(out: &str) -> bool {
    let l = out.to_lowercase();
    ["please provide", "your prompt", "the email thread being replied", "as an ai",
     "i cannot", "i'm unable", "email content you wish", "section in your prompt"]
    .iter()
    .any(|m| l.contains(m))
}

/// Phase 2 (activity digests): distill a surface's recent RAW captures into a few CLEAN factual
/// sentences for the memory graph. The LLM extraction IS the cleaning — it keeps people / orgs /
/// projects / tools / files / decisions / commitments and DROPS UI chrome, nav, and boilerplate.
/// Returns Err (SKIP) when there's nothing worth remembering, so trivial activity never enters
/// the graph. Runs as a batched consolidation pass, with cognee as the graph store.
pub fn digest_activity(surface: &str, context: &str) -> Result<String, String> {
    const SYSTEM: &str = "You are a memory extractor. From the raw on-screen text below (captured \
while the user worked), write 3-8 SHORT factual sentences capturing what matters for long-term \
memory: people, organizations, projects, tools, files, decisions and commitments — use full \
names. IGNORE all UI chrome, navigation, menus, buttons, ads and boilerplate. Do NOT invent \
anything not present in the text. If there is nothing worth remembering, reply with exactly SKIP.";
    let user = format!("## Surface: {surface}\n## Raw captured text:\n{context}");
    let out = chat(SYSTEM, &user, 0.3)?;
    let s = out.trim();
    if s.is_empty() || s.eq_ignore_ascii_case("skip") || smells_meta(s) {
        return Err("nothing worth remembering".to_string());
    }
    Ok(s.to_string())
}

/// Polish a rough email draft body. Dedicated prompt because the generic rewrite ECHOES tiny
/// telegraphic drafts back ("noted ankit. best regards..." returned byte-identical, observed live).
pub fn polish_email_draft(draft: &str, persona: &str) -> Result<String, String> {
    let mut system = String::from(
        "You polish the body of an email the USER drafted. Fix capitalization, punctuation, \
spelling and phrasing into natural professional prose; keep their meaning, brevity and warmth. \
Names get proper case ('ankit' → 'Ankit'). If the draft includes an inline closing with the \
user's name, keep it, properly cased. NEVER return the draft unchanged — at minimum fix the \
casing and punctuation. Do NOT add new facts, names or sentences. Output ONLY the polished body.",
    );
    if !persona.trim().is_empty() {
        system.push_str("\n\n");
        system.push_str(persona.trim());
    }
    chat(&system, &format!("## Draft:\n{}", draft.trim()), 0.4)
}

const CHAT_SYSTEM: &str = "You are quill, the user's ambient assistant. Answer the user's question \
concisely and directly, using their RECENT ACTIVITY (captured from their screen) when relevant. If \
you lack the context to answer, say so briefly instead of guessing. No preamble, no fluff.";

/// Answer a free-form panel-chat question, grounded in recent captured activity.
pub fn answer(question: &str, context: &str, persona: &str) -> Result<String, String> {
    let mut system = CHAT_SYSTEM.to_string();
    if !persona.trim().is_empty() {
        system.push('\n');
        system.push_str(persona.trim());
    }
    let user = if context.trim().is_empty() {
        question.to_string()
    } else {
        format!("## My recent activity (context):\n{context}\n\n## My question:\n{question}")
    };
    chat(&system, &user, 0.4)
}

const STYLE_SYSTEM: &str = "You study how one specific person writes and summarise their style. \
Given example messages they wrote in a single app, output 2-4 SHORT rules describing their tone, \
energy, formality, length, phrasing and emoji use. Be concrete and specific to these samples. \
VOICE ONLY — never prescribe mechanical sloppiness as style: do NOT output rules about typos, \
misspellings, missing capitalization or missing punctuation (drafts always use standard \
capitalization and correct spelling; the user types fast, that is not their voice). \
Describe emoji FREQUENCY and placement, never specific emojis (naming '🚀' turns it into a \
whitelist the writer repeats forever). \
Output ONLY a bullet list, one rule per line starting with '- ', no preamble.";

/// Strip a leading bullet (-,*,•) and/or numeric enumerator ("1." / "2)") from a line.
fn clean_bullet(line: &str) -> String {
    let s = line.trim().trim_start_matches(['-', '*', '•']).trim();
    let bytes = s.as_bytes();
    let digits = bytes.iter().take_while(|b| b.is_ascii_digit()).count();
    if digits > 0 && matches!(bytes.get(digits), Some(b'.') | Some(b')')) {
        s[digits + 1..].trim().to_string()
    } else {
        s.to_string()
    }
}

/// Distil 2-4 style bullets from the user's own writing samples for a surface.
pub fn distill_style(surface: &str, samples: &[String]) -> Result<Vec<String>, String> {
    let joined: String = samples
        .iter()
        .map(|s| format!("- {}", s.trim().replace('\n', " ")))
        .collect::<Vec<_>>()
        .join("\n");
    let user = format!("App: {surface}\nMessages this person wrote here:\n{joined}\n\nTheir writing style:");

    let out = chat(STYLE_SYSTEM, &user, 0.3)?;
    let bullets: Vec<String> = out
        .lines()
        .map(clean_bullet)
        .filter(|l| l.chars().count() > 2)
        .take(4)
        .collect();
    Ok(bullets)
}
