//! Telegram bot bridge. The overlay talks to api.telegram.org directly: it sends banners to
//! the user's own bot chat (with inline buttons for permissions and options) and long-polls
//! getUpdates so a tap or a typed reply answers the pending card. No server, no open port.
//!
//! Only the paired private chat is obeyed, and only messages and taps of the user it belongs
//! to. A chat pairs itself by sending a one-time code that the PC shows.
use crate::{config, events, util, Shared};
use serde_json::{json, Value};
use std::collections::{BTreeSet, HashSet};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tauri::AppHandle;

/// Longest message text we send; Telegram allows 4096 characters after parsing.
const MAX_HTML: usize = 3500;
/// Pairing codes use characters that cannot be mistaken for one another (no 0/O, 1/I/L).
const CODE_ALPHABET: &[u8] = b"23456789ABCDEFGHJKMNPQRSTUVWXYZ";
const CODE_LEN: usize = 8;
/// Wrong codes allowed before the code stops working (a new one comes from the tray).
const MAX_WRONG_CODES: u32 = 10;

/// Bumped by Reload config: a token Telegram rejected is tried again after it.
static RELOADS: AtomicU64 = AtomicU64::new(0);
/// Last 409 Conflict (unix ms): something else polls the same bot.
static LAST_CONFLICT_MS: AtomicI64 = AtomicI64::new(0);
/// Last poll error logged, and when (unix ms): the same error is logged once per 10 minutes.
static LAST_POLL_ERROR: Mutex<(String, i64)> = Mutex::new((String::new(), 0));
/// The token whose pairing code was last shown on the PC.
static ANNOUNCED: Mutex<String> = Mutex::new(String::new());
/// Options ticked so far on multi-select questions: (card id, question index, ticked).
static MULTI: Mutex<Vec<(String, usize, BTreeSet<usize>)>> = Mutex::new(Vec::new());

/// The one-time pairing code, kept in memory only (never on disk or in the log).
struct Pairing {
    token: String,
    /// None once blocked after too many wrong attempts.
    code: Option<String>,
    wrong: u32,
}

static PAIRING: Mutex<Option<Pairing>> = Mutex::new(None);

pub struct Creds {
    pub token: String,
    pub chat_id: String,
}

pub fn creds(cfg: &Value) -> Option<Creds> {
    let token = config::get_str(cfg, "phone.telegram.token")?.trim().to_string();
    if token.is_empty() {
        return None;
    }
    let mut chat_id = config::get_str(cfg, "phone.telegram.chatId")
        .unwrap_or_default()
        .trim()
        .to_string();
    if chat_id.starts_with('-') {
        // A group paired by an older version: only a private chat is obeyed now, so it counts
        // as unpaired (no phone holds, the pairing code is offered again).
        static WARNED: AtomicBool = AtomicBool::new(false);
        if !WARNED.swap(true, Ordering::SeqCst) {
            util::log_line(&["telegram-error", "paired chat is a group; only a private chat is obeyed: pair again with the code"]);
        }
        chat_id.clear();
    }
    Some(Creds { token, chat_id })
}

/// Phone delivery is on and a bot token is set, but no chat is paired yet.
pub fn needs_pairing(shared: &Shared) -> bool {
    let cfg = shared.cfg();
    config::get_bool(&cfg, "phone.enabled", true) && creds(&cfg).map_or(false, |c| c.chat_id.is_empty())
}

/// "HTTP 409: ..." -> 409.
fn http_status(e: &str) -> Option<u16> {
    e.strip_prefix("HTTP ")?.split(':').next()?.trim().parse().ok()
}

fn api(token: &str, method: &str, body: &Value, timeout_s: u64) -> Result<Value, String> {
    let url = format!("https://api.telegram.org/bot{token}/{method}");
    // Never let the bot token reach the log through error messages.
    let redact = |s: String| s.replace(token, "***");
    let resp = ureq::post(&url)
        .timeout(Duration::from_secs(timeout_s))
        .send_json(body.clone());
    let v: Value = match resp {
        Ok(r) => r.into_json().map_err(|e| redact(e.to_string()))?,
        Err(ureq::Error::Status(code, r)) => {
            let text = r.into_string().unwrap_or_default();
            return Err(redact(format!("HTTP {code}: {}", util::truncate(&text, 200))));
        }
        Err(e) => return Err(redact(e.to_string())),
    };
    if v.get("ok").and_then(Value::as_bool) != Some(true) {
        return Err(util::truncate(&v.to_string(), 200));
    }
    Ok(v.get("result").cloned().unwrap_or(Value::Null))
}

// ---------------------------------------------------------------- HTML

/// Cut Telegram HTML to `max` characters without splitting a tag or an entity, closing the
/// tags left open, so a long message still parses.
pub fn fit_html(html: &str, max: usize) -> String {
    if html.chars().count() <= max {
        return html.to_string();
    }
    // Room for the ellipsis and the closing tags.
    let budget = max.saturating_sub(40);
    let mut out = String::new();
    let mut open: Vec<String> = Vec::new();
    let mut used = 0usize;
    let mut rest = html;
    while let Some(c) = rest.chars().next() {
        let len = match c {
            '<' => rest.find('>').map_or(rest.len(), |i| i + 1),
            '&' => rest.char_indices().take(10).find(|(_, ch)| *ch == ';').map_or(1, |(i, _)| i + 1),
            _ => c.len_utf8(),
        };
        let token = &rest[..len];
        let n = token.chars().count();
        if used + n > budget {
            break;
        }
        if c == '<' {
            let inner = token.trim_start_matches('<').trim_end_matches('>');
            if let Some(name) = inner.strip_prefix('/') {
                let name = name.trim().to_ascii_lowercase();
                if let Some(pos) = open.iter().rposition(|o| *o == name) {
                    open.truncate(pos);
                }
            } else if !inner.ends_with('/') {
                let name = inner.split(char::is_whitespace).next().unwrap_or("").to_ascii_lowercase();
                if !name.is_empty() {
                    open.push(name);
                }
            }
        }
        out.push_str(token);
        used += n;
        rest = &rest[len..];
    }
    out.push('…');
    for name in open.iter().rev() {
        out.push_str(&format!("</{name}>"));
    }
    out
}

/// Telegram HTML as plain text (tags dropped, entities decoded): the fallback when Telegram
/// cannot parse a message.
fn html_to_plain(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut in_tag = false;
    for c in html.chars() {
        match c {
            '<' => in_tag = true,
            '>' if in_tag => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    out.replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"").replace("&amp;", "&")
}

/// A card's HTML with a closing line (answered, closed, expired): the card is cut so the
/// closing line always fits.
pub fn with_footer(html: &str, footer: &str) -> String {
    let footer = fit_html(footer, MAX_HTML / 2);
    let room = MAX_HTML.saturating_sub(footer.chars().count() + 2);
    format!("{}\n\n{footer}", fit_html(html, room))
}

fn keyboard(buttons: &[Vec<(String, String)>]) -> Value {
    let rows: Vec<Value> = buttons
        .iter()
        .map(|row| Value::Array(row.iter().map(|(label, data)| json!({"text": label, "callback_data": data})).collect()))
        .collect();
    json!({"inline_keyboard": rows})
}

/// sendMessage / editMessageText with HTML in `body.text`, fitted to the length limit. When
/// Telegram cannot parse the markup, the same text goes once more as plain text.
fn api_html(token: &str, method: &str, mut body: Value) -> Result<Value, String> {
    let html = fit_html(body.get("text").and_then(Value::as_str).unwrap_or(""), MAX_HTML);
    body["text"] = json!(html);
    body["parse_mode"] = json!("HTML");
    match api(token, method, &body, 15) {
        Err(e) if e.contains("can't parse entities") => {
            util::log_line(&["telegram-error", "HTML rejected, sent as plain text"]);
            body["text"] = json!(html_to_plain(&html));
            if let Some(o) = body.as_object_mut() {
                o.remove("parse_mode");
            }
            api(token, method, &body, 15)
        }
        r => r,
    }
}

// ---------------------------------------------------------------- sending

/// Send a message to the paired chat. `buttons` are rows of (label, callback_data).
/// Returns Telegram's message_id so the message can be edited once the card is answered.
pub fn send(shared: &Shared, text: &str, buttons: &[Vec<(String, String)>]) -> Result<i64, String> {
    let cfg = shared.cfg();
    let c = creds(&cfg).ok_or_else(|| "telegram not configured".to_string())?;
    if c.chat_id.is_empty() {
        return Err("telegram not paired yet: send the pairing code to your bot".to_string());
    }
    let mut body = json!({
        "chat_id": c.chat_id,
        "text": text,
        "disable_web_page_preview": true,
    });
    if !buttons.is_empty() {
        body["reply_markup"] = keyboard(buttons);
    }
    let r = api_html(&c.token, "sendMessage", body)?;
    r.get("message_id")
        .and_then(Value::as_i64)
        .ok_or_else(|| "no message_id in reply".to_string())
}

/// Replace a message's text and drop its buttons (card answered or expired).
pub fn edit(shared: &Shared, message_id: i64, text: &str) {
    let cfg = shared.cfg();
    let Some(c) = creds(&cfg) else { return };
    if c.chat_id.is_empty() {
        return;
    }
    let body = json!({
        "chat_id": c.chat_id,
        "message_id": message_id,
        "text": text,
        "reply_markup": {"inline_keyboard": []},
    });
    if let Err(e) = api_html(&c.token, "editMessageText", body) {
        util::log_line(&["telegram-error", &e]);
    }
}

/// Register the bot's command menu (the "/" list in Telegram) for remote sessions.
fn register_commands(shared: &Shared, c: &Creds) {
    if shared.remote.lock().unwrap().commands_registered {
        return;
    }
    let cmds: Vec<Value> = if crate::remote::enabled(shared) {
        crate::remote::commands(shared)
            .into_iter()
            .map(|(command, description)| json!({"command": command, "description": description}))
            .collect()
    } else {
        Vec::new()
    };
    let had_commands = shared.remote.lock().unwrap().commands_set;
    let result = if cmds.is_empty() {
        if !had_commands {
            shared.remote.lock().unwrap().commands_registered = true;
            return;
        }
        api(&c.token, "deleteMyCommands", &json!({}), 10)
    } else {
        api(&c.token, "setMyCommands", &json!({"commands": cmds}), 10)
    };
    match result {
        Ok(_) => {
            let mut st = shared.remote.lock().unwrap();
            st.commands_registered = true;
            st.commands_set = !cmds.is_empty();
        }
        Err(e) => util::log_line(&["telegram-error", &e]),
    }
}

/// Replace a message's text and its buttons (next question of a multi-question card).
pub fn edit_with_buttons(shared: &Shared, message_id: i64, text: &str, buttons: &[Vec<(String, String)>]) -> bool {
    let cfg = shared.cfg();
    let Some(c) = creds(&cfg) else { return false };
    if c.chat_id.is_empty() {
        return false;
    }
    let body = json!({
        "chat_id": c.chat_id,
        "message_id": message_id,
        "text": text,
        "reply_markup": keyboard(buttons),
    });
    match api_html(&c.token, "editMessageText", body) {
        Ok(_) => true,
        Err(e) => {
            util::log_line(&["telegram-error", &e]);
            false
        }
    }
}

/// Ask for a typed answer: Telegram opens the reply box on the user's phone.
fn send_force_reply(shared: &Shared, text: &str) -> Option<i64> {
    let cfg = shared.cfg();
    let c = creds(&cfg)?;
    if c.chat_id.is_empty() {
        return None;
    }
    let body = json!({
        "chat_id": c.chat_id,
        "text": text,
        "reply_markup": {"force_reply": true, "input_field_placeholder": util::truncate(shared.t().type_placeholder, 60)},
    });
    match api_html(&c.token, "sendMessage", body) {
        Ok(r) => r.get("message_id").and_then(Value::as_i64),
        Err(e) => {
            util::log_line(&["telegram-error", &e]);
            None
        }
    }
}

fn plain(c: &Creds, chat: &str, text: &str) {
    let _ = api(&c.token, "sendMessage", &json!({"chat_id": chat, "text": text}), 10);
}

// ---------------------------------------------------------------- questions

/// Answer question `qi` of a question card from the phone. A multi-question card collects
/// answers one by one; the card is resolved when every question has an answer. `tapped` is
/// the card message whose button gave the answer (None: the answer was typed). Returns false
/// when the card or question is not current any more.
fn answer_question(app: &AppHandle, shared: &Arc<Shared>, id: &str, qi: usize, answer: String, tapped: Option<i64>) -> bool {
    let progress = {
        let mut pending = shared.pending.lock().unwrap();
        let Some(p) = pending.get_mut(id) else { return false };
        if p.kind != "question" || qi != p.partial.len() {
            return false;
        }
        let Some(qs) = p.event.pointer("/tool_input/questions").and_then(Value::as_array) else { return false };
        let total = qs.len();
        let Some(key) = qs.get(qi).and_then(|q| q.get("question")).and_then(Value::as_str).map(String::from) else { return false };
        p.partial.insert(key, Value::String(answer.clone()));
        if p.partial.len() >= total {
            Err(Value::Object(p.partial.clone()))
        } else {
            Ok((p.event.clone(), p.partial.clone()))
        }
    };
    MULTI.lock().unwrap().retain(|(i, q, _)| !(i == id && *q == qi));
    let (event, done) = match progress {
        Err(all) => {
            events::finish_interactive(app, shared, id, Some(json!({"answers": all})));
            return true;
        }
        Ok(x) => x,
    };
    let next = done.len();
    util::log_line(&["tg-progress", id, &next.to_string(), if tapped.is_some() { "button" } else { "typed" }]);
    // The PC card moves on to the same question.
    events::card_progress(app, shared, id, next, &done);
    let Some((_, html, buttons)) = events::phone_question(shared, id, &event, next, &done) else { return true };
    // The card's open message on the phone (one, unless an earlier move failed half-way).
    let live: Vec<i64> = shared.tg_msgs.lock().unwrap().iter().filter(|(_, v)| v.0 == id).map(|(k, _)| *k).collect();
    let in_place = tapped.filter(|m| live.contains(m));
    let mut shown = None;
    // After a tap the message is on screen: it moves on to the next question in place.
    if let Some(mid) = in_place {
        if edit_with_buttons(shared, mid, &html, &buttons) {
            shared.tg_msgs.lock().unwrap().insert(mid, (id.to_string(), html.clone()));
            shown = Some(mid);
        }
    }
    if shown.is_none() {
        // After a typed answer the card message sits above the prompt and the reply, maybe out
        // of view: the next question goes to the bottom of the chat as a new message.
        match send(shared, &html, &buttons) {
            Ok(new_mid) => {
                shared.tg_card_msgs.lock().unwrap().insert(new_mid);
                shared.tg_msgs.lock().unwrap().insert(new_mid, (id.to_string(), html.clone()));
                shared.tg_last_pending_ms.store(util::now_ms(), Ordering::SeqCst);
                *shared.tg_last_pending.lock().unwrap() = Some(id.to_string());
                shown = Some(new_mid);
                // The old message keeps its question with the answer under it, and no buttons:
                // it takes no more input (unless resolved() closed it meanwhile).
                let compact = events::phone_question_answered(shared, &event, qi, &answer);
                for mid in live {
                    if shared.tg_msgs.lock().unwrap().remove(&mid).is_some() {
                        if let Some(compact) = &compact {
                            edit(shared, mid, compact);
                        }
                    }
                }
            }
            Err(e) => {
                util::log_line(&["telegram-error", &e]);
                // Could not send: move the old message on instead (a tap already tried that).
                if let (None, Some(&mid)) = (in_place, live.first()) {
                    if edit_with_buttons(shared, mid, &html, &buttons) {
                        shared.tg_msgs.lock().unwrap().insert(mid, (id.to_string(), html.clone()));
                        shown = Some(mid);
                    }
                }
            }
        }
    }
    // Closed (on the PC, or expired) while this was being sent: close it again.
    if let Some(m) = shown {
        if !shared.pending.lock().unwrap().contains_key(id) && shared.tg_msgs.lock().unwrap().remove(&m).is_some() {
            edit(shared, m, &with_footer(&html, shared.t().tg_closed));
        }
    }
    true
}

/// (option labels by position, index of the question waiting for an answer, multi-select)
/// of a question card.
fn current_question(shared: &Shared, id: &str) -> Option<(Vec<String>, usize, bool)> {
    let pending = shared.pending.lock().unwrap();
    let p = pending.get(id)?;
    let qi = p.partial.len();
    let q = p.event.pointer("/tool_input/questions")?.as_array()?.get(qi)?;
    let options = q
        .get("options")
        .and_then(Value::as_array)
        .map(|os| os.iter().map(|o| o.get("label").and_then(Value::as_str).unwrap_or("").to_string()).collect())
        .unwrap_or_default();
    let multi = q.get("multiSelect").and_then(Value::as_bool).unwrap_or(false);
    Some((options, qi, multi))
}

/// Options ticked so far on a multi-select question shown on the phone.
pub fn multi_selected(id: &str, qi: usize) -> Vec<usize> {
    MULTI
        .lock()
        .unwrap()
        .iter()
        .find(|(i, q, _)| i == id && *q == qi)
        .map(|(_, _, set)| set.iter().copied().collect())
        .unwrap_or_default()
}

fn toggle_multi(shared: &Shared, id: &str, qi: usize, oi: usize) {
    let live: HashSet<String> = shared.pending.lock().unwrap().keys().cloned().collect();
    let mut m = MULTI.lock().unwrap();
    // Cards answered on the PC or expired leave their ticks behind: drop them here.
    m.retain(|(i, _, _)| live.contains(i));
    match m.iter().position(|(i, q, _)| i == id && *q == qi) {
        Some(k) => {
            let set = &mut m[k].2;
            if !set.remove(&oi) {
                set.insert(oi);
            }
        }
        None => m.push((id.to_string(), qi, BTreeSet::from([oi]))),
    }
}

/// Redraw a card's current question on its message (the ticks of a multi-select question).
fn refresh_question(shared: &Shared, id: &str, qi: usize, mid: Option<i64>) {
    let Some(mid) = mid else { return };
    let card = shared.pending.lock().unwrap().get(id).map(|p| (p.event.clone(), p.partial.clone()));
    let Some((event, done)) = card else { return };
    let Some((_, html, buttons)) = events::phone_question(shared, id, &event, qi, &done) else { return };
    if let Some(entry) = shared.tg_msgs.lock().unwrap().get_mut(&mid) {
        entry.1 = html.clone();
    }
    edit_with_buttons(shared, mid, &html, &buttons);
}

/// "allow" / "deny" for a typed reply to a permission card. Turkish capitals are folded
/// first: phone keyboards turn "izin" into "İzin", which lower-cases to "i̇zin".
fn answer_word(text: &str) -> Option<&'static str> {
    let l = text.trim().to_lowercase().replace('\u{307}', "");
    let l = l.trim_end_matches(['.', '!']).trim();
    if ["allow", "yes", "ok", "evet", "izin", "izin ver"].contains(&l) {
        Some("allow")
    } else if ["deny", "no", "n", "hayır", "hayir", "h", "reddet"].contains(&l) {
        Some("deny")
    } else {
        None
    }
}

// ---------------------------------------------------------------- pairing

fn new_code() -> String {
    let n = CODE_ALPHABET.len();
    let mut out = String::with_capacity(CODE_LEN);
    while out.len() < CODE_LEN {
        let hex = util::random_hex(16);
        for i in 0..16 {
            let b = u8::from_str_radix(&hex[2 * i..2 * i + 2], 16).unwrap_or(255) as usize;
            // Only bytes below a multiple of the alphabet size: every character equally likely.
            if b < 256 - 256 % n && out.len() < CODE_LEN {
                out.push(CODE_ALPHABET[b % n] as char);
            }
        }
    }
    out
}

/// The pairing code for `token`: a new one when the token changed, or (with `renew`) when
/// the old one was blocked. None while blocked.
fn pairing_code(token: &str, renew: bool) -> Option<String> {
    let mut p = PAIRING.lock().unwrap();
    let fresh = match p.as_ref() {
        Some(x) => x.token != token || (renew && x.code.is_none()),
        None => true,
    };
    if fresh {
        *p = Some(Pairing { token: token.to_string(), code: Some(new_code()), wrong: 0 });
    }
    p.as_ref().and_then(|x| x.code.clone())
}

enum CodeCheck {
    Match,
    Wrong { blocked: bool },
    NoCode,
}

fn check_code(token: &str, candidate: &str) -> CodeCheck {
    let mut guard = PAIRING.lock().unwrap();
    let Some(p) = guard.as_mut().filter(|p| p.token == token) else { return CodeCheck::NoCode };
    let Some(code) = &p.code else { return CodeCheck::NoCode };
    if util::ct_eq(candidate.as_bytes(), code.as_bytes()) {
        return CodeCheck::Match;
    }
    p.wrong += 1;
    let blocked = p.wrong >= MAX_WRONG_CODES;
    if blocked {
        p.code = None;
    }
    CodeCheck::Wrong { blocked }
}

fn clear_pairing() {
    *PAIRING.lock().unwrap() = None;
}

/// The code in "/start CODE", "/start@bot CODE" or a bare "CODE", upper-cased with spaces
/// and dashes removed. None for a bare /start (Telegram's Start button) or another command.
fn code_candidate(text: &str) -> Option<String> {
    let t = text.trim();
    let rest = match t.strip_prefix("/start") {
        Some(r) => {
            // "/start@MyBot CODE"
            let r = if r.starts_with('@') { r.find(char::is_whitespace).map_or("", |i| &r[i..]) } else { r };
            if !r.is_empty() && !r.starts_with(char::is_whitespace) {
                return None;
            }
            r
        }
        None if t.starts_with('/') => return None,
        None => t,
    };
    let code = rest.chars().filter(|c| !c.is_whitespace() && *c != '-').collect::<String>().to_uppercase();
    (!code.is_empty()).then_some(code)
}

fn show_code(app: &AppHandle, shared: &Shared, code: &str) {
    let t = shared.t();
    events::notify(app, shared, "HeadsUp", &t.tg_pair_code.replace("{code}", code), t.tg_pair_send, "info", Some(60_000));
}

/// Show the pairing code on the PC (or say that it is blocked); once per token unless forced.
fn announce(app: &AppHandle, shared: &Shared, token: &str, force: bool) {
    {
        let mut a = ANNOUNCED.lock().unwrap();
        if !force && *a == token {
            return;
        }
        *a = token.to_string();
    }
    match pairing_code(token, false) {
        Some(code) => show_code(app, shared, &code),
        None => events::notify(app, shared, "HeadsUp", shared.t().tg_pair_blocked, "", "permission", Some(15_000)),
    }
}

/// Tray: show the pairing code, with a new one if the old one was blocked.
pub fn show_pairing_code(app: &AppHandle, shared: &Shared) {
    let cfg = shared.cfg();
    let t = shared.t();
    match creds(&cfg) {
        None => events::notify(app, shared, "HeadsUp", t.tg_no_token, "", "permission", Some(10_000)),
        Some(c) if !c.chat_id.is_empty() => events::notify(app, shared, "HeadsUp", t.tg_already_paired, "", "info", Some(10_000)),
        Some(_) if !config::get_bool(&cfg, "phone.enabled", true) => {
            events::notify(app, shared, "HeadsUp", t.phone_not_configured, "", "permission", Some(10_000))
        }
        Some(c) => {
            if let Some(code) = pairing_code(&c.token, true) {
                *ANNOUNCED.lock().unwrap() = c.token.clone();
                show_code(app, shared, &code);
            }
        }
    }
}

/// Reload config: retry a token Telegram rejected, log errors afresh, and show the pairing
/// code again while no chat is paired.
pub fn on_reload(app: &AppHandle, shared: &Shared) {
    RELOADS.fetch_add(1, Ordering::SeqCst);
    *LAST_POLL_ERROR.lock().unwrap() = (String::new(), 0);
    let cfg = shared.cfg();
    if !config::get_bool(&cfg, "phone.enabled", true) {
        return;
    }
    if let Some(c) = creds(&cfg).filter(|c| c.chat_id.is_empty()) {
        announce(app, shared, &c.token, true);
    }
}

/// "@username" (or the name) of the account that sent `m`, so the PC can show who paired.
fn account_name(m: &Value) -> String {
    let f = |k: &str| m.pointer(&format!("/from/{k}")).and_then(Value::as_str).unwrap_or("").trim().to_string();
    let user = f("username");
    let name = if user.is_empty() { format!("{} {}", f("first_name"), f("last_name")).trim().to_string() } else { format!("@{user}") };
    util::truncate(&name, 64)
}

/// A private message while no chat is paired: only the one-time code pairs the chat.
fn pairing_attempt(app: &AppHandle, shared: &Arc<Shared>, c: &Creds, m: &Value, chat: &str, text: &str) {
    let t = shared.t();
    if text.is_empty() {
        return;
    }
    let Some(candidate) = code_candidate(text) else {
        // Telegram's Start button or another command: explain, but do not count it.
        plain(c, chat, t.tg_pair_hint);
        return;
    };
    match check_code(&c.token, &candidate) {
        CodeCheck::Match => {
            if pair(shared, chat) {
                clear_pairing();
                plain(c, chat, t.tg_paired);
                let who = t.tg_paired_with.replace("{name}", &account_name(m));
                events::notify(app, shared, "HeadsUp", t.tg_paired, &who, "done", Some(15_000));
            } else {
                let msg = crate::remote::s(shared).config_unreadable;
                plain(c, chat, msg);
                events::notify(app, shared, "HeadsUp", msg, "", "permission", None);
            }
        }
        CodeCheck::Wrong { blocked } => {
            util::log_line(&["telegram-pair-refused", if blocked { "code blocked" } else { "wrong code" }]);
            plain(c, chat, t.tg_pair_hint);
            if blocked {
                events::notify(app, shared, "HeadsUp", t.tg_pair_blocked, "", "permission", Some(15_000));
            }
        }
        CodeCheck::NoCode => plain(c, chat, t.tg_pair_hint),
    }
}

fn pair(shared: &Shared, chat: &str) -> bool {
    let path = util::config_path();
    let lock = util::lock_config();
    let mut cfg = match util::read_json(&path) {
        Some(v) if v.is_object() => v,
        // Never overwrite a config.json that exists but does not parse.
        _ if path.exists() => {
            util::log_line(&["telegram-error", "config.json unreadable, pairing not saved"]);
            return false;
        }
        _ => json!({}),
    };
    if !cfg.get("phone").map(Value::is_object).unwrap_or(false) {
        cfg["phone"] = json!({});
    }
    if !cfg["phone"].get("telegram").map(Value::is_object).unwrap_or(false) {
        cfg["phone"]["telegram"] = json!({});
    }
    cfg["phone"]["telegram"]["chatId"] = json!(chat);
    // A newly paired chat never inherits remote control: it has to be switched on again
    // from the tray, on the PC.
    if cfg.get("remote").map(Value::is_object).unwrap_or(false) {
        cfg["remote"]["enabled"] = json!(false);
    }
    if util::write_json(&util::config_path(), &cfg).is_err() {
        return false;
    }
    drop(lock);
    shared.reload_config();
    util::log_line(&["telegram-paired", &util::tail_id(chat)]);
    true
}

// ---------------------------------------------------------------- polling

/// Log a poll error, but the same one at most once per 10 minutes.
fn log_poll_error(e: &str) {
    let now = util::now_ms();
    {
        let mut last = LAST_POLL_ERROR.lock().unwrap();
        if last.0 == e && now - last.1 < 600_000 {
            return;
        }
        *last = (e.to_string(), now);
    }
    util::log_line(&["telegram-error", e]);
}

/// Long-poll loop. Re-reads the config every round, so filling in the token (and
/// "Reload config") is enough to turn it on.
pub fn start(app: AppHandle, shared: Arc<Shared>) {
    std::thread::spawn(move || {
        let mut offset: i64 = 0;
        // Update ids belong to one bot: a new token starts from scratch.
        let mut offset_token = String::new();
        let mut failures: u32 = 0;
        // A token Telegram rejected, with the reload count then: not polled until a reload.
        let mut halted: Option<(String, u64)> = None;
        let mut had_token: Option<bool> = None;
        loop {
            if shared.quitting.load(Ordering::SeqCst) {
                return;
            }
            let cfg = shared.cfg();
            let enabled = config::get_bool(&cfg, "phone.enabled", true);
            let c = creds(&cfg);
            // Only whether a token is set ever reaches the log, never the token itself.
            if had_token != Some(c.is_some()) {
                had_token = Some(c.is_some());
                util::log_line(&["telegram", if c.is_some() { "token set" } else { "token missing" }]);
            }
            let Some(c) = c.filter(|_| enabled) else {
                std::thread::sleep(Duration::from_secs(10));
                continue;
            };
            if c.token != offset_token {
                offset = 0;
                offset_token = c.token.clone();
                failures = 0;
            }
            if let Some((token, reloads)) = &halted {
                if *token == c.token && *reloads == RELOADS.load(Ordering::SeqCst) {
                    std::thread::sleep(Duration::from_secs(10));
                    continue;
                }
                halted = None;
            }
            if c.chat_id.is_empty() {
                announce(&app, &shared, &c.token, false);
                // Nothing sent before this code existed can pair: skip the backlog except
                // the newest update, which is checked like any other.
                if offset == 0 {
                    offset = -1;
                }
            } else {
                register_commands(&shared, &c);
            }
            let body = json!({
                "offset": offset,
                "timeout": 25,
                "allowed_updates": ["message", "callback_query"],
            });
            match api(&c.token, "getUpdates", &body, 40) {
                Ok(v) => {
                    failures = 0;
                    for u in v.as_array().into_iter().flatten() {
                        if let Some(id) = u.get("update_id").and_then(Value::as_i64) {
                            offset = offset.max(id + 1);
                        }
                        handle_update(&app, &shared, &c, u);
                    }
                }
                Err(e) => {
                    log_poll_error(&e);
                    match http_status(&e) {
                        // A revoked or mistyped token: asking again cannot help.
                        Some(401) | Some(404) => {
                            halted = Some((c.token.clone(), RELOADS.load(Ordering::SeqCst)));
                            events::notify(&app, &shared, "HeadsUp", shared.t().tg_unauthorized, "", "permission", Some(15_000));
                            continue;
                        }
                        // Another PC or app polls this bot: Telegram hands each update to one
                        // of us only. Say so when it starts, then poll less often.
                        Some(409) => {
                            let now = util::now_ms();
                            if now - LAST_CONFLICT_MS.swap(now, Ordering::SeqCst) > 600_000 {
                                events::notify(&app, &shared, "HeadsUp", shared.t().tg_conflict, "", "permission", Some(15_000));
                            }
                        }
                        _ => {}
                    }
                    failures = failures.saturating_add(1);
                    let secs = (5u64 << failures.min(5).saturating_sub(1)).min(60);
                    std::thread::sleep(Duration::from_secs(secs));
                }
            }
        }
    });
}

fn handle_update(app: &AppHandle, shared: &Arc<Shared>, c: &Creds, u: &Value) {
    let t = shared.t();
    // Re-read: an earlier update of the same batch may have just paired a chat.
    let fresh = creds(&shared.cfg());
    let c = fresh.as_ref().unwrap_or(c);

    if let Some(cb) = u.get("callback_query") {
        let data = cb.get("data").and_then(Value::as_str).unwrap_or("");
        let cb_id = cb.get("id").and_then(Value::as_str).unwrap_or("");
        let chat = cb.pointer("/message/chat/id").map(|v| v.to_string()).unwrap_or_default();
        let from = cb.pointer("/from/id").map(|v| v.to_string()).unwrap_or_default();
        // Only the paired user's own taps in the paired chat: a card forwarded or shown
        // anywhere else never authorizes anything.
        if c.chat_id.is_empty() || chat != c.chat_id || from != c.chat_id {
            return;
        }
        let card_mid = cb.pointer("/message/message_id").and_then(Value::as_i64);
        // What waits for a usage-limit reset: continue, don't, cancel, send now (schedule.rs).
        if data.starts_with('l') {
            let toast = crate::schedule::on_callback(app, shared, data).unwrap_or_default();
            let _ = api(&c.token, "answerCallbackQuery", &json!({"callback_query_id": cb_id, "text": util::truncate(&toast, 180)}), 10);
            return;
        }
        if data.starts_with('r') {
            let toast = crate::remote::on_callback(app, shared, data).unwrap_or_default();
            let _ = api(&c.token, "answerCallbackQuery", &json!({"callback_query_id": cb_id, "text": util::truncate(&toast, 180)}), 10);
            return;
        }
        let parts: Vec<&str> = data.split(':').collect();
        let mut reply = t.tg_no_open;
        match (parts.first().copied(), parts.len()) {
            (Some("p"), 3) => {
                let id = parts[1].to_string();
                if shared.pending.lock().unwrap().contains_key(&id) {
                    let behavior = if parts[2] == "a" { "allow" } else { "deny" };
                    events::finish_interactive(app, shared, &id, Some(json!({"behavior": behavior})));
                    reply = t.tg_answered;
                }
            }
            (Some("qo"), 3) => {
                let id = parts[1].to_string();
                let qi: usize = parts[2].parse().unwrap_or(usize::MAX);
                if let Some((_, current, _)) = current_question(shared, &id) {
                    if current == qi {
                        let question = {
                            let pending = shared.pending.lock().unwrap();
                            pending
                                .get(&id)
                                .and_then(|p| p.event.pointer(&format!("/tool_input/questions/{qi}/question")).and_then(Value::as_str).map(String::from))
                                .unwrap_or_default()
                        };
                        *shared.tg_await_text.lock().unwrap() = Some((id.clone(), qi));
                        let question = events::cut(&util::clean_message(&question), 1500);
                        let prompt = format!("✍️ <b>{}</b>\n{}", crate::phone::esc(t.type_placeholder), crate::phone::esc(&question));
                        if let Some(mid) = send_force_reply(shared, &prompt) {
                            // A reply to this prompt answers exactly this question.
                            shared.tg_prompt_msgs.lock().unwrap().insert(mid, (id.clone(), qi));
                            shared.tg_card_msgs.lock().unwrap().insert(mid);
                        }
                        util::log_line(&["tg-await-text", &id, &qi.to_string()]);
                        reply = t.tg_answered;
                    }
                }
            }
            (Some("q"), 3) | (Some("q"), 4) => {
                let id = parts[1].to_string();
                let (qi, oi) = if parts.len() == 4 {
                    (parts[2].parse().unwrap_or(usize::MAX), parts[3].parse().unwrap_or(usize::MAX))
                } else {
                    (0, parts[2].parse().unwrap_or(usize::MAX))
                };
                if let Some((options, current, _)) = current_question(shared, &id) {
                    if current == qi {
                        if let Some(label) = options.get(oi).filter(|l| !l.is_empty()) {
                            if answer_question(app, shared, &id, qi, label.clone(), card_mid) {
                                reply = t.tg_answered;
                            }
                        }
                    }
                }
            }
            // Multi-select: a tap ticks or unticks an option, Done submits the ticked ones.
            (Some("qm"), 4) => {
                let id = parts[1].to_string();
                let qi: usize = parts[2].parse().unwrap_or(usize::MAX);
                let oi: usize = parts[3].parse().unwrap_or(usize::MAX);
                if let Some((options, current, multi)) = current_question(shared, &id) {
                    if current == qi && multi && options.get(oi).map_or(false, |l| !l.is_empty()) {
                        toggle_multi(shared, &id, qi, oi);
                        refresh_question(shared, &id, qi, card_mid);
                        reply = "";
                    }
                }
            }
            (Some("qd"), 3) => {
                let id = parts[1].to_string();
                let qi: usize = parts[2].parse().unwrap_or(usize::MAX);
                if let Some((options, current, multi)) = current_question(shared, &id) {
                    if current == qi && multi {
                        let picked: Vec<String> = multi_selected(&id, qi)
                            .into_iter()
                            .filter_map(|i| options.get(i).filter(|l| !l.is_empty()).cloned())
                            .collect();
                        if picked.is_empty() {
                            reply = t.tg_pick_one;
                        } else if answer_question(app, shared, &id, qi, picked.join(", "), card_mid) {
                            reply = t.tg_answered;
                        }
                    }
                }
            }
            _ => {}
        }
        let _ = api(&c.token, "answerCallbackQuery", &json!({"callback_query_id": cb_id, "text": reply}), 10);
        return;
    }

    let Some(m) = u.get("message") else { return };
    let chat = m.pointer("/chat/id").map(|v| v.to_string()).unwrap_or_default();
    let from = m.pointer("/from/id").map(|v| v.to_string()).unwrap_or_default();
    let text = m.get("text").and_then(Value::as_str).unwrap_or("").trim().to_string();
    // Only a private chat, written by the user it belongs to: never a group or a channel,
    // where others can talk to the bot too (even one paired by an older version).
    let private = m.pointer("/chat/type").and_then(Value::as_str) == Some("private") && !chat.is_empty() && from == chat;
    if !private {
        return;
    }
    if c.chat_id.is_empty() {
        pairing_attempt(app, shared, c, m, &chat, &text);
        return;
    }
    if chat != c.chat_id || text.is_empty() {
        return;
    }
    if text.starts_with("/start") {
        plain(c, &chat, t.tg_paired);
        return;
    }
    if text.starts_with('/') && crate::remote::on_command(app, shared, &text) {
        return;
    }

    // "Other…" was tapped on a question: this text is its answer.
    let reply_to = m.pointer("/reply_to_message/message_id").and_then(Value::as_i64);
    // A reply to an "Other…" prompt answers the question that prompt asked, if still current.
    let prompt = reply_to.and_then(|mid| shared.tg_prompt_msgs.lock().unwrap().get(&mid).cloned());
    if let Some((id, qi)) = prompt {
        *shared.tg_await_text.lock().unwrap() = None;
        let current = current_question(shared, &id).map(|(_, q, _)| q);
        if current == Some(qi) {
            util::log_line(&["tg-text", "prompt", &id]);
            if answer_question(app, shared, &id, qi, text.clone(), None) {
                return;
            }
        }
        // That question was answered meanwhile; the card may still be open on a later one.
        plain(c, &chat, if current.is_some() { t.tg_use_buttons } else { t.tg_no_open });
        return;
    }
    // Plain text right after tapping "Other…" answers that question; an explicit reply to
    // some other message cancels the wait and is routed normally.
    let awaiting = shared.tg_await_text.lock().unwrap().take();
    if let (Some((id, qi)), None) = (awaiting, reply_to) {
        let current = current_question(shared, &id).map(|(_, q, _)| q);
        if current == Some(qi) {
            util::log_line(&["tg-text", "await", &id]);
            if !answer_question(app, shared, &id, qi, text.clone(), None) {
                plain(c, &chat, t.tg_no_open);
            }
            return;
        }
    }

    // Which open card (if any) does this text answer?
    //  1. A Telegram reply to a card message answers that card (a permission card only with
    //     an allow / deny word).
    //  2. A reply to any other message never answers a card (a reply to a remote run's own
    //     message is a prompt for that run).
    //  3. Plain text answers the newest card on the phone only when that card is a question,
    //     and only while no remote session is in use (or the card is newer than the last
    //     remote interaction). Plain text never approves or denies a permission, and answers
    //     nothing while another PC polls the same bot (the card may be that PC's).
    let reply_mid = reply_to;
    let replied_card = reply_mid.and_then(|mid| shared.tg_msgs.lock().unwrap().get(&mid).map(|v| v.0.clone()));
    // A reply to a card that is already closed (or to a question of it already answered) is
    // not a new prompt for anybody.
    if let (Some(mid), None) = (reply_mid, &replied_card) {
        if shared.tg_card_msgs.lock().unwrap().contains(&mid) {
            let open = shared.tg_last_pending.lock().unwrap().clone();
            let open = open.filter(|id| shared.pending.lock().unwrap().contains_key(id));
            plain(c, &chat, if open.is_some() { t.tg_use_buttons } else { t.tg_no_open });
            return;
        }
    }
    let remote_in_use = crate::remote::enabled(shared) && crate::remote::has_target_or_run(shared);
    let last_card = shared.tg_last_pending.lock().unwrap().clone();
    let last_card = last_card.filter(|id| shared.pending.lock().unwrap().contains_key(id));
    let last_card_is_remote = last_card
        .as_ref()
        .and_then(|id| shared.pending.lock().unwrap().get(id).and_then(|p| p.event.get("session_id").and_then(Value::as_str).map(String::from)))
        .map(|sid| crate::remote::is_remote_session(shared, Some(&sid)))
        .unwrap_or(false);
    let card_is_newer = last_card_is_remote
        && shared.tg_last_pending_ms.load(Ordering::SeqCst) > crate::remote::last_touch_ms(shared);
    let conflict = util::now_ms() - LAST_CONFLICT_MS.load(Ordering::SeqCst) < 300_000;
    let kind_of = |id: &str| shared.pending.lock().unwrap().get(id).map(|p| p.kind.clone());
    let target = match (reply_mid, replied_card) {
        (Some(_), Some(card)) => {
            if !shared.pending.lock().unwrap().contains_key(&card) {
                plain(c, &chat, t.tg_no_open);
                return;
            }
            Some(card)
        }
        (Some(mid), None) if crate::remote::is_remote_msg(shared, mid) => None,
        (Some(_), None) => {
            plain(c, &chat, if last_card.is_some() { t.tg_use_buttons } else { t.tg_no_open });
            return;
        }
        (None, _) if remote_in_use && !card_is_newer => None,
        (None, _) => match last_card {
            Some(id) if !conflict && kind_of(&id).as_deref() == Some("question") => Some(id),
            Some(_) => {
                plain(c, &chat, t.tg_use_buttons);
                return;
            }
            None => None,
        },
    }
    .filter(|id| shared.pending.lock().unwrap().contains_key(id));
    util::log_line(&["tg-text", match &target { Some(_) => "card", None if remote_in_use => "remote", None => "none" }]);
    let Some(id) = target else {
        if crate::remote::on_text(app, shared, &text) {
            return;
        }
        plain(c, &chat, t.tg_no_open);
        return;
    };
    let kind = kind_of(&id);
    if kind.as_deref() == Some("question") {
        let qi = current_question(shared, &id).map(|(_, qi, _)| qi).unwrap_or(usize::MAX);
        if !answer_question(app, shared, &id, qi, text.clone(), None) {
            plain(c, &chat, t.tg_no_open);
        }
        return;
    }
    // Only an explicit reply to this permission card gets here.
    match (kind.as_deref(), answer_word(&text)) {
        (Some("permission"), Some(behavior)) => events::finish_interactive(app, shared, &id, Some(json!({"behavior": behavior}))),
        _ => plain(c, &chat, t.tg_use_buttons),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn balanced(html: &str) -> bool {
        ["b", "i", "pre", "code"].iter().all(|t| html.matches(&format!("<{t}>")).count() == html.matches(&format!("</{t}>")).count())
    }

    fn entities_whole(html: &str) -> bool {
        html.match_indices('&').all(|(i, _)| ["&lt;", "&gt;", "&amp;"].iter().any(|e| html[i..].starts_with(e)))
    }

    #[test]
    fn fit_html_never_splits_markup() {
        assert_eq!(fit_html("<b>x</b> &lt;y&gt;", 3500), "<b>x</b> &lt;y&gt;");
        let long = format!("<b>head</b>\n{}\n<i>hint</i>", "&lt;x&gt; &amp; ".repeat(400));
        for max in [3500, 3501, 3497, 200, 41] {
            let out = fit_html(&long, max);
            assert!(out.chars().count() <= max, "{max}: {}", out.chars().count());
            assert!(balanced(&out) && entities_whole(&out), "{max}: {out}");
        }
        let pre = format!("🔐 <b>Code</b>\n<pre>{}</pre>", "é".repeat(5000));
        let out = fit_html(&pre, 3500);
        assert!(out.ends_with("…</pre>") && balanced(&out));
    }

    #[test]
    fn footer_always_fits() {
        let card = format!("<pre>{}</pre>", "a".repeat(4000));
        let out = with_footer(&card, "✓ Allow");
        assert!(out.ends_with("\n\n✓ Allow") && out.chars().count() <= MAX_HTML && balanced(&out));
    }

    #[test]
    fn plain_text_fallback() {
        assert_eq!(html_to_plain("<b>a</b> &lt;b&gt; &amp;lt; <i>x</i>"), "a <b> &lt; x");
    }

    #[test]
    fn pairing_code_forms() {
        assert_eq!(code_candidate("/start abcd2345").as_deref(), Some("ABCD2345"));
        assert_eq!(code_candidate("/start@MyBot ABCD-2345").as_deref(), Some("ABCD2345"));
        assert_eq!(code_candidate(" abcd 2345 ").as_deref(), Some("ABCD2345"));
        assert_eq!(code_candidate("/start"), None);
        assert_eq!(code_candidate("/start@MyBot"), None);
        assert_eq!(code_candidate("/startABCD2345"), None);
        assert_eq!(code_candidate("/help"), None);
    }

    #[test]
    fn codes_and_wrong_attempts() {
        let c = new_code();
        assert_eq!(c.len(), CODE_LEN);
        assert!(c.bytes().all(|b| CODE_ALPHABET.contains(&b)));
        assert_ne!(new_code(), new_code());

        let token = "123:test";
        let code = pairing_code(token, false).unwrap();
        assert_eq!(pairing_code(token, false).as_deref(), Some(code.as_str()), "stable until used or blocked");
        for i in 1..=MAX_WRONG_CODES {
            match check_code(token, "WRONG123") {
                CodeCheck::Wrong { blocked } => assert_eq!(blocked, i == MAX_WRONG_CODES),
                _ => panic!("attempt {i} not counted"),
            }
        }
        assert!(matches!(check_code(token, &code), CodeCheck::NoCode), "blocked code still pairs");
        assert!(pairing_code(token, false).is_none());
        let fresh = pairing_code(token, true).unwrap();
        assert!(matches!(check_code("other", &fresh), CodeCheck::NoCode), "code of another token");
        assert!(matches!(check_code(token, &fresh), CodeCheck::Match));
        clear_pairing();
    }

    #[test]
    fn typed_permission_answers() {
        for (text, want) in [
            ("İzin", Some("allow")),
            ("İZİN VER", Some("allow")),
            ("Evet!", Some("allow")),
            ("ok.", Some("allow")),
            ("Hayır", Some("deny")),
            ("HAYIR", Some("deny")),
            ("y", None),
            ("e", None),
            ("sure, but first check", None),
        ] {
            assert_eq!(answer_word(text), want, "{text}");
        }
    }

    #[test]
    fn http_status_codes() {
        assert_eq!(http_status("HTTP 409: {\"ok\":false}"), Some(409));
        assert_eq!(http_status("Network Error: timed out"), None);
    }
}
