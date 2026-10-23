//! Work that waits for a usage limit to reset. Three kinds:
//!
//!  - A message from the phone that arrives while a usage limit is used up is not run (it would
//!    only fail): it waits, and is sent when the limit resets, as if it had just been written.
//!    One waiting message per target (a new session in a project, or one session); a second
//!    message for the same target is appended to it.
//!  - A remote run that ends because the limit ran out is continued ("Continue where you left
//!    off.") when it resets (`remote.continueAtReset`, default on).
//!  - A Claude Code session on the PC that stopped on the limit (StopFailure) is continued the
//!    same way at the reset, after asking on the PC card and the phone (`limits.continueAtReset`
//!    "ask", the default) or on its own ("always"), unless Claude Code (the CLI's "Continue
//!    automatically at usage limit") or the Claude app (its own auto-resume) does it, or the
//!    session went on in the meantime.
//!
//! Everything waiting is kept in state.json (`limitSchedule`), so it survives a restart; /status
//! on the phone lists it with buttons to cancel it, and /stop cancels all of it. It runs through
//! the remote runner (remote.rs): one run at a time (the rest wait their turn), known projects
//! only, the allowed permission modes, never while HeadsUp is elevated. Never more than one
//! continue per session and reset, never a chain of them across resets without a new answer
//! from the user, at most MAX_CONTINUES per reset.
use crate::limits::{self, Hit, Waiter};
use crate::remote::{self, Origin, Start, Target};
use crate::{config, events, phone, telegram, util, Shared};
use chrono::{DateTime, TimeZone, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;
use tauri::AppHandle;

/// What a continue sends to the session.
pub const CONTINUE_PROMPT: &str = "Continue where you left off.";
/// Seconds after the reset a waiting message, or a remote session, goes.
const MESSAGE_DELAY: i64 = 20;
/// The same for a session of the Claude Code CLI: its own continue (the setting, or the wait
/// picked in its limit dialog) fires 30 to 90 s after the reset, and goes first.
const CLI_DELAY: i64 = 150;
/// The same for a session of the SDK (claude -p), which has no continue of its own.
const SDK_DELAY: i64 = 45;
/// The same for a session of the Claude app (or one whose launcher is not known): the app's own
/// auto-resume goes first (about 95 s after the reset, then up to two more tries 45 s apart).
const DESKTOP_DELAY: i64 = 240;
/// While the Claude app holds such a session armed for its own auto-resume, HeadsUp waits up to
/// this long after the reset before it continues the session itself.
const DESKTOP_WAIT_MAX: i64 = 600;
/// After the PC wakes from sleep nothing starts for this long (the network comes back; the Claude
/// app waits a minute itself); a Claude app session waits DESKTOP_DELAY.
const WAKE_DELAY_MS: i64 = 90_000;
/// A continue is planned only for a reset at most this far ahead.
const MAX_AHEAD: i64 = 24 * 3600;
/// A continue that was asked about and not answered stands until this long after the reset.
const OFFER_GRACE: i64 = 3600;
/// Anything still waiting this long after it was due (the PC was off) is dropped.
const STALE: i64 = 6 * 3600;
/// Continues per reset, all sessions together.
const MAX_CONTINUES: usize = 5;
/// Everything waiting, all kinds together.
const MAX_ITEMS: usize = 40;
/// A waiting message (appended ones together) is at most this long.
const MAX_PROMPT_CHARS: usize = 16_000;
/// A limit still in force when the item is due moves it to that limit's reset this often at most.
const MAX_DEFERRALS: u32 = 5;
/// Outcomes per session are kept this long.
const DONE_KEEP_MS: i64 = 7 * 86_400_000;
/// A session HeadsUp continued on its own this recently is asked about at the next reset, not
/// continued again on its own (no chain of continues without a new answer from the user).
const CHAIN_MS: i64 = 24 * 3_600_000;
/// How long the PC card waits for an answer (the question stays open on the phone after it).
const OFFER_CARD_MS: u64 = 120_000;
/// How long the PC card of a scheduled continue stays (its button cancels it).
const NOTICE_CARD_MS: u64 = 30_000;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    /// A message from the phone, sent at the reset.
    #[default]
    Message,
    /// CONTINUE_PROMPT to a session, at the reset.
    Continue,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Source {
    /// Written on the phone.
    #[default]
    Phone,
    /// A remote run that ended on the limit.
    Remote,
    /// A session on the PC that stopped on the limit.
    Pc,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    /// Runs when due.
    #[default]
    Scheduled,
    /// Waits for the user's answer ("ask").
    Offered,
    /// Being started now (cannot be cancelled any more).
    Starting,
}

/// One thing waiting for a reset, as kept in state.json.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Item {
    /// Random; part of its buttons' data.
    pub id: String,
    pub kind: Kind,
    pub source: Source,
    pub status: Status,
    pub cwd: String,
    pub project: String,
    /// The session it goes to; empty for a message that starts a new session.
    pub session_id: String,
    pub title: String,
    /// A message's text, as written (only here; never logged). Empty for a continue.
    pub prompt: String,
    /// The limit it waits for (limits.rs window name) and its reset (unix s).
    pub window: String,
    pub resets_at: i64,
    /// When it runs (unix s).
    pub due_at: i64,
    pub created_ms: i64,
    /// Who ran the session: "cli", "claude-desktop", "sdk-cli", "remote", "pc" (a session that
    /// stopped on the PC, launcher not known; messages only) or "".
    pub entrypoint: String,
    /// The permission mode the session of the PC stopped in (a continue runs in no looser one).
    pub permission_mode: String,
    pub transcript_path: String,
    pub deferrals: u32,
    /// The phone's target generation when the message was written (remote.rs).
    pub target_gen: u64,
    /// Its newest message on the phone, to edit when it runs or is cancelled.
    pub phone_mid: Option<i64>,
    pub phone_html: String,
    /// A card about it was shown on the PC.
    pub pc_card: bool,
}

/// What became of a session's continue for one reset.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Done {
    pub resets_at: i64,
    /// "ran", "declined", "cancelled", "skipped", "replaced", "claude-code".
    pub outcome: String,
    pub at_ms: i64,
}

/// state.json `limitSchedule`: what waits, and what was done per session.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Book {
    pub items: Vec<Item>,
    pub done: BTreeMap<String, Done>,
}

fn same_reset(a: i64, b: i64) -> bool {
    (a - b).abs() <= limits::SAME_RESET_SECS
}

fn same_dir(a: &str, b: &str) -> bool {
    let k = |p: &str| p.trim_end_matches(['\\', '/']).replace('/', "\\").to_lowercase();
    k(a) == k(b)
}

impl Book {
    fn load(st: &Value) -> Book {
        let v = st.get("limitSchedule");
        let items = v
            .and_then(|v| v.get("items"))
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(|i| serde_json::from_value::<Item>(i.clone()).ok()).filter(|i| !i.id.is_empty()).collect())
            .unwrap_or_default();
        let done = v
            .and_then(|v| v.get("done"))
            .and_then(|d| serde_json::from_value::<BTreeMap<String, Done>>(d.clone()).ok())
            .unwrap_or_default();
        Book { items, done }
    }

    fn store(&self, st: &mut Value) {
        st["limitSchedule"] = json!({"items": self.items, "done": self.done});
    }

    fn find(&self, id: &str) -> Option<&Item> {
        self.items.iter().find(|i| i.id == id)
    }

    fn get_mut(&mut self, id: &str) -> Option<&mut Item> {
        self.items.iter_mut().find(|i| i.id == id)
    }

    fn take(&mut self, id: &str) -> Option<Item> {
        let pos = self.items.iter().position(|i| i.id == id)?;
        Some(self.items.remove(pos))
    }

    /// Something waits that goes to this session.
    fn has_session(&self, session: &str) -> bool {
        !session.is_empty() && self.items.iter().any(|i| i.session_id == session)
    }

    /// The session's continue for this reset was decided already (run, declined, skipped...).
    fn done_for(&self, session: &str, resets_at: i64) -> bool {
        self.done.get(session).is_some_and(|d| same_reset(d.resets_at, resets_at))
    }

    fn mark(&mut self, session: &str, resets_at: i64, outcome: &str, now_ms: i64) {
        if !session.is_empty() {
            self.done.insert(session.to_string(), Done { resets_at, outcome: outcome.to_string(), at_ms: now_ms });
        }
    }

    /// Continues for this reset: waiting, asked about, or run.
    fn continues_for(&self, resets_at: i64) -> usize {
        let waiting = self.items.iter().filter(|i| i.kind == Kind::Continue && same_reset(i.resets_at, resets_at)).count();
        let ran = self.done.values().filter(|d| d.outcome == "ran" && same_reset(d.resets_at, resets_at)).count();
        waiting + ran
    }

    /// HeadsUp continued this session on its own at an earlier reset, recently.
    fn continued_recently(&self, session: &str, resets_at: i64, now_ms: i64) -> bool {
        self.done
            .get(session)
            .is_some_and(|d| d.outcome == "ran" && !same_reset(d.resets_at, resets_at) && now_ms - d.at_ms < CHAIN_MS)
    }

    fn prune(&mut self, now_ms: i64) {
        self.done.retain(|_, d| now_ms - d.at_ms < DONE_KEEP_MS);
    }
}

/// Read, change and (if it changed) save the book in state.json. Nothing slow inside `f`.
fn with_book<R>(shared: &Shared, f: impl FnOnce(&mut Book) -> R) -> R {
    let mut st = shared.state.lock().unwrap();
    let mut book = Book::load(&st);
    let before = book.clone();
    let r = f(&mut book);
    if book != before {
        book.store(&mut st);
        events::save_state(&st);
    }
    r
}

fn new_id() -> String {
    util::random_hex(6)
}

fn card_id(id: &str) -> String {
    format!("offer-{id}")
}

// ---------------------------------------------------------------- strings

pub struct S {
    offer: &'static str,
    will: &'static str,
    background: &'static str,
    yes: &'static str,
    no: &'static str,
    dont: &'static str,
    cancel: &'static str,
    send_now: &'static str,
    saved: &'static str,
    accepted: &'static str,
    declined: &'static str,
    cancelled: &'static str,
    expired: &'static str,
    late: &'static str,
    sent: &'static str,
    continued: &'static str,
    went_on: &'static str,
    archived: &'static str,
    no_transcript: &'static str,
    claude_code: &'static str,
    replaced: &'static str,
    held: &'static str,
    remote_off: &'static str,
    still: &'static str,
    gave_up: &'static str,
    gone: &'static str,
    starting: &'static str,
    queued: &'static str,
    appended: &'static str,
    too_long: &'static str,
    full: &'static str,
    remote_will: &'static str,
    pub again: &'static str,
    continuing: &'static str,
    continuing_body: &'static str,
    not_continued: &'static str,
    st_message: &'static str,
    st_continue: &'static str,
    st_offer: &'static str,
    pub stop_cleared: &'static str,
    /// The `{time}` of the templates above: today, tomorrow, another day.
    today: &'static str,
    tomorrow: &'static str,
    on_day: &'static str,
}

static EN: S = S {
    offer: "Continue {p} {time}?",
    will: "{p} continues {time}.",
    background: "It runs in the background; reopen the session to see it.",
    yes: "Continue {time}",
    no: "Don't",
    dont: "Don't continue",
    cancel: "Cancel",
    send_now: "Send now",
    saved: "Saved",
    accepted: "✓ Continues {time}",
    declined: "✗ Not continued",
    cancelled: "✗ Cancelled",
    expired: "⌛ Expired",
    late: "⌛ Not done: it was due more than 6 hours ago.",
    sent: "✉️ Sent",
    continued: "▶️ Continued",
    went_on: "↷ It went on after the limit already: not continued.",
    archived: "↷ The session is archived in the Claude app: not continued.",
    no_transcript: "↷ The session was not found: not continued.",
    claude_code: "↷ Claude Code continues it itself.",
    replaced: "↷ Your waiting message goes there instead.",
    held: "↷ The session changed meanwhile: see below.",
    remote_off: "✗ Remote sessions are off: not sent.",
    still: "⛔ Still limited ({limit}): moved, now {time}.",
    gave_up: "✗ Still limited ({limit}): given up.",
    gone: "That button is out of date.",
    starting: "It is on its way already.",
    queued: "⛔ {limit}. Your message for <b>{p}</b> will be sent {time}.",
    appended: "➕ Added to your waiting message for <b>{p}</b>: it goes {time}.",
    too_long: "Your waiting message is long enough already: this one was not added. Wait for the reset, or cancel it and send a new one.",
    full: "Too much is waiting for the limit reset already: this was not kept.",
    remote_will: "⏸ <b>{p}</b>: I'll continue this session {time}, when the limit resets.",
    again: "⛔ The limit was used up again: not continued a second time.",
    continuing: "Continuing in the background",
    continuing_body: "The usage limit reset. Reopen the session to see this turn.",
    not_continued: "Not continued",
    st_message: "✉️ <b>{p}</b> · waiting message · goes {time}",
    st_continue: "⏰ <b>{p}</b> · continues {time}",
    st_offer: "❔ <b>{p}</b> · continue {time}? (waiting for your answer)",
    stop_cleared: "Cancelled what waited for the limit reset ({n}).",
    today: "at {time}",
    tomorrow: "tomorrow at {time}",
    on_day: "on {date} at {time}",
};

static TR: S = S {
    offer: "{p}: {time} olunca devam etsin mi?",
    will: "{p}: {time} olunca devam edecek.",
    background: "Arka planda çalışır; görmek için oturumu yeniden aç.",
    yes: "Devam et · {time}",
    no: "Devam etme",
    dont: "Devam etme",
    cancel: "İptal",
    send_now: "Şimdi gönder",
    saved: "Kaydedildi",
    accepted: "✓ {time} olunca devam edecek",
    declined: "✗ Devam ettirilmeyecek",
    cancelled: "✗ İptal edildi",
    expired: "⌛ Süresi doldu",
    late: "⌛ Yapılmadı: zamanı 6 saatten fazla geçti.",
    sent: "✉️ Gönderildi",
    continued: "▶️ Devam ettirildi",
    went_on: "↷ Limitten sonra zaten devam etmiş: devam ettirilmedi.",
    archived: "↷ Oturum Claude uygulamasında arşivlenmiş: devam ettirilmedi.",
    no_transcript: "↷ Oturum bulunamadı: devam ettirilmedi.",
    claude_code: "↷ Claude Code buna kendisi devam ediyor.",
    replaced: "↷ Yerine bekleyen mesajın oraya gidecek.",
    held: "↷ Oturum bu arada değişti: aşağıya bak.",
    remote_off: "✗ Uzaktan oturum kapalı: gönderilmedi.",
    still: "⛔ Limit hâlâ dolu ({limit}): {time} saatine ertelendi.",
    gave_up: "✗ Limit hâlâ dolu ({limit}): vazgeçildi.",
    gone: "Bu buton eskidi.",
    starting: "Zaten yola çıktı.",
    queued: "⛔ {limit}. <b>{p}</b> için mesajın {time} olunca gönderilecek.",
    appended: "➕ <b>{p}</b> için bekleyen mesajına eklendi: {time} olunca gidecek.",
    too_long: "Bekleyen mesajın zaten yeterince uzun: bu eklenmedi. Yenilenmeyi bekle ya da onu iptal edip yenisini gönder.",
    full: "Limit yenilenmesini bekleyen çok fazla iş var: bu tutulmadı.",
    remote_will: "⏸ <b>{p}</b>: limit yenilenince, {time} olunca bu oturuma devam edeceğim.",
    again: "⛔ Limit yine doldu: ikinci kez devam ettirilmedi.",
    continuing: "Arka planda devam ediyor",
    continuing_body: "Kullanım limiti yenilendi. Bu turu görmek için oturumu yeniden aç.",
    not_continued: "Devam ettirilemedi",
    st_message: "✉️ <b>{p}</b> · bekleyen mesaj · {time}",
    st_continue: "⏰ <b>{p}</b> · {time} olunca devam edecek",
    st_offer: "❔ <b>{p}</b> · {time} olunca devam etsin mi? (cevabını bekliyor)",
    stop_cleared: "Limit yenilenmesini bekleyen {n} iş iptal edildi.",
    today: "{time}",
    tomorrow: "yarın {time}",
    on_day: "{date} {time}",
};

pub fn s(shared: &Shared) -> &'static S {
    if *shared.lang_code.lock().unwrap() == "tr" {
        &TR
    } else {
        &EN
    }
}

fn fill(template: &str, pairs: &[(&str, &str)]) -> String {
    let mut out = template.to_string();
    for (k, v) in pairs {
        out = out.replace(&format!("{{{k}}}"), v);
    }
    out
}

/// "at 18:40", "tomorrow at 09:00", "on 25.10 at 09:00" ("18:40", "yarın 09:00", "25.10 09:00"):
/// `at` in `now`'s time zone.
fn when_in<Tz: TimeZone>(x: &S, at: i64, now: &DateTime<Tz>) -> String
where
    Tz::Offset: std::fmt::Display,
{
    let Some(dt) = DateTime::<Utc>::from_timestamp(at, 0) else { return String::new() };
    let dt = dt.with_timezone(&now.timezone());
    let time = dt.format("%H:%M").to_string();
    let (day, today) = (dt.date_naive(), now.date_naive());
    if day == today {
        x.today.replace("{time}", &time)
    } else if today.succ_opt() == Some(day) {
        x.tomorrow.replace("{time}", &time)
    } else {
        x.on_day.replace("{date}", &dt.format("%d.%m").to_string()).replace("{time}", &time)
    }
}

fn when(x: &S, at: i64) -> String {
    when_in(x, at, &chrono::Local::now())
}

/// "18:40", or "25.10 18:40" on another day: for a button label.
fn when_short(at: i64) -> String {
    let Some(dt) = DateTime::<Utc>::from_timestamp(at, 0) else { return String::new() };
    let dt = dt.with_timezone(&chrono::Local);
    if dt.date_naive() == chrono::Local::now().date_naive() {
        dt.format("%H:%M").to_string()
    } else {
        dt.format("%d.%m %H:%M").to_string()
    }
}

fn esc(s: &str) -> String {
    phone::esc(s)
}

// ---------------------------------------------------------------- decisions (pure)

/// Does this used-up limit stop a run on `model` (remote::run_model; None: not known)? The
/// 5-hour and weekly limits, and one that names no model, stop every run; a per-model limit
/// (Opus, Sonnet, Fable...) only a run whose model names it. With the model not known it does
/// not hold the run: a run that hits it ends on the limit and is handled then.
pub fn applies(hit: &Hit, model: Option<&str>) -> bool {
    let name = match hit.window.as_str() {
        "five_hour" | "seven_day" => return true,
        "seven_day_opus" => "opus".to_string(),
        "seven_day_sonnet" => "sonnet".to_string(),
        "seven_day_overage_included" => hit.model.as_deref().unwrap_or("fable").to_string(),
        _ => match hit.model.as_deref() {
            None => return true,
            Some(m) => m.to_string(),
        },
    };
    // "Fable 5" -> "fable": model ids and aliases ("claude-fable-5", "fable") carry the first word.
    let name = name.split_whitespace().next().unwrap_or("").to_lowercase();
    !name.is_empty() && model.is_some_and(|m| m.to_lowercase().contains(&name))
}

/// When the limits in force that stop a run on `model` let work go again (unix s; the reset that
/// comes last), and that limit. `own_window`: a limit that stops it whatever the model (the one
/// a continue waits for; "" for none). None when none is in force. A limit whose reset is not
/// known stands an hour.
pub fn blocked_until(active: &[limits::Active], now_ms: i64, model: Option<&str>, own_window: &str) -> Option<(i64, Hit)> {
    let now = now_ms / 1000;
    active
        .iter()
        .filter(|a| applies(&a.hit, model) || (!own_window.is_empty() && a.hit.window == own_window))
        .map(|a| (a.hit.resets_at.unwrap_or((a.alerted_ms + limits::UNKNOWN_RESET_MS) / 1000), &a.hit))
        .filter(|(at, _)| *at > now)
        .max_by_key(|(at, _)| *at)
        .map(|(at, h)| (at, h.clone()))
}

/// Seconds after the reset an item runs. A message for a session that stopped on the PC (its
/// `entrypoint` is that session's launcher) waits as long as a continue of it would: Claude
/// Code's or the Claude app's own continue goes first, and the busy guard then holds the message.
fn delay(kind: Kind, source: Source, entrypoint: &str) -> i64 {
    let pc = match kind {
        Kind::Continue => source == Source::Pc,
        Kind::Message => !entrypoint.is_empty() && entrypoint != "remote",
    };
    if !pc {
        return MESSAGE_DELAY;
    }
    match entrypoint {
        "cli" => CLI_DELAY,
        "sdk-cli" => SDK_DELAY,
        _ => DESKTOP_DELAY,
    }
}

/// `limits.continueAtReset`: "ask" (default), "always" or "off".
fn mode(cfg: &Value) -> &'static str {
    match config::get_str(cfg, "limits.continueAtReset").as_deref().map(str::trim) {
        Some(m) if m.eq_ignore_ascii_case("always") => "always",
        Some(m) if m.eq_ignore_ascii_case("off") => "off",
        _ => "ask",
    }
}

/// What to do about a session on the PC that stopped on a used-up limit.
#[derive(Debug, PartialEq)]
pub enum PcPlan {
    /// Ask whether to continue it at the reset.
    Offer,
    /// Continue it at the reset.
    Schedule,
    /// Leave it (why, for the log).
    Skip(&'static str),
}

pub struct PcFacts<'a> {
    pub mode: &'a str,
    pub entrypoint: &'a str,
    /// Claude Code's own "Continue automatically at usage limit" is on (it matters for "cli").
    pub claude_code_continues: bool,
    pub resets_at: Option<i64>,
    pub session_id: &'a str,
    pub known_project: bool,
    /// A PC card or a phone message can carry the question.
    pub can_ask: bool,
    pub now_ms: i64,
}

pub fn plan_pc(f: &PcFacts, book: &Book) -> PcPlan {
    let now = f.now_ms / 1000;
    if f.mode == "off" {
        return PcPlan::Skip("off");
    }
    if f.session_id.is_empty() {
        return PcPlan::Skip("no session id");
    }
    let Some(at) = f.resets_at else { return PcPlan::Skip("reset not known") };
    if at <= now {
        return PcPlan::Skip("reset passed");
    }
    if at - now > MAX_AHEAD {
        return PcPlan::Skip("reset too far ahead");
    }
    // An interactive CLI session continues by itself (Claude Code's own setting, on by default).
    if f.entrypoint == "cli" && f.claude_code_continues {
        return PcPlan::Skip("claude code continues it");
    }
    if book.has_session(f.session_id) {
        return PcPlan::Skip("planned already");
    }
    if book.done_for(f.session_id, at) {
        return PcPlan::Skip("decided already");
    }
    if book.continues_for(at) >= MAX_CONTINUES || book.items.len() >= MAX_ITEMS {
        return PcPlan::Skip("cap");
    }
    if !f.known_project {
        return PcPlan::Skip("unknown project");
    }
    let chain = book.continued_recently(f.session_id, at, f.now_ms);
    match (f.mode, chain) {
        ("always", false) => PcPlan::Schedule,
        _ if f.can_ask => PcPlan::Offer,
        _ => PcPlan::Skip(if chain { "continued at the last reset" } else { "nobody to ask" }),
    }
}

/// What queueing a message did.
#[derive(Debug, PartialEq)]
pub enum Queued {
    New(String),
    /// Added to the message already waiting for the same target; that one's notice on the phone
    /// (id, html), whose buttons go.
    Appended { id: String, notice: Option<(i64, String)> },
    TooLong,
    Full,
}

fn same_target(a: &Item, b: &Item) -> bool {
    if a.session_id.is_empty() && b.session_id.is_empty() {
        same_dir(&a.cwd, &b.cwd)
    } else {
        a.session_id == b.session_id
    }
}

/// Keep `item` (a message) in the book: appended to one waiting for the same target, else new.
pub fn queue_into(book: &mut Book, item: Item) -> Queued {
    let existing = book
        .items
        .iter_mut()
        .find(|q| q.kind == Kind::Message && q.status != Status::Starting && same_target(q, &item));
    if let Some(q) = existing {
        if q.prompt.chars().count() + 2 + item.prompt.chars().count() > MAX_PROMPT_CHARS {
            return Queued::TooLong;
        }
        q.prompt = format!("{}\n\n{}", q.prompt, item.prompt);
        // A limit found in force since may hold it longer.
        q.resets_at = q.resets_at.max(item.resets_at);
        q.due_at = q.due_at.max(item.due_at);
        q.target_gen = item.target_gen;
        let notice = q.phone_mid.take().map(|m| (m, std::mem::take(&mut q.phone_html)));
        return Queued::Appended { id: q.id.clone(), notice };
    }
    if item.prompt.chars().count() > MAX_PROMPT_CHARS {
        return Queued::TooLong;
    }
    if book.items.len() >= MAX_ITEMS {
        return Queued::Full;
    }
    let id = item.id.clone();
    book.items.push(item);
    Queued::New(id)
}

/// A message now waits for `session`: its continues give way to it.
fn supersede(book: &mut Book, session: &str, now_ms: i64) -> Vec<Item> {
    let mut out = Vec::new();
    book.items.retain(|i| {
        let gone = i.kind == Kind::Continue && i.session_id == session && i.status != Status::Starting;
        if gone {
            out.push(i.clone());
        }
        !gone
    });
    for i in &out {
        book.mark(session, i.resets_at, "replaced", now_ms);
    }
    out
}

#[derive(Debug, PartialEq)]
pub enum Gone {
    /// Asked about, never answered.
    Expired,
    /// Due long ago (the PC was off).
    Late,
}

/// Drop what can no longer run.
fn expire(book: &mut Book, now: i64) -> Vec<(Item, Gone)> {
    let mut out = Vec::new();
    book.items.retain(|i| {
        let why = match i.status {
            Status::Offered if now > i.resets_at + OFFER_GRACE => Some(Gone::Expired),
            Status::Scheduled if now > i.due_at + STALE => Some(Gone::Late),
            _ => None,
        };
        match why {
            Some(w) => {
                out.push((i.clone(), w));
                false
            }
            None => true,
        }
    });
    let now_ms = now * 1000;
    for (i, _) in &out {
        if i.kind == Kind::Continue {
            book.mark(&i.session_id, i.resets_at, "skipped", now_ms);
        }
    }
    out
}

/// After the PC woke at `woke_ms`, how long this item waits before it may start.
fn wake_wait_ms(i: &Item) -> i64 {
    if i.kind == Kind::Continue && i.source == Source::Pc && delay(i.kind, i.source, &i.entrypoint) == DESKTOP_DELAY {
        DESKTOP_DELAY * 1000
    } else {
        WAKE_DELAY_MS
    }
}

/// The item due first, marked Starting.
fn take_due(book: &mut Book, now_ms: i64, woke_ms: i64) -> Option<Item> {
    let now = now_ms / 1000;
    let next = book
        .items
        .iter_mut()
        .filter(|i| i.status == Status::Scheduled && i.due_at <= now && (woke_ms == 0 || now_ms >= woke_ms + wake_wait_ms(i)))
        .min_by_key(|i| i.due_at)?;
    next.status = Status::Starting;
    Some(next.clone())
}

/// A record that reports a used-up usage limit.
fn limit_record(r: &Value) -> bool {
    r.get("isApiErrorMessage").and_then(Value::as_bool) == Some(true)
        && (r.get("apiError").and_then(Value::as_str) == Some("usage_limit_reached")
            || limits::parse_message(&limits::content_text(r.pointer("/message/content"))).is_some())
}

/// The start of a user record of a local slash command (/model, /usage, /cost...) or its output.
const LOCAL_COMMAND: [&str; 5] =
    ["<command-name>", "<command-message>", "<local-command-stdout>", "<local-command-stderr>", "<local-command-caveat>"];

/// Did the session go on after it stopped on the limit (at `stopped_ms`)? `tail`: the end of its
/// transcript. Yes when anything was sent after the last limit record: a prompt (typed, the
/// Claude app's or Claude Code's own continue, a queued one) or an answer; a local slash command
/// is not. Without a limit record in the tail, yes when such a record is newer than the stop.
pub fn went_on(tail: &str, stopped_ms: i64) -> bool {
    let records: Vec<Value> = tail.lines().filter_map(|l| serde_json::from_str::<Value>(l.trim()).ok()).collect();
    let moved = |r: &Value| match r.get("type").and_then(Value::as_str) {
        Some("assistant" | "queue-operation") => true,
        Some("user") => {
            let t = limits::content_text(r.pointer("/message/content"));
            let t = t.trim_start();
            !LOCAL_COMMAND.iter().any(|p| t.starts_with(p))
        }
        _ => false,
    };
    match records.iter().rposition(limit_record) {
        Some(pos) => records[pos + 1..].iter().any(moved),
        None => records.iter().filter(|r| moved(r)).any(|r| {
            r.get("timestamp")
                .and_then(Value::as_str)
                .and_then(|t| DateTime::parse_from_rfc3339(t).ok())
                .is_some_and(|t| t.timestamp_millis() > stopped_ms + 2000)
        }),
    }
}

/// The Claude app's own auto-resume store (ccd-limit-auto-resume.json): does it hold the desktop
/// session `local_id` of `account` ("account/org") armed for a reset near `resets_at`?
pub fn desktop_armed_in(store: &Value, account: &str, local_id: &str, resets_at: i64) -> bool {
    let Some(w) = store.get("windows").and_then(|w| w.get(account)).and_then(|a| a.get(local_id)) else { return false };
    let num = |k: &str| w.get(k).and_then(Value::as_i64);
    w.get("armed").and_then(Value::as_bool) == Some(true)
        && w.get("optedIn").and_then(Value::as_bool) == Some(true)
        && num("attempt").is_some_and(|a| a < 3)
        && num("resetsAt").is_some_and(|r| (r - resets_at).abs() < 6 * 3600)
}

/// A Claude Code setting that defaults to on, from settings files in precedence order (the
/// first that sets it wins).
pub fn setting_on(files: &[Value], key: &str) -> bool {
    files.iter().find_map(|v| v.get(key).and_then(Value::as_bool)).unwrap_or(true)
}

// ---------------------------------------------------------------- what Claude does itself

/// Claude Code's own "Continue automatically at usage limit" (`autoContinueAtUsageLimit`, on by
/// default) for a session in `cwd`: managed settings, the project's local and shared settings,
/// then the user's.
fn claude_code_continues(cwd: &str) -> bool {
    let env = |k: &str| std::env::var_os(k).map(PathBuf::from);
    let mut files: Vec<PathBuf> = Vec::new();
    for base in ["ProgramData", "ProgramFiles"] {
        if let Some(b) = env(base) {
            files.push(b.join("ClaudeCode").join("managed-settings.json"));
        }
    }
    if !cwd.is_empty() {
        let dir = Path::new(cwd).join(".claude");
        files.push(dir.join("settings.local.json"));
        files.push(dir.join("settings.json"));
    }
    files.push(remote::claude_config_dir().join("settings.json"));
    let values: Vec<Value> = files.iter().filter_map(|p| util::read_json(p)).collect();
    setting_on(&values, "autoContinueAtUsageLimit")
}

/// A Code-tab session of the Claude app: where the app keeps its data, the session's own id
/// there, its account key ("account/org"), and whether it is archived.
struct DesktopSession {
    root: PathBuf,
    account: String,
    local_id: String,
    archived: bool,
}

/// The Claude app's data folders: the Store app's (%LOCALAPPDATA%\Claude-Data\<package>) and
/// the classic installer's (%APPDATA%\Claude).
fn desktop_roots() -> Vec<PathBuf> {
    let env = |k: &str| std::env::var_os(k).map(PathBuf::from);
    let mut out = Vec::new();
    if let Some(l) = env("LOCALAPPDATA") {
        if let Ok(rd) = std::fs::read_dir(l.join("Claude-Data")) {
            out.extend(rd.flatten().map(|e| e.path()).filter(|p| p.is_dir()));
        }
    }
    if let Some(a) = env("APPDATA") {
        out.push(a.join("Claude"));
    }
    out
}

/// The Claude app's record of the Claude Code session `session_id` (claude-code-sessions\
/// <account>\<org>\local_<id>.json, whose cliSessionId names it). Best effort.
fn desktop_session(session_id: &str) -> Option<DesktopSession> {
    if session_id.is_empty() {
        return None;
    }
    let name_of = |p: &Path| p.file_name().and_then(|n| n.to_str()).unwrap_or("").to_string();
    let mut looked = 0usize;
    for root in desktop_roots() {
        let Ok(accounts) = std::fs::read_dir(root.join("claude-code-sessions")) else { continue };
        for acct in accounts.flatten() {
            let Ok(orgs) = std::fs::read_dir(acct.path()) else { continue };
            for org in orgs.flatten() {
                let Ok(files) = std::fs::read_dir(org.path()) else { continue };
                for f in files.flatten() {
                    let p = f.path();
                    let name = name_of(&p);
                    if !name.starts_with("local_") || !name.ends_with(".json") {
                        continue;
                    }
                    looked += 1;
                    if looked > 3000 {
                        return None;
                    }
                    if f.metadata().map(|m| m.len() > 8_000_000).unwrap_or(true) {
                        continue;
                    }
                    let Ok(text) = std::fs::read_to_string(&p) else { continue };
                    if !text.contains(session_id) {
                        continue;
                    }
                    let Ok(v) = serde_json::from_str::<Value>(&text) else { continue };
                    if v.get("cliSessionId").and_then(Value::as_str) != Some(session_id) {
                        continue;
                    }
                    return Some(DesktopSession {
                        root: root.clone(),
                        account: format!("{}/{}", name_of(&acct.path()), name_of(&org.path())),
                        local_id: name.trim_end_matches(".json").to_string(),
                        archived: v.get("isArchived").and_then(Value::as_bool) == Some(true),
                    });
                }
            }
        }
    }
    None
}

fn desktop_armed(ds: &DesktopSession, resets_at: i64) -> bool {
    util::read_json(&ds.root.join("ccd-limit-auto-resume.json")).is_some_and(|v| desktop_armed_in(&v, &ds.account, &ds.local_id, resets_at))
}

// ---------------------------------------------------------------- the phone and the PC card

/// A continue planned for a session on the PC, for its PC card and phone message.
pub struct Plan {
    pub id: String,
    /// Asked about (else scheduled).
    pub offered: bool,
    pub project: String,
    pub due_at: i64,
}

/// "Continue alpha at 18:44? It runs in the background; reopen the session to see it."
pub fn plan_line(shared: &Shared, p: &Plan) -> String {
    let x = s(shared);
    let time = when(x, p.due_at);
    let head = fill(if p.offered { x.offer } else { x.will }, &[("p", &p.project), ("time", &time)]);
    format!("{head} {}", x.background)
}

pub fn plan_buttons(shared: &Shared, p: &Plan) -> Vec<Vec<(String, String)>> {
    let x = s(shared);
    if p.offered {
        let yes = x.yes.replace("{time}", &when(x, p.due_at));
        vec![vec![(yes, format!("lc:{}", p.id)), (x.no.to_string(), format!("ln:{}", p.id))]]
    } else {
        vec![vec![(x.dont.to_string(), format!("ln:{}", p.id))]]
    }
}

/// The PC card of a planned continue: Continue / Don't, or (scheduled) Don't continue.
pub fn show_card(app: &AppHandle, shared: &Arc<Shared>, subtitle: &str, body: &str, p: &Plan) {
    let x = s(shared);
    let actions: Vec<(String, bool, bool)> = if p.offered {
        vec![(x.no.to_string(), false, false), (x.yes.replace("{time}", &when(x, p.due_at)), true, true)]
    } else {
        vec![(x.dont.to_string(), false, false)]
    };
    let wait = if p.offered { OFFER_CARD_MS } else { NOTICE_CARD_MS };
    events::show_offer(app, shared, &card_id(&p.id), subtitle, body, &actions, x.saved, wait);
    with_book(shared, |b| {
        if let Some(i) = b.get_mut(&p.id) {
            i.pc_card = true;
        }
    });
}

/// The phone message about an item went out as `mid`.
pub fn note_phone_msg(shared: &Shared, id: &str, mid: i64, html: &str) {
    with_book(shared, |b| {
        if let Some(i) = b.get_mut(id) {
            i.phone_mid = Some(mid);
            i.phone_html = html.to_string();
        }
    });
}

/// The buttons an item's phone message carries now.
fn buttons_for(x: &S, i: &Item) -> Vec<Vec<(String, String)>> {
    match (i.kind, i.status) {
        (_, Status::Offered) => vec![vec![
            (x.yes.replace("{time}", &when(x, i.due_at)), format!("lc:{}", i.id)),
            (x.no.to_string(), format!("ln:{}", i.id)),
        ]],
        (Kind::Message, _) => vec![vec![(x.send_now.to_string(), format!("ls:{}", i.id)), (x.cancel.to_string(), format!("ln:{}", i.id))]],
        _ => vec![vec![(x.cancel.to_string(), format!("ln:{}", i.id))]],
    }
}

/// Telegram reaches the user about this item: the paired chat for what the phone asked for; for
/// a session of the PC, only while the phone is set up to answer and gets limit news.
fn phone_on(shared: &Shared, i: &Item) -> bool {
    let cfg = shared.cfg();
    match i.source {
        Source::Pc => phone::answers_cards(shared) && config::get_bool(&cfg, "phone.events.limitHit", true),
        _ => telegram::creds(&cfg).is_some_and(|c| !c.chat_id.is_empty()),
    }
}

/// The item is done with (run, cancelled, dropped): its PC card closes and its phone message
/// gets `footer` instead of its buttons.
fn close(app: Option<&AppHandle>, shared: &Arc<Shared>, i: &Item, footer: &str) {
    if let (Some(app), true) = (app, i.pc_card) {
        events::resolve_card(app, shared, &card_id(&i.id), true);
    }
    if let Some(mid) = i.phone_mid {
        let html = telegram::with_footer(&i.phone_html, footer);
        let shared = shared.clone();
        std::thread::spawn(move || telegram::edit(&shared, mid, &html));
    }
}

/// A new phone message about a waiting item (with its buttons); the one before keeps its text
/// with `old_footer` and no buttons.
fn post(shared: &Arc<Shared>, i: &Item, html: String, old_footer: &str) {
    if !phone_on(shared, i) {
        return;
    }
    if let Some(mid) = i.phone_mid {
        telegram::edit(shared, mid, &telegram::with_footer(&i.phone_html, old_footer));
    }
    let buttons = buttons_for(s(shared), i);
    if let Some(mid) = remote::send_msg(shared, &html, &buttons) {
        note_phone_msg(shared, &i.id, mid, &html);
    }
}

/// The phone's /status: one line per waiting item, and a row of buttons for each.
pub fn status(shared: &Shared) -> (Vec<String>, Vec<Vec<(String, String)>>) {
    let x = s(shared);
    let mut items = with_book(shared, |b| b.items.clone());
    items.retain(|i| i.status != Status::Starting);
    items.sort_by_key(|i| i.due_at);
    let mut lines = Vec::new();
    let mut buttons = Vec::new();
    for i in items {
        let time = when(x, i.due_at);
        let template = match (i.kind, i.status) {
            (_, Status::Offered) => x.st_offer,
            (Kind::Message, _) => x.st_message,
            _ => x.st_continue,
        };
        lines.push(fill(template, &[("p", &esc(&i.project)), ("time", &time)]));
        let label = util::truncate(&crate::mask::secrets(&format!("{} · {}", i.project, when_short(i.due_at))), 40);
        if i.status == Status::Offered {
            buttons.push(vec![(format!("✓ {label}"), format!("lc:{}", i.id)), (format!("✗ {}", x.no), format!("ln:{}", i.id))]);
        } else {
            buttons.push(vec![(format!("✗ {label}"), format!("ln:{}", i.id))]);
        }
    }
    (lines, buttons)
}

// ---------------------------------------------------------------- planning

/// A session on the PC stopped on a used-up limit (StopFailure; limits.rs). Plans its continue
/// at the reset per `limits.continueAtReset` and returns it for the alert's card and message.
/// `cli_off`: Claude Code said it will not continue this session itself.
pub fn on_pc_stop(shared: &Arc<Shared>, hit: &Hit, w: &Waiter, cli_off: bool) -> Option<Plan> {
    let cfg = shared.cfg();
    let mode = mode(&cfg);
    if mode == "off" {
        return None;
    }
    // Files are read here, before the book is locked.
    let known_project = !w.cwd.is_empty() && remote::is_known_project(&cfg, &w.cwd);
    let claude_code = !cli_off && w.entrypoint == "cli" && claude_code_continues(&w.cwd);
    let can_ask = config::get_bool(&cfg, "events.limit_hit", true)
        || (phone::answers_cards(shared) && config::get_bool(&cfg, "phone.events.limitHit", true));
    let title = remote::read_head(Path::new(&w.transcript_path)).map(|(_, t)| t).unwrap_or_default();
    let project = w.project();
    let now_ms = util::now_ms();
    let facts = PcFacts {
        mode,
        entrypoint: &w.entrypoint,
        claude_code_continues: claude_code,
        resets_at: hit.resets_at,
        session_id: &w.session_id,
        known_project,
        can_ask,
        now_ms,
    };
    let (decision, plan) = with_book(shared, |b| {
        let decision = plan_pc(&facts, b);
        let offered = match decision {
            PcPlan::Offer => true,
            PcPlan::Schedule => false,
            PcPlan::Skip(_) => return (decision, None),
        };
        let at = hit.resets_at.unwrap_or(0);
        let item = Item {
            id: new_id(),
            kind: Kind::Continue,
            source: Source::Pc,
            status: if offered { Status::Offered } else { Status::Scheduled },
            cwd: w.cwd.clone(),
            project: project.clone(),
            session_id: w.session_id.clone(),
            title,
            window: hit.window.clone(),
            resets_at: at,
            due_at: at + delay(Kind::Continue, Source::Pc, &w.entrypoint),
            created_ms: now_ms,
            entrypoint: w.entrypoint.clone(),
            permission_mode: w.permission_mode.clone(),
            transcript_path: w.transcript_path.clone(),
            ..Item::default()
        };
        let plan = Plan { id: item.id.clone(), offered, project: project.clone(), due_at: item.due_at };
        b.items.push(item);
        (decision, Some(plan))
    });
    let what = match decision {
        PcPlan::Offer => "offer",
        PcPlan::Schedule => "scheduled",
        PcPlan::Skip(why) => why,
    };
    util::log_line(&["schedule", "pc", what, &project, if w.entrypoint.is_empty() { "-" } else { &w.entrypoint }]);
    plan
}

/// A message from the phone for `target` while a usage limit is in force: it waits for the
/// reset (appended to one already waiting for the same target) and the phone is told when it
/// goes, with Send now / Cancel. False when no limit is in force (the caller sends it now).
pub fn queue_if_limited(app: &AppHandle, shared: &Arc<Shared>, target: &Target, text: &str) -> bool {
    let now_ms = util::now_ms();
    let active = limits::active(shared);
    let model = remote::run_model(&shared.cfg());
    let Some((until, hit)) = blocked_until(&active, now_ms, model.as_deref(), "") else { return false };
    let x = s(shared);
    let (cwd, project, session_id, title) = match target {
        Target::New { cwd, project } => (cwd.clone(), project.clone(), String::new(), String::new()),
        Target::Resume { id, cwd, project, title } => (cwd.clone(), project.clone(), id.clone(), title.clone()),
    };
    // A session that stopped on the PC: the message goes after Claude Code's or the Claude app's
    // own continue of it (the busy guard then holds it while that turn runs).
    let entrypoint = active
        .iter()
        .flat_map(|a| a.waiters.iter())
        .find(|w| !session_id.is_empty() && w.session_id == session_id)
        .map(|w| if w.entrypoint.is_empty() { "pc".to_string() } else { w.entrypoint.clone() })
        .unwrap_or_default();
    let wait = delay(Kind::Message, Source::Phone, &entrypoint);
    let item = Item {
        id: new_id(),
        kind: Kind::Message,
        source: Source::Phone,
        status: Status::Scheduled,
        cwd,
        project: project.clone(),
        session_id: session_id.clone(),
        title,
        prompt: text.to_string(),
        window: hit.window.clone(),
        resets_at: until,
        due_at: until + wait,
        created_ms: now_ms,
        entrypoint,
        target_gen: remote::target_gen(shared),
        ..Item::default()
    };
    let (queued, replaced, due) = with_book(shared, |b| {
        let q = queue_into(b, item);
        let id = match &q {
            Queued::New(id) | Queued::Appended { id, .. } => Some(id.clone()),
            _ => None,
        };
        let replaced = if id.is_some() && !session_id.is_empty() { supersede(b, &session_id, now_ms) } else { Vec::new() };
        let due = id.and_then(|id| b.find(&id).map(|i| i.due_at)).unwrap_or(until + wait);
        (q, replaced, due)
    });
    for r in &replaced {
        util::log_line(&["schedule", "replaced", &r.project]);
        close(Some(app), shared, r, x.replaced);
    }
    let label = limits::label(shared.t(), &hit.window, hit.model.as_deref());
    let (id, template, what) = match queued {
        Queued::New(id) => (id, x.queued, "queued"),
        Queued::Appended { id, notice } => {
            if let Some((mid, html)) = notice {
                let shared = shared.clone();
                std::thread::spawn(move || telegram::edit(&shared, mid, &telegram::with_footer(&html, "➕")));
            }
            (id, x.appended, "appended")
        }
        Queued::TooLong => {
            remote::send_msg(shared, x.too_long, &[]);
            return true;
        }
        Queued::Full => {
            remote::send_msg(shared, x.full, &[]);
            return true;
        }
    };
    util::log_line(&["schedule", what, &project, &hit.window, &due.to_string()]);
    let html = fill(template, &[("limit", &esc(&label)), ("p", &esc(&project)), ("time", &when(x, due))]);
    let buttons = vec![vec![(x.send_now.to_string(), format!("ls:{id}")), (x.cancel.to_string(), format!("ln:{id}"))]];
    if let Some(mid) = remote::send_msg(shared, &html, &buttons) {
        note_phone_msg(shared, &id, mid, &html);
    }
    true
}

/// A remote run on `session_id` ended because the usage limit ran out: continue it at the reset
/// (`remote.continueAtReset`), with Cancel on the phone.
pub fn after_remote_limit(shared: &Arc<Shared>, session_id: &str, cwd: &str, project: &str, title: &str, hit: &Hit) {
    if !config::get_bool(&shared.cfg(), "remote.continueAtReset", true) {
        util::log_line(&["schedule", "remote", "off", project]);
        return;
    }
    let now_ms = util::now_ms();
    let now = now_ms / 1000;
    let at = hit
        .resets_at
        .or_else(|| limits::active(shared).into_iter().find(|a| a.hit.window == hit.window).and_then(|a| a.hit.resets_at));
    // A reset that is (nearly) here would only run into the same limit again.
    let Some(at) = at.filter(|at| *at > now + 30 && *at - now <= MAX_AHEAD) else {
        util::log_line(&["schedule", "remote", "no usable reset", project]);
        return;
    };
    let item = Item {
        id: new_id(),
        kind: Kind::Continue,
        source: Source::Remote,
        status: Status::Scheduled,
        cwd: cwd.to_string(),
        project: project.to_string(),
        session_id: session_id.to_string(),
        title: title.to_string(),
        window: hit.window.clone(),
        resets_at: at,
        due_at: at + MESSAGE_DELAY,
        created_ms: now_ms,
        entrypoint: "remote".to_string(),
        ..Item::default()
    };
    let added = with_book(shared, |b| {
        if b.has_session(session_id) || b.done_for(session_id, at) || b.continues_for(at) >= MAX_CONTINUES || b.items.len() >= MAX_ITEMS {
            return None;
        }
        b.items.push(item.clone());
        Some(item.clone())
    });
    let Some(item) = added else {
        util::log_line(&["schedule", "remote", "skipped", project]);
        return;
    };
    util::log_line(&["schedule", "remote", "scheduled", project, &item.due_at.to_string()]);
    let x = s(shared);
    let html = fill(x.remote_will, &[("p", &esc(project)), ("time", &when(x, item.due_at))]);
    post(shared, &item, html, "");
}

// ---------------------------------------------------------------- answers

/// The user said to continue: an item asked about is scheduled.
fn accept(app: &AppHandle, shared: &Arc<Shared>, id: &str) -> String {
    let x = s(shared);
    let now = util::now_secs();
    let r = with_book(shared, |b| {
        let i = b.get_mut(id)?;
        let changed = i.status == Status::Offered;
        if changed {
            i.status = Status::Scheduled;
            i.due_at = i.due_at.max(now + 5);
        }
        Some((i.clone(), changed))
    });
    let Some((i, changed)) = r else { return x.gone.to_string() };
    if i.pc_card {
        events::resolve_card(app, shared, &card_id(&i.id), true);
    }
    if changed {
        util::log_line(&["schedule", "accepted", &i.project]);
        if let Some(mid) = i.phone_mid {
            let html = telegram::with_footer(&i.phone_html, &x.accepted.replace("{time}", &when(x, i.due_at)));
            let buttons = buttons_for(x, &i);
            note_phone_msg(shared, &i.id, mid, &html);
            let shared = shared.clone();
            std::thread::spawn(move || {
                telegram::edit_with_buttons(&shared, mid, &html, &buttons);
            });
        }
    }
    "✓".to_string()
}

/// The user said no, or cancelled: the item goes (one being started cannot).
fn cancel(app: &AppHandle, shared: &Arc<Shared>, id: &str) -> String {
    let x = s(shared);
    let now_ms = util::now_ms();
    let r = with_book(shared, |b| match b.find(id).map(|i| i.status) {
        None => Ok(None),
        Some(Status::Starting) => Err(()),
        Some(_) => {
            let i = b.take(id).expect("found above");
            if i.kind == Kind::Continue {
                let outcome = if i.status == Status::Offered { "declined" } else { "cancelled" };
                b.mark(&i.session_id, i.resets_at, outcome, now_ms);
            }
            Ok(Some(i))
        }
    });
    match r {
        Err(()) => x.starting.to_string(),
        Ok(None) => x.gone.to_string(),
        Ok(Some(i)) => {
            util::log_line(&["schedule", "cancelled", &i.project]);
            let footer = if i.status == Status::Offered { x.declined } else { x.cancelled };
            close(Some(app), shared, &i, footer);
            footer.to_string()
        }
    }
}

/// Send a waiting message now, limit or not (extra usage may be on, or another model free).
fn send_now(app: &AppHandle, shared: &Arc<Shared>, id: &str) -> String {
    let x = s(shared);
    if !remote::enabled(shared) {
        return remote::s(shared).disabled.to_string();
    }
    if remote::busy(shared) {
        return remote::s(shared).busy.to_string();
    }
    let item = with_book(shared, |b| {
        let i = b.get_mut(id).filter(|i| i.kind == Kind::Message && i.status == Status::Scheduled)?;
        i.status = Status::Starting;
        Some(i.clone())
    });
    let Some(item) = item else { return x.gone.to_string() };
    util::log_line(&["schedule", "send-now", &item.project]);
    // As if it had just been written: a session in use on the PC asks first (and its Send anyway
    // sends it, limit or not).
    if !item.session_id.is_empty() {
        if let Some(age_ms) = remote::changed_since(shared, &item.session_id, 0) {
            with_book(shared, |b| b.take(&item.id));
            util::log_line(&["schedule", "held", &item.project]);
            close(Some(app), shared, &item, x.held);
            remote::hold_prompt(shared, target_of(&item), item.prompt.clone(), age_ms, true);
            return "✓".to_string();
        }
    }
    let gen = Some(remote::target_gen(shared)).filter(|g| *g == item.target_gen);
    match remote::start(app, shared, target_of(&item), item.prompt.clone(), Origin::Phone, gen, None) {
        Start::Busy => {
            requeue(shared, &item.id, item.due_at);
            remote::s(shared).busy.to_string()
        }
        Start::Started => {
            with_book(shared, |b| b.take(&item.id));
            close(Some(app), shared, &item, x.sent);
            "✓".to_string()
        }
        Start::Refused(why) => {
            with_book(shared, |b| b.take(&item.id));
            close(Some(app), shared, &item, x.cancelled);
            util::truncate(&why, 150)
        }
    }
}

/// A button of a phone message ("lc:<id>" continue, "ln:<id>" no / cancel, "ls:<id>" send
/// now). Returns the text for Telegram's toast.
pub fn on_callback(app: &AppHandle, shared: &Arc<Shared>, data: &str) -> Option<String> {
    let (kind, id) = data.split_once(':')?;
    util::log_line(&["schedule-cb", kind]);
    match kind {
        "lc" => Some(accept(app, shared, id)),
        "ln" => Some(cancel(app, shared, id)),
        "ls" => Some(send_now(app, shared, id)),
        _ => None,
    }
}

/// A button of the PC card (`id` without its "offer-").
pub fn answer_card(app: &AppHandle, shared: &Arc<Shared>, id: &str, yes: bool) {
    if id == "test" {
        events::resolve_card(app, shared, &card_id(id), true);
        return;
    }
    let toast = if yes { accept(app, shared, id) } else { cancel(app, shared, id) };
    // Answered elsewhere first (or gone): the card still closes.
    if toast == s(shared).gone {
        events::resolve_card(app, shared, &card_id(id), true);
    }
}

/// /stop: everything waiting is cancelled. How many.
pub fn cancel_all(app: &AppHandle, shared: &Arc<Shared>) -> usize {
    let now_ms = util::now_ms();
    let gone = with_book(shared, |b| {
        let (keep, gone): (Vec<Item>, Vec<Item>) = b.items.drain(..).partition(|i| i.status == Status::Starting);
        b.items = keep;
        for i in gone.iter().filter(|i| i.kind == Kind::Continue) {
            b.mark(&i.session_id, i.resets_at, "cancelled", now_ms);
        }
        gone
    });
    let x = s(shared);
    for i in &gone {
        close(Some(app), shared, i, x.cancelled);
    }
    if !gone.is_empty() {
        util::log_line(&["schedule", "cancelled-all", &gone.len().to_string()]);
    }
    gone.len()
}

/// Remote sessions were switched off: what the phone asked for goes (sessions of the PC stay).
pub fn drop_remote(shared: &Arc<Shared>) {
    let gone = with_book(shared, |b| {
        let (gone, keep): (Vec<Item>, Vec<Item>) =
            b.items.drain(..).partition(|i| i.status != Status::Starting && (i.kind == Kind::Message || i.source == Source::Remote));
        b.items = keep;
        gone
    });
    let x = s(shared);
    for i in &gone {
        close(None, shared, i, x.remote_off);
    }
    if !gone.is_empty() {
        util::log_line(&["schedule", "remote-off", &gone.len().to_string()]);
    }
}

/// Claude Code's own continue-at-reset reported on a session (Notification
/// quota_auto_resume_*): fired or waiting for Enter (stale) means it is Claude Code's; disabled
/// means it will not, so HeadsUp may.
pub fn on_quota_notice(app: &AppHandle, shared: &Arc<Shared>, event: &Value) {
    let nt = event.get("notification_type").and_then(Value::as_str).unwrap_or("");
    let sid = event.get("session_id").and_then(Value::as_str).unwrap_or("");
    if sid.is_empty() {
        return;
    }
    match nt {
        "quota_auto_resume_fired" | "quota_auto_resume_stale" => {
            let now_ms = util::now_ms();
            let gone = with_book(shared, |b| {
                let (gone, keep): (Vec<Item>, Vec<Item>) =
                    b.items.drain(..).partition(|i| i.session_id == sid && i.kind == Kind::Continue && i.status != Status::Starting);
                b.items = keep;
                for i in &gone {
                    b.mark(sid, i.resets_at, "claude-code", now_ms);
                }
                gone
            });
            for i in &gone {
                util::log_line(&["schedule", "claude-code", &i.project]);
                close(Some(app), shared, i, s(shared).claude_code);
            }
        }
        "quota_auto_resume_disabled" => limits::replan(app, shared, sid),
        _ => {}
    }
}

// ---------------------------------------------------------------- running

fn target_of(i: &Item) -> Target {
    if i.session_id.is_empty() {
        Target::New { cwd: i.cwd.clone(), project: i.project.clone() }
    } else {
        Target::Resume { id: i.session_id.clone(), cwd: i.cwd.clone(), project: i.project.clone(), title: i.title.clone() }
    }
}

/// Start the item's run through the remote runner.
fn run(app: &AppHandle, shared: &Arc<Shared>, i: &Item) -> Start {
    let (origin, gen, prompt) = match (i.kind, i.source) {
        // The phone's next message goes to this session too, unless another target was picked.
        (Kind::Message, _) => (Origin::Queued, Some(remote::target_gen(shared)).filter(|g| *g == i.target_gen), i.prompt.clone()),
        (Kind::Continue, Source::Pc) => (Origin::Continue { pc: true }, None, CONTINUE_PROMPT.to_string()),
        (Kind::Continue, _) => (Origin::Continue { pc: false }, None, CONTINUE_PROMPT.to_string()),
    };
    // A session of the PC runs in no looser a permission mode than it stopped in.
    let cap = (origin == Origin::Continue { pc: true }).then_some(i.permission_mode.as_str());
    remote::start(app, shared, target_of(i), prompt, origin, gen, cap)
}

/// Back in line, due at `due_at`.
fn requeue(shared: &Shared, id: &str, due_at: i64) {
    with_book(shared, |b| {
        if let Some(i) = b.get_mut(id) {
            i.status = Status::Scheduled;
            i.due_at = due_at;
        }
    });
}

/// The item will not run: it goes, the phone (and for a session of the PC with `pc_banner`, the
/// PC) is told why.
fn drop_item(app: &AppHandle, shared: &Arc<Shared>, i: &Item, why: &str, note: &str, pc_banner: bool) {
    let now_ms = util::now_ms();
    with_book(shared, |b| {
        b.take(&i.id);
        if i.kind == Kind::Continue {
            b.mark(&i.session_id, i.resets_at, "skipped", now_ms);
        }
    });
    util::log_line(&["schedule", "dropped", why, &i.project]);
    if i.phone_mid.is_some() {
        close(Some(app), shared, i, note);
    } else if why != "refused" && phone_on(shared, i) {
        // (A refused run said why on the phone already.)
        remote::send_msg(shared, &format!("<b>{}</b> · {}", esc(&i.project), esc(note)), &[]);
    }
    if pc_banner && i.source == Source::Pc {
        let x = s(shared);
        events::banner(app, shared, true, shared.t().code_name, &format!("{} · {}", x.not_continued, i.project), note, "limit", None);
    }
}

/// A run of a session continued for the PC ended without an answer (an error, or the limit
/// again): the PC is told.
pub fn pc_not_continued(app: &AppHandle, shared: &Arc<Shared>, project: &str, detail: &str) {
    let x = s(shared);
    events::banner(app, shared, true, shared.t().code_name, &format!("{} · {project}", x.not_continued), detail, "limit", None);
}

/// One due item: checked again, then run, moved or dropped.
fn process(app: &AppHandle, shared: &Arc<Shared>, i: Item, now_ms: i64) {
    let x = s(shared);
    let now = now_ms / 1000;
    if (i.kind == Kind::Message || i.source == Source::Remote) && !remote::enabled(shared) {
        return drop_item(app, shared, &i, "remote off", x.remote_off, false);
    }
    // Switched off since it was planned (on the PC, that is how a planned continue is cancelled
    // once its card has gone).
    let cfg = shared.cfg();
    let switched_off = match (i.kind, i.source) {
        (Kind::Continue, Source::Pc) => mode(&cfg) == "off",
        (Kind::Continue, _) => !config::get_bool(&cfg, "remote.continueAtReset", true),
        _ => false,
    };
    if switched_off {
        return drop_item(app, shared, &i, "switched off", x.cancelled, false);
    }
    // An elevated overlay would hand its administrator token to the run.
    if crate::win::is_elevated() {
        return drop_item(app, shared, &i, "elevated", remote::s(shared).elevated, true);
    }
    // A limit still (or again) in force for the model the run uses (or, for a continue, the one it
    // waited for): the item moves to its reset.
    let model = remote::run_model(&cfg);
    let own = if i.kind == Kind::Continue { i.window.as_str() } else { "" };
    if let Some((until, hit)) = blocked_until(&limits::active(shared), now_ms, model.as_deref(), own) {
        let label = limits::label(shared.t(), &hit.window, hit.model.as_deref());
        if i.deferrals >= MAX_DEFERRALS || (i.kind == Kind::Continue && until - now > MAX_AHEAD) {
            return drop_item(app, shared, &i, "still limited", &x.gave_up.replace("{limit}", &label), true);
        }
        let due = until + delay(i.kind, i.source, &i.entrypoint);
        let moved = with_book(shared, |b| {
            let m = b.get_mut(&i.id)?;
            m.status = Status::Scheduled;
            m.resets_at = until;
            m.due_at = due;
            m.deferrals += 1;
            Some(m.clone())
        });
        util::log_line(&["schedule", "moved", &hit.window, &i.project, &due.to_string()]);
        if let Some(m) = moved {
            let html = format!("<b>{}</b> · {}", esc(&m.project), fill(x.still, &[("limit", &esc(&label)), ("time", &when(x, due))]));
            post(shared, &m, html, "↓");
        }
        return;
    }
    if i.kind == Kind::Continue {
        // Gone on since it stopped (typed, the Claude app's or Claude Code's own continue, a
        // message from the phone): never a second continue.
        let path = if i.transcript_path.is_empty() {
            remote::transcript_path(&i.session_id).map(|p| p.to_string_lossy().into_owned())
        } else {
            Some(i.transcript_path.clone())
        };
        match path.and_then(|p| limits::transcript_tail(&p)) {
            None => return drop_item(app, shared, &i, "no transcript", x.no_transcript, false),
            Some(tail) if went_on(&tail, i.created_ms) => return drop_item(app, shared, &i, "went on", x.went_on, false),
            Some(_) => {}
        }
        if i.source == Source::Pc && delay(i.kind, i.source, &i.entrypoint) == DESKTOP_DELAY {
            if let Some(ds) = desktop_session(&i.session_id) {
                if ds.archived {
                    return drop_item(app, shared, &i, "archived", x.archived, false);
                }
                // The Claude app is about to continue it itself: look again in a minute.
                if i.window == "five_hour" && now < i.resets_at + DESKTOP_WAIT_MAX && desktop_armed(&ds, i.resets_at) {
                    util::log_line(&["schedule", "claude-app-armed", &i.project]);
                    return requeue(shared, &i.id, now + 60);
                }
            }
        }
    }
    // A session that changed since the message was written may be open on the PC: as with any
    // message from the phone, the user confirms first.
    if i.kind == Kind::Message && !i.session_id.is_empty() {
        if let Some(age_ms) = remote::changed_since(shared, &i.session_id, i.created_ms) {
            // One prompt is held at a time (in memory): this one waits in the book meanwhile.
            if remote::has_held(shared) {
                return requeue(shared, &i.id, now + 30);
            }
            with_book(shared, |b| b.take(&i.id));
            util::log_line(&["schedule", "held", &i.project]);
            close(Some(app), shared, &i, x.held);
            remote::hold_prompt(shared, target_of(&i), i.prompt.clone(), age_ms, false);
            return;
        }
    }
    match run(app, shared, &i) {
        // A run from the phone got there first: next round.
        Start::Busy => requeue(shared, &i.id, i.due_at),
        Start::Refused(why) => drop_item(app, shared, &i, "refused", &why, true),
        Start::Started => {
            with_book(shared, |b| {
                b.take(&i.id);
                b.mark(&i.session_id, i.resets_at, if i.kind == Kind::Continue { "ran" } else { "message" }, now_ms);
            });
            util::log_line(&["schedule", "started", if i.kind == Kind::Message { "message" } else { "continue" }, &i.project]);
            close(Some(app), shared, &i, if i.kind == Kind::Message { x.sent } else { x.continued });
            if i.source == Source::Pc {
                let subtitle = format!("{} · {}", x.continuing, i.project);
                events::banner(app, shared, true, shared.t().code_name, &subtitle, x.continuing_body, "info", Some(15_000));
            }
        }
    }
}

fn tick(app: &AppHandle, shared: &Arc<Shared>, now_ms: i64, woke_ms: i64) {
    let gone = with_book(shared, |b| {
        b.prune(now_ms);
        expire(b, now_ms / 1000)
    });
    let x = s(shared);
    for (i, why) in gone {
        util::log_line(&["schedule", if why == Gone::Expired { "expired" } else { "late" }, &i.project]);
        close(Some(app), shared, &i, if why == Gone::Expired { x.expired } else { x.late });
    }
    // One run at a time: what is due waits for the one going on.
    if remote::busy(shared) {
        return;
    }
    if let Some(i) = with_book(shared, |b| take_due(b, now_ms, woke_ms)) {
        process(app, shared, i, now_ms);
    }
}

/// The scheduler: every two seconds, what is due runs (one at a time).
pub fn start(app: AppHandle, shared: Arc<Shared>) {
    // One that was being started when HeadsUp stopped goes back in line.
    with_book(&shared, |b| {
        for i in b.items.iter_mut().filter(|i| i.status == Status::Starting) {
            i.status = Status::Scheduled;
        }
    });
    std::thread::spawn(move || {
        let mut last = util::now_ms();
        let mut woke_ms = 0i64;
        loop {
            std::thread::sleep(Duration::from_secs(2));
            if shared.quitting.load(Ordering::SeqCst) {
                return;
            }
            let now = util::now_ms();
            // The clock jumped: the PC slept.
            if now - last > 30_000 {
                woke_ms = now;
                util::log_line(&["schedule", "wake", &((now - last) / 1000).to_string()]);
            }
            tick(&app, &shared, now, woke_ms);
            // From the end of the round: a slow one (the network) is not a sleep.
            last = util::now_ms();
        }
    });
}

/// `--send test-limit-offer`: a sample card (nothing is recorded; its buttons only close it).
pub fn test_card(app: &AppHandle, shared: &Arc<Shared>) {
    let t = shared.t();
    let x = s(shared);
    let due = util::now_secs() + 2 * 3600 + 13 * 60;
    let p = Plan { id: "test".to_string(), offered: true, project: "demo".to_string(), due_at: due };
    let reset = limits::reset_phrase(t, due - DESKTOP_DELAY, &chrono::Local::now());
    let subtitle = limits::join_nonempty(&[t.limit_hit_five_hour, &reset, t.test]);
    let body = format!("{}\n{}", t.limit_waiting.replace("{list}", "demo"), plan_line(shared, &p));
    let actions = vec![(x.no.to_string(), false, false), (x.yes.replace("{time}", &when(x, due)), true, true)];
    events::show_offer(app, shared, &card_id("test"), &subtitle, &body, &actions, x.saved, OFFER_CARD_MS);
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::FixedOffset;

    const NOW_MS: i64 = 1_792_700_437_000; // 2026-10-22 20:20:37 UTC
    const RESET: i64 = 1_792_701_000; // 20:30 UTC

    fn active(window: &str, resets_at: Option<i64>, alerted_ms: i64) -> limits::Active {
        limits::Active {
            hit: Hit { window: window.into(), resets_at, reset_exact: resets_at.is_some(), ..Hit::default() },
            waiters: Vec::new(),
            alerted_ms,
        }
    }

    fn facts<'a>(mode: &'a str, entrypoint: &'a str, session: &'a str) -> PcFacts<'a> {
        PcFacts {
            mode,
            entrypoint,
            claude_code_continues: true,
            resets_at: Some(RESET),
            session_id: session,
            known_project: true,
            can_ask: true,
            now_ms: NOW_MS,
        }
    }

    fn continue_item(id: &str, session: &str, resets_at: i64) -> Item {
        Item { id: id.into(), kind: Kind::Continue, source: Source::Pc, session_id: session.into(), resets_at, due_at: resets_at + 240, ..Item::default() }
    }

    fn message(id: &str, session: &str, cwd: &str, prompt: &str) -> Item {
        Item {
            id: id.into(),
            kind: Kind::Message,
            session_id: session.into(),
            cwd: cwd.into(),
            prompt: prompt.into(),
            resets_at: RESET,
            due_at: RESET + 20,
            ..Item::default()
        }
    }

    #[test]
    fn limit_in_force_and_until_when() {
        assert_eq!(blocked_until(&[], NOW_MS, None, ""), None);
        // The latest reset of those in force: all of them must have let go.
        let a = [active("five_hour", Some(RESET), NOW_MS), active("seven_day", Some(RESET + 86_400), NOW_MS)];
        let (until, hit) = blocked_until(&a, NOW_MS, None, "").unwrap();
        assert_eq!((until, hit.window.as_str()), (RESET + 86_400, "seven_day"));
        // A reset that has passed is not in force; a reset not known stands an hour.
        assert_eq!(blocked_until(&[active("five_hour", Some(NOW_MS / 1000 - 1), 0)], NOW_MS, None, ""), None);
        let unknown = [active("usage", None, NOW_MS - 600_000)];
        assert_eq!(blocked_until(&unknown, NOW_MS, None, "").map(|(at, _)| at), Some((NOW_MS - 600_000 + 3_600_000) / 1000));
        assert_eq!(blocked_until(&unknown, NOW_MS + 3_600_000, None, ""), None);
    }

    #[test]
    fn a_per_model_limit_holds_only_runs_on_that_model() {
        let modelled = |window: &str, model: Option<&str>| Hit { window: window.into(), model: model.map(String::from), ..Hit::default() };
        for w in ["five_hour", "seven_day", "usage"] {
            assert!(applies(&modelled(w, None), None), "{w}");
            assert!(applies(&modelled(w, None), Some("sonnet")), "{w}");
        }
        let opus = modelled("seven_day_opus", None);
        assert!(applies(&opus, Some("opus")));
        assert!(applies(&opus, Some("claude-opus-5-5")));
        assert!(!applies(&opus, Some("sonnet")));
        assert!(!applies(&opus, None));
        assert!(applies(&modelled("seven_day_sonnet", None), Some("claude-sonnet-5")));
        assert!(!applies(&modelled("seven_day_sonnet", None), Some("opus")));
        let fable = modelled("seven_day_overage_included", Some("Fable 5"));
        assert!(applies(&fable, Some("claude-fable-5")));
        assert!(!applies(&fable, Some("opus")));
        assert!(applies(&modelled("seven_day_overage_included", None), Some("fable")));
        assert!(applies(&modelled("usage", Some("Haiku")), Some("claude-haiku-5")));
        assert!(!applies(&modelled("usage", Some("Haiku")), None));
        // The Opus week holds an Opus run only; a Sonnet run goes at the 5-hour reset.
        let a = [active("five_hour", Some(RESET), NOW_MS), active("seven_day_opus", Some(RESET + 3 * 86_400), NOW_MS)];
        assert_eq!(blocked_until(&a, NOW_MS, Some("opus"), "").map(|(at, _)| at), Some(RESET + 3 * 86_400));
        assert_eq!(blocked_until(&a, NOW_MS, Some("sonnet"), "").map(|(at, _)| at), Some(RESET));
        assert_eq!(blocked_until(&a, NOW_MS, None, "").map(|(at, _)| at), Some(RESET));
        // A continue's own limit holds it whatever the model.
        assert_eq!(blocked_until(&a, NOW_MS, None, "seven_day_opus").map(|(at, _)| at), Some(RESET + 3 * 86_400));
    }

    #[test]
    fn due_times_leave_room_for_claude_s_own_continue() {
        assert_eq!(delay(Kind::Message, Source::Phone, ""), 20);
        assert_eq!(delay(Kind::Continue, Source::Remote, "remote"), 20);
        // Claude Code's own continue fires 30-90 s after the reset.
        assert_eq!(delay(Kind::Continue, Source::Pc, "cli"), 150);
        assert_eq!(delay(Kind::Continue, Source::Pc, "sdk-cli"), 45);
        assert_eq!(delay(Kind::Continue, Source::Pc, "claude-desktop"), 240);
        assert_eq!(delay(Kind::Continue, Source::Pc, ""), 240);
        // A message for a session that stopped on the PC goes after its own continue.
        assert_eq!(delay(Kind::Message, Source::Phone, "cli"), 150);
        assert_eq!(delay(Kind::Message, Source::Phone, "claude-desktop"), 240);
        assert_eq!(delay(Kind::Message, Source::Phone, "pc"), 240);
        assert_eq!(delay(Kind::Message, Source::Phone, "remote"), 20);
        assert_eq!(mode(&json!({"limits": {"continueAtReset": "Always"}})), "always");
        assert_eq!(mode(&json!({"limits": {"continueAtReset": "off"}})), "off");
        assert_eq!(mode(&json!({"limits": {"continueAtReset": "nope"}})), "ask");
        assert_eq!(mode(&json!({})), "ask");
    }

    #[test]
    fn pc_sessions_asked_scheduled_or_left() {
        let book = Book::default();
        assert_eq!(plan_pc(&facts("ask", "claude-desktop", "s1"), &book), PcPlan::Offer);
        assert_eq!(plan_pc(&facts("always", "claude-desktop", "s1"), &book), PcPlan::Schedule);
        assert_eq!(plan_pc(&facts("off", "claude-desktop", "s1"), &book), PcPlan::Skip("off"));
        // The CLI continues by itself unless its setting is off.
        assert_eq!(plan_pc(&facts("always", "cli", "s1"), &book), PcPlan::Skip("claude code continues it"));
        assert_eq!(plan_pc(&PcFacts { claude_code_continues: false, ..facts("always", "cli", "s1") }, &book), PcPlan::Schedule);
        assert_eq!(plan_pc(&facts("always", "sdk-cli", "s1"), &book), PcPlan::Schedule);
        assert_eq!(plan_pc(&facts("ask", "claude-desktop", ""), &book), PcPlan::Skip("no session id"));
        assert_eq!(plan_pc(&PcFacts { resets_at: None, ..facts("ask", "", "s1") }, &book), PcPlan::Skip("reset not known"));
        assert_eq!(plan_pc(&PcFacts { resets_at: Some(NOW_MS / 1000 - 5), ..facts("ask", "", "s1") }, &book), PcPlan::Skip("reset passed"));
        let far = PcFacts { resets_at: Some(NOW_MS / 1000 + 3 * 86_400), ..facts("ask", "", "s1") };
        assert_eq!(plan_pc(&far, &book), PcPlan::Skip("reset too far ahead"));
        assert_eq!(plan_pc(&PcFacts { known_project: false, ..facts("ask", "", "s1") }, &book), PcPlan::Skip("unknown project"));
        assert_eq!(plan_pc(&PcFacts { can_ask: false, ..facts("ask", "", "s1") }, &book), PcPlan::Skip("nobody to ask"));
        assert_eq!(plan_pc(&PcFacts { can_ask: false, ..facts("always", "", "s1") }, &book), PcPlan::Schedule);
    }

    #[test]
    fn one_continue_per_session_and_reset_and_a_cap() {
        let mut book = Book::default();
        book.items.push(continue_item("a", "s1", RESET));
        // Planned already, or decided for this reset (declined, run, skipped).
        assert_eq!(plan_pc(&facts("always", "", "s1"), &book), PcPlan::Skip("planned already"));
        book.mark("s2", RESET + 60, "declined", NOW_MS);
        assert_eq!(plan_pc(&facts("always", "", "s2"), &book), PcPlan::Skip("decided already"));
        // At most MAX_CONTINUES per reset, run ones included.
        for n in 0..3 {
            book.items.push(continue_item(&format!("c{n}"), &format!("x{n}"), RESET));
        }
        book.mark("ran1", RESET, "ran", NOW_MS);
        assert_eq!(book.continues_for(RESET), 5);
        assert_eq!(plan_pc(&facts("always", "", "s9"), &book), PcPlan::Skip("cap"));
        // Another reset has its own count.
        let next = PcFacts { resets_at: Some(RESET + 5 * 3600), ..facts("always", "", "s9") };
        assert_eq!(plan_pc(&next, &book), PcPlan::Schedule);
    }

    #[test]
    fn no_chain_of_continues_without_an_answer() {
        let mut book = Book::default();
        // Continued on its own at the last reset: the next one is asked about.
        book.mark("s1", RESET - 5 * 3600, "ran", NOW_MS - 3_600_000);
        assert_eq!(plan_pc(&facts("always", "", "s1"), &book), PcPlan::Offer);
        assert_eq!(plan_pc(&PcFacts { can_ask: false, ..facts("always", "", "s1") }, &book), PcPlan::Skip("continued at the last reset"));
        // A day later it is a new start.
        book.mark("s1", RESET - 5 * 3600, "ran", NOW_MS - 25 * 3_600_000);
        assert_eq!(plan_pc(&facts("always", "", "s1"), &book), PcPlan::Schedule);
    }

    #[test]
    fn one_waiting_message_per_target() {
        let mut book = Book::default();
        assert_eq!(queue_into(&mut book, message("m1", "s1", "C:/p/a", "first")), Queued::New("m1".into()));
        book.items[0].phone_mid = Some(7);
        book.items[0].phone_html = "notice".into();
        // The same session: appended, and its notice loses its buttons.
        let r = queue_into(&mut book, Item { due_at: RESET + 999, ..message("m2", "s1", "C:/p/a", "second") });
        assert_eq!(r, Queued::Appended { id: "m1".into(), notice: Some((7, "notice".into())) });
        assert_eq!(book.items.len(), 1);
        assert_eq!(book.items[0].prompt, "first\n\nsecond");
        assert_eq!(book.items[0].due_at, RESET + 999);
        // A new session in the same folder (any spelling) is one target; another session is another.
        assert_eq!(queue_into(&mut book, message("m3", "", "C:/p/a", "new one")), Queued::New("m3".into()));
        assert!(matches!(queue_into(&mut book, message("m4", "", "c:\\P\\A\\", "more")), Queued::Appended { .. }));
        assert_eq!(queue_into(&mut book, message("m5", "s2", "C:/p/a", "other")), Queued::New("m5".into()));
        assert_eq!(book.items.len(), 3);
        // Too long, and a message being sent right now is not appended to.
        let long = "x".repeat(MAX_PROMPT_CHARS);
        assert_eq!(queue_into(&mut book, message("m6", "s2", "", &long)), Queued::TooLong);
        book.items[2].status = Status::Starting;
        assert_eq!(queue_into(&mut book, message("m7", "s2", "", "later")), Queued::New("m7".into()));
        while book.items.len() < MAX_ITEMS {
            let n = book.items.len();
            book.items.push(continue_item(&format!("f{n}"), &format!("f{n}"), RESET));
        }
        assert_eq!(queue_into(&mut book, message("m8", "s3", "", "full")), Queued::Full);
    }

    #[test]
    fn a_waiting_message_replaces_the_session_s_continue() {
        let mut book = Book::default();
        book.items.push(continue_item("c1", "s1", RESET));
        book.items.push(continue_item("c2", "s2", RESET));
        let gone = supersede(&mut book, "s1", NOW_MS);
        assert_eq!(gone.iter().map(|i| i.id.as_str()).collect::<Vec<_>>(), vec!["c1"]);
        assert!(book.done_for("s1", RESET));
        assert_eq!(book.items.len(), 1);
    }

    #[test]
    fn skip_a_session_that_went_on() {
        let limit = json!({"type": "assistant", "isApiErrorMessage": true, "apiError": "usage_limit_reached",
                           "message": {"content": [{"type": "text", "text": "You've hit your session limit · resets 11:30pm (X)"}]}});
        let old = json!({"type": "assistant", "isApiErrorMessage": true,
                         "message": {"content": [{"type": "text", "text": "You've hit your limit · resets 4pm (X)"}]}});
        let user = json!({"type": "user", "message": {"content": "go on"}});
        let auto = json!({"type": "user", "isMeta": true, "origin": {"kind": "auto-continuation"}, "message": {"content": "Your claude.ai usage limit has reset."}});
        let system = json!({"type": "system", "subtype": "informational", "content": "Usage limit reached · continuing automatically"});
        let queued = json!({"type": "queue-operation", "operation": "enqueue"});
        let command = json!({"type": "user", "message": {"content": "<command-name>/model</command-name>\n<command-message>model</command-message>"}});
        let stdout = json!({"type": "user", "message": {"content": "<local-command-stdout>Set model to Sonnet</local-command-stdout>"}});
        let caveat = json!({"type": "user", "isMeta": true, "message": {"content": [{"type": "text", "text": "<local-command-caveat>Caveat</local-command-caveat>"}]}});
        let answer = json!({"type": "user", "message": {"content": [{"type": "tool_result", "tool_use_id": "x", "content": "ok"}]}});
        let lines = |rs: &[&Value]| format!("{{\"cut\n{}\n", rs.iter().map(|r| r.to_string()).collect::<Vec<_>>().join("\n"));
        let went = |rs: &[&Value]| went_on(&lines(rs), NOW_MS);
        // Stopped, and only bookkeeping (or a local slash command such as /model) since: continue it.
        assert!(!went(&[&user, &limit, &system]));
        assert!(!went(&[&limit, &caveat, &command, &stdout]));
        assert!(went(&[&limit, &command, &stdout, &user]));
        assert!(went(&[&limit, &answer]));
        assert!(!went(&[&user, &old]));
        // A prompt, an automatic continue or a queued message after the limit: it went on.
        assert!(went(&[&limit, &user]));
        assert!(went(&[&limit, &auto]));
        assert!(went(&[&limit, &system, &queued]));
        assert!(went(&[&limit, &json!({"type": "assistant", "message": {"content": "ok"}})]));
        // Stopped again after going on: the last limit counts.
        assert!(!went(&[&limit, &user, &limit]));
        // No limit record in the tail (not written, or much happened since): by time.
        let before = json!({"type": "user", "timestamp": "2026-10-22T20:20:30.000Z", "message": {"content": "the prompt"}});
        let after = json!({"type": "user", "timestamp": "2026-10-22T20:25:00.000Z", "message": {"content": "typed later"}});
        assert!(!went(&[&before, &system]));
        assert!(went(&[&before, &after]));
        assert!(!went(&[&user]));
        assert!(!went_on("", NOW_MS));
    }

    #[test]
    fn the_claude_app_s_own_auto_resume() {
        let store = json!({"choices": {"acct": true}, "windows": {"acct/org": {
            "local_a": {"resetsAt": RESET, "attempt": 0, "optedIn": true, "armed": true},
            "local_b": {"resetsAt": RESET, "attempt": 0, "optedIn": false, "armed": false},
            "local_c": {"resetsAt": RESET, "attempt": 3, "optedIn": true, "armed": true}
        }}});
        assert!(desktop_armed_in(&store, "acct/org", "local_a", RESET + 30));
        assert!(!desktop_armed_in(&store, "acct/org", "local_a", RESET + 7 * 3600));
        assert!(!desktop_armed_in(&store, "acct/org", "local_b", RESET));
        assert!(!desktop_armed_in(&store, "acct/org", "local_c", RESET));
        assert!(!desktop_armed_in(&store, "acct/other", "local_a", RESET));
        assert!(!desktop_armed_in(&json!({}), "acct/org", "local_a", RESET));
        // Claude Code's setting: the first file that sets it wins; on when none does.
        let key = "autoContinueAtUsageLimit";
        assert!(setting_on(&[], key));
        assert!(!setting_on(&[json!({"x": 1}), json!({key: false}), json!({key: true})], key));
        assert!(setting_on(&[json!({key: true}), json!({key: false})], key));
    }

    #[test]
    fn expiry_wake_and_due_order() {
        let now = NOW_MS / 1000;
        let mut book = Book::default();
        let mut offered = continue_item("o", "s1", now - OFFER_GRACE - 1);
        offered.status = Status::Offered;
        let mut late = message("l", "s2", "", "hi");
        late.due_at = now - STALE - 1;
        let mut fresh = continue_item("f", "s3", now - 300);
        fresh.due_at = now - 10;
        let mut first = message("m", "s4", "", "hi");
        first.due_at = now - 20;
        book.items = vec![offered, late, fresh, first];
        let gone = expire(&mut book, now);
        assert_eq!(gone.iter().map(|(i, w)| (i.id.as_str(), w)).collect::<Vec<_>>(), vec![("o", &Gone::Expired), ("l", &Gone::Late)]);
        assert!(book.done_for("s1", now - OFFER_GRACE - 1));
        // Due first goes first, and once only (it is Starting).
        assert_eq!(take_due(&mut book, NOW_MS, 0).map(|i| i.id), Some("m".into()));
        assert_eq!(take_due(&mut book, NOW_MS, 0).map(|i| i.id), Some("f".into()));
        assert_eq!(take_due(&mut book, NOW_MS, 0), None);
        // Just after a wake, a message waits 90 s and a Claude app session 4 minutes.
        for i in book.items.iter_mut() {
            i.status = Status::Scheduled;
        }
        assert_eq!(take_due(&mut book, NOW_MS, NOW_MS - 60_000), None);
        assert_eq!(take_due(&mut book, NOW_MS, NOW_MS - 100_000).map(|i| i.id), Some("m".into()));
        assert_eq!(take_due(&mut book, NOW_MS, NOW_MS - 100_000), None);
        assert_eq!(take_due(&mut book, NOW_MS, NOW_MS - 241_000).map(|i| i.id), Some("f".into()));
    }

    #[test]
    fn buttons_carry_the_item_and_what_they_do() {
        let mut i = continue_item("ab12", "s1", RESET);
        i.status = Status::Offered;
        let data = |b: Vec<Vec<(String, String)>>| b.concat().into_iter().map(|(_, d)| d).collect::<Vec<_>>();
        assert_eq!(data(buttons_for(&EN, &i)), vec!["lc:ab12", "ln:ab12"]);
        i.status = Status::Scheduled;
        assert_eq!(data(buttons_for(&EN, &i)), vec!["ln:ab12"]);
        assert_eq!(data(buttons_for(&TR, &message("m9", "", "C:/p", "x"))), vec!["ls:m9", "ln:m9"]);
        // Within Telegram's 64 bytes of callback data.
        assert!(format!("lc:{}", new_id()).len() <= 64);
    }

    #[test]
    fn book_round_trips_through_state_json() {
        let mut book = Book::default();
        let mut i = message("m1", "s1", "C:/p/a", "hello");
        i.phone_mid = Some(42);
        book.items.push(i);
        book.mark("s2", RESET, "ran", NOW_MS);
        let mut st = json!({"windows": {}});
        book.store(&mut st);
        assert_eq!(st["limitSchedule"]["items"][0]["sessionId"], "s1");
        assert_eq!(st["limitSchedule"]["items"][0]["kind"], "message");
        assert_eq!(st["limitSchedule"]["done"]["s2"]["outcome"], "ran");
        assert_eq!(Book::load(&st), book);
        // A broken item is dropped, the rest kept.
        st["limitSchedule"]["items"].as_array_mut().unwrap().push(json!({"id": 5}));
        st["limitSchedule"]["items"].as_array_mut().unwrap().push(json!({"kind": "continue"}));
        assert_eq!(Book::load(&st).items.len(), 1);
        assert_eq!(Book::load(&json!({})), Book::default());
        // Outcomes are forgotten after a week.
        book.prune(NOW_MS + DONE_KEEP_MS + 1);
        assert!(book.done.is_empty());
    }

    #[test]
    fn times_in_local_words() {
        let ist = FixedOffset::east_opt(3 * 3600).unwrap();
        let now = ist.with_ymd_and_hms(2026, 10, 22, 23, 5, 0).unwrap();
        assert_eq!(when_in(&EN, RESET, &now), "at 23:30");
        assert_eq!(when_in(&EN, RESET + 3600, &now), "tomorrow at 00:30");
        assert_eq!(when_in(&EN, RESET + 3 * 86_400, &now), "on 25.10 at 23:30");
        assert_eq!(when_in(&TR, RESET, &now), "23:30");
        assert_eq!(when_in(&TR, RESET + 3600, &now), "yarın 00:30");
        assert_eq!(when_in(&TR, RESET + 3 * 86_400, &now), "25.10 23:30");
        let t = when_in(&EN, RESET + 3600, &now);
        assert_eq!(fill(EN.yes, &[("time", &t)]), "Continue tomorrow at 00:30");
        assert_eq!(fill(EN.queued, &[("limit", "x"), ("p", "a"), ("time", &t)]), "⛔ x. Your message for <b>a</b> will be sent tomorrow at 00:30.");
    }
}
