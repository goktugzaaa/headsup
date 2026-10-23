//! Phone delivery: decides whether an event goes to the phone (opt-in, and by default only
//! while the PC has been idle for a while) and fans it out to Telegram and/or ntfy.
use crate::{config, mask, telegram, util, Shared};
use serde_json::{json, Value};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

pub struct PhoneMsg {
    /// Send now, ignoring the idle rule (remote sessions: the user is away by definition).
    pub urgent: bool,
    /// "stop" | "permission" | "question" | "limit" (a reset) | "limitHit" (a limit used up) | "test"
    pub kind: &'static str,
    /// Plain text (ntfy, fallback).
    pub text: String,
    /// Telegram HTML version; built with `esc` for user content.
    pub html: Option<String>,
    /// Rows of (label, callback_data); Telegram only.
    pub buttons: Vec<Vec<(String, String)>>,
    /// The interactive card this message belongs to, if any.
    pub pending_id: Option<String>,
}

/// Escape user content for Telegram's HTML parse mode. Every piece of user content bound
/// for Telegram passes through here, so secrets are masked here too (masking twice is safe).
pub fn esc(s: &str) -> String {
    esc_raw(&mask::secrets(s))
}

/// Escape only, for text that was masked as a whole already.
pub fn esc_raw(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

/// "<b>Agent</b> · <i>project</i>" header line for Telegram messages.
pub fn head(agent: &str, project: &str) -> String {
    if project.is_empty() {
        format!("<b>{}</b>", esc(agent))
    } else {
        format!("<b>{}</b> · <i>{}</i>", esc(agent), esc(project))
    }
}

/// A topic anyone could guess: short, or plain words (no digits or capitals).
fn weak_topic(topic: &str) -> bool {
    let t = topic.trim();
    t.chars().count() < 16 || !t.chars().any(|c| c.is_ascii_digit() || c.is_ascii_uppercase())
}

/// Warning for the phone test (and, once, the log): on a public ntfy server the topic name is
/// all that keeps the messages private, unless an access token is set.
pub fn ntfy_warning(shared: &Shared) -> Option<&'static str> {
    let cfg = shared.cfg();
    let topic = config::get_str(&cfg, "phone.ntfy.topic").unwrap_or_default();
    let token = config::get_str(&cfg, "phone.ntfy.token").unwrap_or_default();
    (!topic.trim().is_empty() && token.trim().is_empty() && weak_topic(&topic)).then(|| shared.t().ntfy_weak)
}

fn ntfy_send(server: &str, topic: &str, token: Option<&str>, msg: &PhoneMsg) -> Result<(), String> {
    let url = format!("{}/", server.trim_end_matches('/'));
    let mut lines = msg.text.splitn(2, '\n');
    let title = mask::secrets(lines.next().unwrap_or(""));
    let body = mask::secrets(lines.next().unwrap_or(""));
    // ntfy JSON publishing wants a numeric priority (1 min .. 5 max).
    let priority: u8 = if matches!(msg.kind, "permission" | "question" | "limitHit") { 4 } else { 3 };
    let tags: Vec<&str> = match msg.kind {
        "permission" => vec!["lock"],
        "question" => vec!["question"],
        "limit" => vec!["orange_circle"],
        "limitHit" => vec!["no_entry"],
        _ => vec!["white_check_mark"],
    };
    let payload: Value = json!({
        "topic": topic,
        "title": title,
        "message": if body.is_empty() { title.clone() } else { body },
        "priority": priority,
        "tags": tags,
    });
    let mut req = ureq::post(&url).timeout(Duration::from_secs(10));
    // Reserved topics and self-hosted servers with access control need a token.
    if let Some(token) = token {
        req = req.set("Authorization", &format!("Bearer {token}"));
    }
    req.send_json(payload).map(|_| ()).map_err(|e| e.to_string())
}

/// Send to the phone if phone delivery is on and the event kind is enabled. Unless this is a
/// test, the PC must have been idle for `phone.onlyWhenIdleMs`; if it is not idle yet, the
/// message waits (up to `phone.deferWindowMs`) and goes out the moment the PC becomes idle,
/// as long as the card is still open and no newer event replaced it.
pub fn push(shared: &Arc<Shared>, msg: PhoneMsg) {
    let cfg = shared.cfg();
    if !config::get_bool(&cfg, "phone.enabled", true) {
        return;
    }
    if !configured(shared) {
        if msg.kind == "test" {
            util::log_line(&["phone-error", "not configured"]);
        }
        return;
    }
    if msg.kind == "test" {
        deliver(shared, msg);
        return;
    }
    if msg.urgent {
        // Remote runs: the user is away and the request is denied without an answer, so the
        // per-event switches do not apply.
        mark_on_phone(shared, &msg);
        deliver(shared, msg);
        return;
    }
    if !config::get_bool(&cfg, &format!("phone.events.{}", msg.kind), true) {
        return;
    }
    let idle_required = config::get_u64(&cfg, "phone.onlyWhenIdleMs", 180000);
    if idle_required == 0 || crate::win::idle_ms() >= idle_required {
        mark_on_phone(shared, &msg);
        deliver(shared, msg);
        return;
    }
    let window_ms = config::get_u64(&cfg, "phone.deferWindowMs", 900000);
    // An open card that nobody answers on the PC goes to the phone after this, idle or not.
    let unanswered_ms = config::get_u64(&cfg, "phone.sendUnansweredAfterMs", 60000);
    let generation = if msg.pending_id.is_none() {
        Some(shared.phone_generation.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1)
    } else {
        None
    };
    util::log_line(&["phone-deferred", msg.kind]);
    // A card's message waits in the card itself: closing the card on the PC sends it at once
    // (events::dismiss_card), and only one of the two ever takes it.
    let card = msg.pending_id.clone();
    let mut waiting = Some(msg);
    if let Some(id) = &card {
        match shared.pending.lock().unwrap().get_mut(id) {
            Some(p) => p.phone_wait = waiting.take(),
            None => return, // answered or closed already
        }
    }
    let shared = shared.clone();
    std::thread::spawn(move || {
        let started = util::now_ms();
        loop {
            std::thread::sleep(Duration::from_secs(10));
            if shared.quitting.load(std::sync::atomic::Ordering::SeqCst) {
                return;
            }
            if util::now_ms() - started > window_ms as i64 {
                return;
            }
            let unanswered = card.is_some() && unanswered_ms > 0 && util::now_ms() - started >= unanswered_ms as i64;
            let due = unanswered || crate::win::idle_ms() >= idle_required;
            if let Some(id) = &card {
                let msg = {
                    let mut pending = shared.pending.lock().unwrap();
                    let Some(p) = pending.get_mut(id) else { return }; // answered on the PC or expired
                    if p.phone_wait.is_none() {
                        return; // sent already: the card was closed on the PC
                    }
                    if !due {
                        continue;
                    }
                    p.on_phone = true;
                    p.phone_wait.take()
                };
                if let Some(msg) = msg {
                    deliver(&shared, msg);
                }
                return;
            }
            if let Some(g) = generation {
                if shared.phone_generation.load(std::sync::atomic::Ordering::SeqCst) != g {
                    return; // a newer event replaced this one
                }
            }
            if due {
                if let Some(msg) = waiting.take() {
                    deliver(&shared, msg);
                }
                return;
            }
        }
    });
}

/// The card `msg` belongs to (if any) has its message on the way to the phone.
fn mark_on_phone(shared: &Shared, msg: &PhoneMsg) {
    if let Some(id) = &msg.pending_id {
        if let Some(p) = shared.pending.lock().unwrap().get_mut(id) {
            p.on_phone = true;
        }
    }
}

/// A card closed on the PC whose message was still waiting for the PC to go idle: it goes to
/// the phone now, ignoring the idle rule (the caller took it out of the card).
pub fn send_now(shared: &Arc<Shared>, mut msg: PhoneMsg) {
    msg.urgent = true;
    util::log_line(&["phone-now", msg.kind]);
    deliver(shared, msg);
}

/// An urgent message that is edited later (its buttons answered): `on_sent` gets its Telegram
/// message id once it is sent.
pub fn push_tracked(shared: &Arc<Shared>, mut msg: PhoneMsg, on_sent: impl FnOnce(i64) + Send + 'static) {
    if !config::get_bool(&shared.cfg(), "phone.enabled", true) || !configured(shared) {
        return;
    }
    msg.urgent = true;
    deliver_with(shared, msg, Some(Box::new(on_sent)));
}

fn deliver(shared: &Arc<Shared>, msg: PhoneMsg) {
    deliver_with(shared, msg, None);
}

type OnSent = Box<dyn FnOnce(i64) + Send>;

fn deliver_with(shared: &Arc<Shared>, msg: PhoneMsg, on_sent: Option<OnSent>) {
    let cfg = shared.cfg();
    let has_telegram = telegram::creds(&cfg).map_or(false, |c| !c.chat_id.is_empty());
    let ntfy_topic = config::get_str(&cfg, "phone.ntfy.topic")
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty());
    static WARNED: AtomicBool = AtomicBool::new(false);
    if ntfy_topic.is_some() && ntfy_warning(shared).is_some() && !WARNED.swap(true, Ordering::SeqCst) {
        util::log_line(&["ntfy-warning", "topic is short or guessable and no phone.ntfy.token is set"]);
    }
    let shared = shared.clone();
    std::thread::spawn(move || {
        if has_telegram {
            let html = msg.html.clone().unwrap_or_else(|| esc(&msg.text));
            match telegram::send(&shared, &html, &msg.buttons) {
                Ok(mid) => {
                    if let Some(f) = on_sent {
                        f(mid);
                    }
                    if let Some(id) = &msg.pending_id {
                        shared.tg_card_msgs.lock().unwrap().insert(mid);
                        shared.tg_msgs.lock().unwrap().insert(mid, (id.clone(), html.clone()));
                        if shared.pending.lock().unwrap().contains_key(id) {
                            shared.tg_last_pending_ms.store(util::now_ms(), std::sync::atomic::Ordering::SeqCst);
                            *shared.tg_last_pending.lock().unwrap() = Some(id.clone());
                        } else if shared.tg_msgs.lock().unwrap().remove(&mid).is_some() {
                            // Closed while this message was on its way (and resolved() did not
                            // see it): close the message right away.
                            telegram::edit(&shared, mid, &telegram::with_footer(&html, shared.t().tg_closed));
                        }
                    }
                    util::log_line(&["telegram-sent", msg.kind]);
                }
                Err(e) => {
                    util::log_line(&["telegram-error", &e]);
                    // The phone cannot answer a card it never got (ntfy cannot answer at all):
                    // closing it on the PC releases it at once, and one closed already ends now
                    // (its timer replies within a second), instead of after phone.holdMs.
                    if let Some(id) = &msg.pending_id {
                        if let Some(p) = shared.pending.lock().unwrap().get_mut(id) {
                            p.on_phone = false;
                            if p.dismissed {
                                p.deadline_ms = util::now_ms();
                            }
                        }
                    }
                }
            }
        }
        if let Some(topic) = ntfy_topic {
            let server = config::get_str(&cfg, "phone.ntfy.server")
                .filter(|s| !s.trim().is_empty())
                .unwrap_or_else(|| "https://ntfy.sh".to_string());
            let token = config::get_str(&cfg, "phone.ntfy.token").map(|t| t.trim().to_string()).filter(|t| !t.is_empty());
            match ntfy_send(&server, &topic, token.as_deref(), &msg) {
                Ok(()) => util::log_line(&["ntfy-sent", msg.kind]),
                Err(e) => util::log_line(&["ntfy-error", &e]),
            }
        }
    });
}

/// A card was answered (on the PC or the phone) or expired: update its Telegram messages.
pub fn resolved(shared: &Arc<Shared>, id: &str, summary: String) {
    let mids: Vec<(i64, String)> = {
        let mut m = shared.tg_msgs.lock().unwrap();
        let ids: Vec<(i64, String)> = m
            .iter()
            .filter(|(_, v)| v.0 == id)
            .map(|(k, v)| (*k, v.1.clone()))
            .collect();
        for (k, _) in &ids {
            m.remove(k);
        }
        ids
    };
    if shared.tg_last_pending.lock().unwrap().as_deref() == Some(id) {
        // Plain text now goes to the newest card still open on the phone, if any. One lock at a
        // time: messages first, then the open cards.
        let open: Vec<(i64, String)> = shared.tg_msgs.lock().unwrap().iter().map(|(k, v)| (*k, v.0.clone())).collect();
        let newest = {
            let pending = shared.pending.lock().unwrap();
            open.into_iter().filter(|(_, card)| pending.contains_key(card)).max_by_key(|(mid, _)| *mid).map(|(_, card)| card)
        };
        let mut lp = shared.tg_last_pending.lock().unwrap();
        // Unless a newer card reached the phone meanwhile.
        if lp.as_deref() == Some(id) {
            *lp = newest;
        }
    }
    if mids.is_empty() {
        return;
    }
    let shared = shared.clone();
    std::thread::spawn(move || {
        for (mid, html) in mids {
            telegram::edit(&shared, mid, &telegram::with_footer(&html, &summary));
        }
    });
}

/// True when phone delivery is on and a channel can actually deliver: a Telegram token with
/// a paired chat, or an ntfy topic. Drives the long card hold and the focus-skip rule, so a
/// switched-off or never-paired phone must not count.
pub fn configured(shared: &Shared) -> bool {
    let cfg = shared.cfg();
    if !config::get_bool(&cfg, "phone.enabled", true) {
        return false;
    }
    telegram::creds(&cfg).map(|c| !c.chat_id.is_empty()).unwrap_or(false)
        || config::get_str(&cfg, "phone.ntfy.topic").map(|t| !t.trim().is_empty()).unwrap_or(false)
}

/// True when the phone can answer a card: phone delivery is on and a Telegram chat is paired
/// (ntfy only notifies). Only then does a card closed on the PC stay open for the phone.
pub fn answers_cards(shared: &Shared) -> bool {
    let cfg = shared.cfg();
    config::get_bool(&cfg, "phone.enabled", true) && telegram::creds(&cfg).is_some_and(|c| !c.chat_id.is_empty())
}
