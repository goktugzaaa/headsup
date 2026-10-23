//! Usage limits used up. Claude Code ends a turn with "You've hit your session limit · resets
//! 11:30pm (Europe/Istanbul)" when a usage limit runs out. HeadsUp learns it from the StopFailure
//! hook (the exact reset comes from the record Claude Code writes to the session's transcript),
//! from the status line (a window at 100 %) and from its own remote runs (stream-json), and reports
//! it once per window and reset: a banner on the PC and, at once, a message on the phone. Each one
//! is kept in state.json (`limitHits`) with the sessions that stopped on it.
//!
//! A transient 429 ("Server is temporarily limiting requests (not your usage limit)"), a spend cap
//! or missing usage credits is not a usage limit: nothing resets, so nothing is reported.
use crate::phone::{self, PhoneMsg};
use crate::{config, events, i18n, mask, util, Shared};
use chrono::{DateTime, Datelike, NaiveDate, TimeZone, Utc};
use serde_json::{json, Map, Value};
use std::io::{Read, Seek, SeekFrom};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;
use tauri::AppHandle;

/// Limit types (quotaLimits.rateLimitType) a hit is filed under; any other one (overage, a type
/// added later) and a hit whose type is not known are filed under "usage".
const WINDOWS: [&str; 5] = ["five_hour", "seven_day", "seven_day_opus", "seven_day_sonnet", "seven_day_overage_included"];
/// Status-line windows that stop work at 100 % (spend_limit only ends extra usage).
const STATUS_WINDOWS: [&str; 4] = ["five_hour", "seven_day", "seven_day_opus", "seven_day_sonnet"];
/// Two sightings whose resets are this close are the same window: a reset read from the message
/// is only to the minute, and two windows of one kind are hours apart.
pub const SAME_RESET_SECS: i64 = 15 * 60;
/// A hit whose reset is not known stands for this long.
pub const UNKNOWN_RESET_MS: i64 = 3_600_000;
/// Sessions kept per hit.
const MAX_WAITERS: usize = 20;
/// A hit is forgotten this long after its reset.
const KEEP_MS: i64 = 7 * 86_400_000;
/// How much of the end of a transcript is searched for the error record.
const TRANSCRIPT_TAIL: u64 = 256 * 1024;
/// The " · " Claude Code puts between the parts of its messages.
pub const SEP: &str = " \u{b7} ";

/// One sighting of a used-up limit.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Hit {
    /// One of WINDOWS, or "usage".
    pub window: String,
    /// The model of a per-model limit ("Opus", "Fable"), as the message names it.
    pub model: Option<String>,
    /// When it resets (unix seconds).
    pub resets_at: Option<i64>,
    /// `resets_at` came from Claude Code's own data, not from reading the message text.
    pub reset_exact: bool,
    /// The reset as the message wrote it ("11:30pm (Europe/Istanbul)").
    pub reset_text: Option<String>,
}

/// A session that stopped on a used-up limit.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Waiter {
    pub session_id: String,
    pub cwd: String,
    pub transcript_path: String,
    /// Who ran it: "cli" (an interactive terminal), "claude-desktop" (the Claude app's Code tab),
    /// "sdk-cli" (claude -p), "remote" (a HeadsUp remote run), "" when not known.
    pub entrypoint: String,
    /// The permission mode it stopped in ("default", "plan", "acceptEdits"...; "" when not known).
    pub permission_mode: String,
    /// When it stopped (unix ms).
    pub at_ms: i64,
}

impl Waiter {
    fn is_known(&self) -> bool {
        !self.session_id.is_empty() || !self.cwd.is_empty()
    }

    fn same_session(&self, other: &Waiter) -> bool {
        if self.session_id.is_empty() && other.session_id.is_empty() {
            self.cwd == other.cwd
        } else {
            self.session_id == other.session_id
        }
    }

    pub fn project(&self) -> String {
        util::project_name(Some(&Value::String(self.cwd.clone())))
    }

    fn to_json(&self) -> Value {
        json!({
            "sessionId": self.session_id,
            "cwd": self.cwd,
            "transcriptPath": self.transcript_path,
            "entrypoint": self.entrypoint,
            "permissionMode": self.permission_mode,
            "atMs": self.at_ms,
        })
    }

    fn from_json(v: &Value) -> Option<Waiter> {
        let s = |k: &str| v.get(k).and_then(Value::as_str).unwrap_or("").to_string();
        let w = Waiter {
            session_id: s("sessionId"),
            cwd: s("cwd"),
            transcript_path: s("transcriptPath"),
            entrypoint: s("entrypoint"),
            permission_mode: s("permissionMode"),
            at_ms: v.get("atMs").and_then(num_i64).unwrap_or(0),
        };
        w.is_known().then_some(w)
    }
}

/// A used-up limit still in force.
#[derive(Clone, Debug, PartialEq)]
pub struct Active {
    pub hit: Hit,
    /// The sessions that stopped on it, oldest first.
    pub waiters: Vec<Waiter>,
    /// When it was first reported (unix ms; 0 for a status-line window): a hit whose reset is not
    /// known stands until UNKNOWN_RESET_MS after this.
    pub alerted_ms: i64,
}

fn num_i64(v: &Value) -> Option<i64> {
    v.as_i64().or_else(|| v.as_f64().map(|f| f as i64))
}

// ---------------------------------------------------------------- reading Claude Code's reports

/// A usage-limit message: "You've hit your session limit · resets 11:30pm (Europe/Istanbul)",
/// with "weekly", "Opus", "Sonnet", "Fable" or "usage" for "session", or no word at all (Claude
/// Code before 2.1.2xx), and " · progress saved" possibly after it. None for anything else.
pub fn parse_message(text: &str) -> Option<Hit> {
    let t = text.trim();
    let rest = t.strip_prefix("You've hit your ").or_else(|| t.strip_prefix("You\u{2019}ve hit your "))?;
    // The reset must follow the limit's name directly: "your monthly spend limit · raise it at …
    // · your session limit resets 4pm" is a spend cap.
    let (name, after) = rest.split_once(&format!("{SEP}resets "))?;
    let name = name.strip_suffix("limit")?.trim_end();
    let (window, model) = match name {
        "" | "usage" => ("usage", None),
        "session" => ("five_hour", None),
        "weekly" => ("seven_day", None),
        "Opus" => ("seven_day_opus", None),
        "Sonnet" => ("seven_day_sonnet", None),
        m if model_name(m) && m.starts_with("Fable") => ("seven_day_overage_included", Some(m)),
        m if model_name(m) => ("usage", Some(m)),
        _ => return None,
    };
    let reset = after.split(SEP).next().unwrap_or("").trim();
    Some(Hit {
        window: window.to_string(),
        model: model.map(String::from),
        reset_text: (!reset.is_empty()).then(|| util::truncate(reset, 60)),
        ..Hit::default()
    })
}

/// "Fable", "Fable 5": a capitalised name of at most two words, letters, digits and dots only.
/// ("individual usage", "org's monthly usage" and the like are not.)
fn model_name(s: &str) -> bool {
    s.len() <= 24
        && s.starts_with(|c: char| c.is_ascii_uppercase())
        && s.split(' ').count() <= 2
        && s.chars().all(|c| c.is_ascii_alphanumeric() || c == ' ' || c == '.')
}

/// A quota status (quotaLimits, rate_limit_info) that stops work: rejected, with no extra usage
/// taking over. (limit type, reset).
fn quota_rejected(info: &Value) -> Option<(Option<String>, Option<i64>)> {
    let s = |k: &str| info.get(k).and_then(Value::as_str);
    if s("status") != Some("rejected")
        || info.get("isUsingOverage").and_then(Value::as_bool) == Some(true)
        || matches!(s("overageStatus"), Some("allowed" | "allowed_warning"))
    {
        return None;
    }
    Some((s("rateLimitType").map(String::from), info.get("resetsAt").and_then(num_i64)))
}

fn window_name(kind: Option<&str>) -> String {
    kind.filter(|k| WINDOWS.contains(k)).unwrap_or("usage").to_string()
}

/// A usage limit from what came with an API error: the message (`text_hit`), the quota status
/// and whether the error code was usage_limit_reached. The quota's limit type and reset win over
/// what the message says.
fn combine(text_hit: Option<Hit>, quota: Option<&Value>, usage_error: bool) -> Option<Hit> {
    let mut hit = match text_hit {
        Some(h) => h,
        None if usage_error => Hit { window: "usage".to_string(), ..Hit::default() },
        None => return None,
    };
    if let Some((kind, at)) = quota.and_then(quota_rejected) {
        if kind.as_deref().is_some_and(|k| WINDOWS.contains(&k)) {
            hit.window = window_name(kind.as_deref());
        }
        if let Some(at) = at {
            hit.resets_at = Some(at);
            hit.reset_exact = true;
        }
    }
    Some(hit)
}

/// The text of a message's content (a string, or its text blocks).
pub fn content_text(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(blocks)) => blocks
            .iter()
            .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
            .filter_map(|b| b.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// A used-up limit in a line of `claude -p --output-format stream-json`: a rate_limit_event that
/// is rejected, the synthetic assistant message with error rate_limit, or a result whose api_error
/// is usage_limit_reached.
pub fn from_stream(v: &Value) -> Option<Hit> {
    match v.get("type").and_then(Value::as_str)? {
        "rate_limit_event" => {
            let (kind, at) = quota_rejected(v.get("rate_limit_info")?)?;
            Some(Hit { window: window_name(kind.as_deref()), resets_at: at, reset_exact: at.is_some(), ..Hit::default() })
        }
        "assistant" if v.get("error").and_then(Value::as_str) == Some("rate_limit") => combine(
            parse_message(&content_text(v.pointer("/message/content"))),
            v.pointer("/api_error_params/rate_limit_info"),
            v.get("api_error").and_then(Value::as_str) == Some("usage_limit_reached"),
        ),
        "result" if v.get("api_error").and_then(Value::as_str) == Some("usage_limit_reached") => combine(
            parse_message(v.get("result").and_then(Value::as_str).unwrap_or("")),
            v.pointer("/api_error_params/rate_limit_info"),
            true,
        ),
        _ => None,
    }
}

/// The last record in `text` (JSON lines) that reports an API error (isApiErrorMessage).
fn last_api_error_in(text: &str) -> Option<Value> {
    text.lines()
        .rev()
        .filter(|l| l.contains("\"isApiErrorMessage\""))
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .find(|v| v.get("isApiErrorMessage").and_then(Value::as_bool) == Some(true))
}

/// The end of the transcript at `path` (an absolute path to a .jsonl file): its last
/// TRANSCRIPT_TAIL bytes, the first line possibly cut.
pub fn transcript_tail(path: &str) -> Option<String> {
    let p = std::path::Path::new(path);
    if !p.is_absolute() || p.extension().and_then(|e| e.to_str()) != Some("jsonl") {
        return None;
    }
    let mut f = std::fs::File::open(p).ok()?;
    let len = f.metadata().ok()?.len();
    f.seek(SeekFrom::Start(len.saturating_sub(TRANSCRIPT_TAIL))).ok()?;
    let mut buf = Vec::new();
    f.take(TRANSCRIPT_TAIL).read_to_end(&mut buf).ok()?;
    Some(String::from_utf8_lossy(&buf).into_owned())
}

/// The same record, from the end of the transcript at `path`.
fn last_api_error(path: &str) -> Option<Value> {
    last_api_error_in(&transcript_tail(path)?)
}

/// The record is the one the hook reports: the same message, or written in the last two minutes.
fn matches_hook(rec: &Value, text: &str, now_ms: i64) -> bool {
    if !text.trim().is_empty() && content_text(rec.pointer("/message/content")).trim() == text.trim() {
        return true;
    }
    rec.get("timestamp")
        .and_then(Value::as_str)
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .is_some_and(|t| (now_ms - t.timestamp_millis()).abs() < 120_000)
}

/// The quota status saved with a transcript's error record.
fn record_quota(rec: &Value) -> Option<&Value> {
    rec.get("quotaLimits").filter(|q| q.is_object()).or_else(|| rec.pointer("/apiErrorParams/rate_limit_info"))
}

/// Letters, digits and - only: an entry point name ("cli", "claude-desktop", "sdk-cli") or a
/// permission mode.
fn clean_entrypoint(s: &str) -> String {
    s.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-').take(32).collect()
}

// ---------------------------------------------------------------- reset times

/// "11:30pm", "4am" or "23:30" as (hour, minute).
fn parse_clock(s: &str) -> Option<(u32, u32)> {
    let s = s.trim().to_ascii_lowercase();
    let (num, pm) = match (s.strip_suffix("am"), s.strip_suffix("pm")) {
        (Some(n), _) => (n.trim(), Some(false)),
        (_, Some(n)) => (n.trim(), Some(true)),
        _ => (s.as_str(), None),
    };
    let (h, m) = match num.split_once(':') {
        Some((h, m)) => (h.parse::<u32>().ok()?, m.parse::<u32>().ok()?),
        None => (num.parse::<u32>().ok()?, 0),
    };
    if m > 59 {
        return None;
    }
    match pm {
        Some(pm) if (1..=12).contains(&h) => Some((h % 12 + if pm { 12 } else { 0 }, m)),
        None if h <= 23 && num.contains(':') => Some((h, m)),
        _ => None,
    }
}

fn month_number(s: &str) -> Option<u32> {
    let m = s.trim().to_ascii_lowercase();
    let months = ["jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec"];
    months.iter().position(|p| m.starts_with(p)).map(|i| i as u32 + 1)
}

/// The reset a message names ("11:30pm (Europe/Istanbul)", "4am (…)", "Oct 25, 9am (…)",
/// "Jan 3, 2027, 9am (…)") as unix seconds. Claude Code writes it in the PC's own time zone, so
/// it is read in `now`'s: the first such moment from ten minutes ago on (a date without a year:
/// from yesterday on). Only a fallback for when no exact reset is known.
pub fn parse_reset_text<Tz: TimeZone>(text: &str, now: &DateTime<Tz>) -> Option<i64> {
    let s = text.split(" (").next()?.trim();
    let parts: Vec<&str> = s.split(',').map(str::trim).collect();
    let (time, date) = parts.split_last()?;
    let (h, m) = parse_clock(time)?;
    let tz = now.timezone();
    let at = |d: NaiveDate| tz.from_local_datetime(&d.and_hms_opt(h, m, 0)?).earliest().map(|x| x.timestamp());
    let now_ts = now.timestamp();
    let today = now.naive_local().date();
    match date {
        [] => {
            let ts = at(today)?;
            if ts >= now_ts - 600 {
                Some(ts)
            } else {
                at(today.succ_opt()?)
            }
        }
        [md] | [md, _] => {
            let (mon, day) = md.split_once(' ')?;
            let (month, day) = (month_number(mon)?, day.trim().parse::<u32>().ok()?);
            if let [_, year] = date {
                return at(NaiveDate::from_ymd_opt(year.parse().ok()?, month, day)?);
            }
            let ts = at(NaiveDate::from_ymd_opt(today.year(), month, day)?)?;
            if ts >= now_ts - 86_400 {
                Some(ts)
            } else {
                at(NaiveDate::from_ymd_opt(today.year() + 1, month, day)?)
            }
        }
        _ => None,
    }
}

/// "resets at 18:40", "resets tomorrow at 09:00", "resets on 25.10 at 09:00" (in `now`'s zone).
pub fn reset_phrase<Tz: TimeZone>(t: &i18n::Strings, at: i64, now: &DateTime<Tz>) -> String
where
    Tz::Offset: std::fmt::Display,
{
    let Some(dt) = DateTime::<Utc>::from_timestamp(at, 0) else { return String::new() };
    let dt = dt.with_timezone(&now.timezone());
    let time = dt.format("%H:%M").to_string();
    let (day, today) = (dt.date_naive(), now.date_naive());
    if day == today {
        t.limit_resets_today.replace("{time}", &time)
    } else if today.succ_opt() == Some(day) {
        t.limit_resets_tomorrow.replace("{time}", &time)
    } else {
        t.limit_resets_on.replace("{date}", &dt.format("%d.%m").to_string()).replace("{time}", &time)
    }
}

// ---------------------------------------------------------------- the record (state.json)

/// What noting a hit did: it is news (no alert yet for this window and reset), the record changed.
#[derive(Debug, PartialEq)]
struct Noted {
    news: bool,
    changed: bool,
}

/// File `hit` (and the session that stopped on it) under its window in `hits` (state.json
/// `limitHits`). The same window and reset again only adds the session, and fills in the reset
/// when it is learnt later or exactly.
fn note_hit(hits: &mut Map<String, Value>, hit: &Hit, waiter: Option<&Waiter>, now_ms: i64) -> Noted {
    let now = now_ms / 1000;
    let same = hits.get(&hit.window).filter(|e| e.is_object()).is_some_and(|e| {
        let prev = e.get("resetsAt").and_then(num_i64);
        let alerted = e.get("alertedAtMs").and_then(num_i64).unwrap_or(0);
        match (prev, hit.resets_at) {
            (Some(a), Some(b)) => (a - b).abs() <= SAME_RESET_SECS,
            // Still before the reset already known: the same limit.
            (Some(a), None) => a > now,
            (None, _) => now_ms - alerted < UNKNOWN_RESET_MS,
        }
    });
    let waiter = waiter.filter(|w| w.is_known());
    if !same {
        hits.insert(
            hit.window.clone(),
            json!({
                "resetsAt": hit.resets_at,
                "resetExact": hit.reset_exact,
                "resetText": hit.reset_text,
                "model": hit.model,
                "alertedAtMs": now_ms,
                "sessions": waiter.map(|w| vec![w.to_json()]).unwrap_or_default(),
            }),
        );
        return Noted { news: true, changed: true };
    }
    let Some(e) = hits.get_mut(&hit.window) else { return Noted { news: false, changed: false } };
    let mut changed = false;
    if let Some(b) = hit.resets_at {
        let prev = e.get("resetsAt").and_then(num_i64);
        let exact = e.get("resetExact").and_then(Value::as_bool) == Some(true);
        if prev.is_none() || (hit.reset_exact && !exact) {
            if prev != Some(b) {
                e["resetsAt"] = json!(b);
                changed = true;
            }
            if hit.reset_exact && !exact {
                e["resetExact"] = json!(true);
                changed = true;
            }
        }
    }
    if let Some(w) = waiter {
        add_waiter(e, w);
        changed = true;
    }
    Noted { news: false, changed }
}

/// Add (or move to the end, updated) the session `w` in a hit's list; the oldest go past MAX_WAITERS.
fn add_waiter(entry: &mut Value, w: &Waiter) {
    if !entry.get("sessions").is_some_and(Value::is_array) {
        entry["sessions"] = json!([]);
    }
    let Some(list) = entry["sessions"].as_array_mut() else { return };
    list.retain(|s| !Waiter::from_json(s).is_some_and(|o| o.same_session(w)));
    list.push(w.to_json());
    let extra = list.len().saturating_sub(MAX_WAITERS);
    list.drain(..extra);
}

/// Forget hits long past their reset. True when any went.
fn prune(hits: &mut Map<String, Value>, now_ms: i64) -> bool {
    let before = hits.len();
    hits.retain(|_, e| {
        let alerted = e.get("alertedAtMs").and_then(num_i64).unwrap_or(0);
        match e.get("resetsAt").and_then(num_i64) {
            Some(at) => at.saturating_mul(1000) > now_ms - KEEP_MS,
            None => e.is_object() && alerted > now_ms - KEEP_MS,
        }
    });
    hits.len() != before
}

fn waiters_of(entry: &Value) -> Vec<Waiter> {
    entry.get("sessions").and_then(Value::as_array).map(|l| l.iter().filter_map(Waiter::from_json).collect()).unwrap_or_default()
}

/// The projects of these sessions, each once, in the order they stopped.
fn unique_projects(waiters: &[Waiter]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for p in waiters.iter().map(Waiter::project).filter(|p| !p.is_empty()) {
        if !out.contains(&p) {
            out.push(p);
        }
    }
    out
}

fn projects_of(entry: &Value) -> Vec<String> {
    unique_projects(&waiters_of(entry))
}

fn hit_of(window: &str, entry: &Value) -> Hit {
    let s = |k: &str| entry.get(k).and_then(Value::as_str).map(String::from);
    Hit {
        window: window.to_string(),
        model: s("model"),
        resets_at: entry.get("resetsAt").and_then(num_i64),
        reset_exact: entry.get("resetExact").and_then(Value::as_bool) == Some(true),
        reset_text: s("resetText"),
    }
}

/// The used-up limits still in force in `st` (state.json): recorded hits whose reset is ahead
/// (or not known, seen within the last hour), and status-line windows at 100 % whose reset is
/// ahead. Soonest reset first.
fn active_in(st: &Value, now_ms: i64) -> Vec<Active> {
    let now = now_ms / 1000;
    let mut out: Vec<Active> = Vec::new();
    for (window, e) in st.get("limitHits").and_then(Value::as_object).into_iter().flatten() {
        let alerted = e.get("alertedAtMs").and_then(num_i64).unwrap_or(0);
        let live = match e.get("resetsAt").and_then(num_i64) {
            Some(at) => at > now,
            None => now_ms - alerted < UNKNOWN_RESET_MS,
        };
        if live {
            out.push(Active { hit: hit_of(window, e), waiters: waiters_of(e), alerted_ms: alerted });
        }
    }
    for (window, w) in st.get("windows").and_then(Value::as_object).into_iter().flatten() {
        if !STATUS_WINDOWS.contains(&window.as_str()) || out.iter().any(|a| &a.hit.window == window) {
            continue;
        }
        let at = w.get("resetsAt").and_then(num_i64).filter(|at| *at > now);
        let used = w.get("usedPercentage").and_then(Value::as_f64).unwrap_or(0.0);
        if let (Some(at), true) = (at, used >= 100.0) {
            out.push(Active { hit: Hit::at(window, at), waiters: Vec::new(), alerted_ms: 0 });
        }
    }
    out.sort_by_key(|a| a.hit.resets_at.unwrap_or(i64::MAX));
    out
}

/// The used-up limits in force now (see `active_in`).
pub fn active(shared: &Shared) -> Vec<Active> {
    let st = shared.state.lock().unwrap();
    active_in(&st, util::now_ms())
}

/// The window's reset the status line or a remote run reported, if it is still ahead.
fn known_reset(st: &Value, window: &str, now: i64) -> Option<i64> {
    if !events::LIMIT_WINDOWS.contains(&window) {
        return None;
    }
    st.pointer(&format!("/windows/{window}/resetsAt")).and_then(num_i64).filter(|at| *at > now)
}

impl Hit {
    fn at(window: &str, resets_at: i64) -> Hit {
        Hit { window: window.to_string(), resets_at: Some(resets_at), reset_exact: true, ..Hit::default() }
    }
}

// ---------------------------------------------------------------- reporting

/// Record a used-up limit seen by `source` ("stopfailure", "status", "remote") and report it if it
/// is news for its window and reset.
pub fn record_hit(app: &AppHandle, shared: &Arc<Shared>, mut hit: Hit, waiter: Option<Waiter>, source: &str) {
    let now_ms = util::now_ms();
    let now = now_ms / 1000;
    let (noted, projects, best) = {
        let mut st = shared.state.lock().unwrap();
        if hit.resets_at.is_none() {
            if let Some(at) = known_reset(&st, &hit.window, now) {
                hit.resets_at = Some(at);
                hit.reset_exact = true;
            } else {
                hit.resets_at = hit.reset_text.as_deref().and_then(|t| parse_reset_text(t, &chrono::Local::now()));
            }
        }
        // A reset that has passed (an old record) or one further ahead than any limit period.
        if hit.resets_at.is_some_and(|at| at < now - 120 || at > now + 40 * 86_400) {
            drop(st);
            util::log_line(&["limit-hit-ignored", source, &hit.window, &hit.resets_at.unwrap_or(0).to_string()]);
            return;
        }
        if !st.get("limitHits").is_some_and(Value::is_object) {
            st["limitHits"] = json!({});
        }
        let Some(hits) = st["limitHits"].as_object_mut() else { return };
        let pruned = prune(hits, now_ms);
        let noted = note_hit(hits, &hit, waiter.as_ref(), now_ms);
        let projects = hits.get(&hit.window).map(projects_of).unwrap_or_default();
        // The best reset known for this window (an exact one noted earlier wins over the text).
        let best = hits.get(&hit.window).map(|e| hit_of(&hit.window, e)).filter(|b| b.resets_at.is_some());
        if noted.changed || pruned {
            events::save_state(&st);
        }
        (noted, projects, best)
    };
    // The status line repeats itself on every refresh: only what changed is logged.
    if noted.changed {
        let project = waiter.as_ref().map(Waiter::project).unwrap_or_default();
        let entrypoint = waiter.as_ref().map(|w| w.entrypoint.as_str()).unwrap_or("");
        let reset = hit.resets_at.map_or_else(|| "?".to_string(), |at| at.to_string());
        util::log_line(&["limit-hit", source, &hit.window, &reset, if noted.news { "new" } else { "known" }, entrypoint, &project]);
    }
    // A Claude Code session on the PC that stopped on it may be continued at the reset
    // (schedule.rs): asked about on the PC card and the phone, or scheduled.
    let plan = match (&waiter, source) {
        (Some(w), "stopfailure") => crate::schedule::on_pc_stop(shared, best.as_ref().unwrap_or(&hit), w, false),
        _ => None,
    };
    if noted.news {
        alert(app, shared, &hit, &projects, plan.as_ref());
    } else if let Some(p) = &plan {
        // A limit reported already: this session's own question (or notice) alone.
        alert(app, shared, best.as_ref().unwrap_or(&hit), &[], Some(p));
    }
    // The reset timer (logo and phone at the reset) learns the window too: the Claude app's
    // sessions have no status line to tell it.
    if source != "status" && hit.reset_exact && events::LIMIT_WINDOWS.contains(&hit.window.as_str()) {
        if let Some(at) = hit.resets_at {
            let mut rl = Map::new();
            rl.insert(hit.window.clone(), json!({"resets_at": at, "used_percentage": 100.0}));
            events::handle_status(app, shared, &json!({"rate_limits": Value::Object(rl)}));
        }
    }
}

/// The status line (or a remote run's rate-limit report) has `window` at 100 % until `resets_at`.
pub fn status_hit(app: &AppHandle, shared: &Arc<Shared>, window: &str, resets_at: i64) {
    if STATUS_WINDOWS.contains(&window) {
        record_hit(app, shared, Hit::at(window, resets_at), None, "status");
    }
}

/// Claude Code reported that it will not continue `session_id` itself after all
/// (quota_auto_resume_disabled): if it waits on a limit in force, it may be continued at the reset.
pub fn replan(app: &AppHandle, shared: &Arc<Shared>, session_id: &str) {
    if session_id.is_empty() {
        return;
    }
    let found = active(shared)
        .into_iter()
        .find_map(|a| a.waiters.iter().find(|w| w.session_id == session_id).cloned().map(|w| (a.hit.clone(), w)));
    let Some((hit, w)) = found else { return };
    if let Some(p) = crate::schedule::on_pc_stop(shared, &hit, &w, true) {
        alert(app, shared, &hit, &[], Some(&p));
    }
}

/// The StopFailure hook (matcher rate_limit): the turn ended on a rate-limit error. HeadsUp's own
/// remote runs are left to the remote runner, which reads their stream.
pub fn on_stop_failure(app: &AppHandle, shared: &Arc<Shared>, event: &Value) {
    let s = |k: &str| event.get(k).and_then(Value::as_str).unwrap_or("").to_string();
    let session_id = s("session_id");
    if s("error") != "rate_limit" || crate::remote::is_remote_session(shared, Some(session_id.as_str())) {
        return;
    }
    let text = s("last_assistant_message");
    let from_text = parse_message(&text);
    if from_text.is_some() {
        // The Claude app may show a notification for the same moment: this alert stands for it.
        shared.last_hook_banner_ms.store(util::now_ms(), Ordering::SeqCst);
    }
    // Added by the hook script from CLAUDE_CODE_ENTRYPOINT.
    let env_entry = clean_entrypoint(&s("headsup_entrypoint"));
    let (cwd, path) = (s("cwd"), s("transcript_path"));
    // The mode the session stopped in: its continue at the reset runs in no looser one.
    let permission_mode = clean_entrypoint(&s("permission_mode"));
    let (app, shared) = (app.clone(), shared.clone());
    // Off the request: the hook is waiting for its reply, and the transcript may need a moment.
    std::thread::spawn(move || {
        let tries = if from_text.is_some() { 5 } else { 2 };
        let mut rec = None;
        for i in 0..tries {
            if i > 0 {
                std::thread::sleep(Duration::from_millis(400));
            }
            rec = last_api_error(&path).filter(|r| matches_hook(r, &text, util::now_ms()));
            if rec.is_some() {
                break;
            }
        }
        let usage_error = rec.as_ref().and_then(|r| r.get("apiError")).and_then(Value::as_str) == Some("usage_limit_reached");
        let hit = combine(from_text, rec.as_ref().and_then(record_quota), usage_error);
        let rec_entry = clean_entrypoint(rec.as_ref().and_then(|r| r.get("entrypoint")).and_then(Value::as_str).unwrap_or(""));
        let project = util::project_name(Some(&Value::String(cwd.clone())));
        util::log_line(&[
            "stopfailure",
            if hit.is_some() { "usage-limit" } else { "other" },
            &project,
            &format!("env={}", if env_entry.is_empty() { "-" } else { env_entry.as_str() }),
            &format!("transcript={}", if rec_entry.is_empty() { "-" } else { rec_entry.as_str() }),
        ]);
        let Some(hit) = hit else { return };
        let entrypoint = if env_entry.is_empty() { rec_entry } else { env_entry };
        let waiter = Waiter { session_id, cwd, transcript_path: path, entrypoint, permission_mode, at_ms: util::now_ms() };
        record_hit(&app, &shared, hit, Some(waiter), "stopfailure");
    });
}

/// "5-hour limit used up", "Opus limit used up", "Usage limit used up".
pub fn label(t: &i18n::Strings, window: &str, model: Option<&str>) -> String {
    let model = match window {
        "five_hour" => return t.limit_hit_five_hour.to_string(),
        "seven_day" => return t.limit_hit_seven_day.to_string(),
        "seven_day_opus" => Some("Opus"),
        "seven_day_sonnet" => Some("Sonnet"),
        "seven_day_overage_included" => model.or(Some("Fable")),
        _ => model,
    };
    match model {
        Some(m) => t.limit_hit_model.replace("{model}", m),
        None => t.limit_hit_generic.to_string(),
    }
}

/// "a, b, c +2"
fn project_list(projects: &[String]) -> String {
    let mut s = projects.iter().take(3).cloned().collect::<Vec<_>>().join(", ");
    if projects.len() > 3 {
        s.push_str(&format!(" +{}", projects.len() - 3));
    }
    s
}

/// (what ran out, when it resets, who waits for it); the last two may be empty.
fn texts<Tz: TimeZone>(t: &i18n::Strings, hit: &Hit, projects: &[String], now: &DateTime<Tz>) -> (String, String, String)
where
    Tz::Offset: std::fmt::Display,
{
    let reset = match (hit.resets_at, &hit.reset_text) {
        (Some(at), _) => reset_phrase(t, at, now),
        (None, Some(text)) => t.limit_resets_raw.replace("{text}", text),
        (None, None) => String::new(),
    };
    let waiting = if projects.is_empty() { String::new() } else { t.limit_waiting.replace("{list}", &project_list(projects)) };
    (label(t, &hit.window, hit.model.as_deref()), reset, waiting)
}

pub fn join_nonempty(parts: &[&str]) -> String {
    parts.iter().filter(|p| !p.is_empty()).copied().collect::<Vec<_>>().join(SEP)
}

/// The banner on the PC (events.limit_hit) and, at once, the phone (phone.events.limitHit).
/// With `plan` (a session to continue at the reset, schedule.rs) the PC shows a card with its
/// buttons instead of the banner, and the phone message carries the same buttons.
fn alert(app: &AppHandle, shared: &Arc<Shared>, hit: &Hit, projects: &[String], plan: Option<&crate::schedule::Plan>) {
    let cfg = shared.cfg();
    let t = shared.t();
    let (label, reset, waiting) = texts(t, hit, projects, &chrono::Local::now());
    let line = plan.map(|p| crate::schedule::plan_line(shared, p)).unwrap_or_default();
    if config::get_bool(&cfg, "events.limit_hit", true) {
        shared.last_hook_banner_ms.store(util::now_ms(), Ordering::SeqCst);
        let subtitle = join_nonempty(&[&label, &reset]);
        match plan {
            Some(p) => {
                let body = [waiting.as_str(), line.as_str()].into_iter().filter(|s| !s.is_empty()).collect::<Vec<_>>().join("\n");
                crate::schedule::show_card(app, shared, &subtitle, &body, p);
            }
            None => events::banner(app, shared, true, t.code_name, &subtitle, &waiting, "limit", None),
        }
    }
    // Urgent: it is news while the user is away too (push skips the per-kind switch for those).
    if config::get_bool(&cfg, "phone.events.limitHit", true) {
        let mut text = format!("⛔ {}", mask::secrets(&join_nonempty(&[&label, &reset])));
        let mut html = format!("⛔ <b>{}</b>", phone::esc(&label));
        if !reset.is_empty() {
            html.push_str(&format!("{SEP}{}", phone::esc(&reset)));
        }
        for extra in [&waiting, &line] {
            if !extra.is_empty() {
                text.push_str(&format!("\n{}", mask::secrets(extra)));
                html.push_str(&format!("\n<i>{}</i>", phone::esc(extra)));
            }
        }
        let buttons = plan.map(|p| crate::schedule::plan_buttons(shared, p)).unwrap_or_default();
        let msg = PhoneMsg { urgent: true, kind: "limitHit", text, html: Some(html.clone()), buttons, pending_id: None };
        match plan {
            // Its message is edited once the question is answered (on the PC or the phone).
            Some(p) => {
                let (shared2, id) = (shared.clone(), p.id.clone());
                phone::push_tracked(shared, msg, move |mid| crate::schedule::note_phone_msg(&shared2, &id, mid, &html));
            }
            None => phone::push(shared, msg),
        }
    }
}

/// `--send test-limit-hit`: a sample banner (PC only; nothing is recorded).
pub fn test_banner(app: &AppHandle, shared: &Arc<Shared>) {
    let t = shared.t();
    let hit = Hit::at("five_hour", util::now_secs() + 2 * 3600 + 13 * 60);
    let (label, reset, waiting) = texts(t, &hit, &["demo".to_string()], &chrono::Local::now());
    let subtitle = join_nonempty(&[&label, &reset, t.test]);
    events::banner(app, shared, true, t.code_name, &subtitle, &waiting, "limit", None);
}

/// Lines for the phone's /status (Telegram HTML): the used-up limits in force and who waits.
pub fn status_lines(shared: &Shared) -> Vec<String> {
    let t = shared.t();
    let now = chrono::Local::now();
    active(shared)
        .iter()
        .map(|a| {
            let (label, reset, waiting) = texts(t, &a.hit, &unique_projects(&a.waiters), &now);
            format!("⛔ {}", phone::esc(&join_nonempty(&[&label, &reset, &waiting])))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::FixedOffset;

    fn istanbul(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> DateTime<FixedOffset> {
        FixedOffset::east_opt(3 * 3600).unwrap().with_ymd_and_hms(y, mo, d, h, mi, 0).unwrap()
    }

    #[test]
    fn usage_limit_messages() {
        let h = parse_message("You've hit your session limit · resets 11:30pm (Europe/Istanbul)").unwrap();
        assert_eq!(h.window, "five_hour");
        assert_eq!(h.reset_text.as_deref(), Some("11:30pm (Europe/Istanbul)"));
        assert_eq!(h.resets_at, None);
        let h = parse_message("You've hit your weekly limit · resets Oct 25, 9am (Europe/Istanbul) · progress saved").unwrap();
        assert_eq!((h.window.as_str(), h.reset_text.as_deref()), ("seven_day", Some("Oct 25, 9am (Europe/Istanbul)")));
        assert_eq!(parse_message("You've hit your Opus limit · resets 4pm (UTC)").unwrap().window, "seven_day_opus");
        assert_eq!(parse_message("You've hit your Sonnet limit · resets 4pm (UTC)").unwrap().window, "seven_day_sonnet");
        let h = parse_message("You've hit your Fable limit · resets 4pm (UTC)").unwrap();
        assert_eq!((h.window.as_str(), h.model.as_deref()), ("seven_day_overage_included", Some("Fable")));
        // Claude Code before 2.1.2xx, the generic one, and a typographic apostrophe.
        assert_eq!(parse_message("You've hit your limit · resets 4pm (Europe/Istanbul)").unwrap().window, "usage");
        assert_eq!(parse_message("  You've hit your usage limit · resets 4pm (X)\n").unwrap().window, "usage");
        assert_eq!(parse_message("You\u{2019}ve hit your session limit · resets 4am (X)").unwrap().window, "five_hour");
        let h = parse_message("You've hit your Haiku limit · resets 4am (X)").unwrap();
        assert_eq!((h.window.as_str(), h.model.as_deref()), ("usage", Some("Haiku")));
    }

    #[test]
    fn other_rate_limit_errors_are_not_usage_limits() {
        for text in [
            "",
            "API Error: Server is temporarily limiting requests (not your usage limit) · Rate limited",
            "API Error: Request rejected (429) · Too many requests",
            "API Error: Rate limit reached",
            "Opus is experiencing high load, please use /model to switch to Sonnet",
            "You've hit your monthly spend limit. Run /usage-credits to manage your limit and keep using Fable 5 or switch models to continue this chat.",
            "You've hit your org's monthly spend limit · ask your admin to raise it at x · your session limit resets 4pm (X)",
            "You've hit your individual usage limit · resets 4pm (X)",
            "You've reached your Fable 5 limit. Run /usage-credits to continue or switch models with /model.",
            "Fable 5 requires usage credits. Run /usage-credits to continue or switch models with /model.",
            "You've used 90% of your session limit · resets 4pm (X)",
            "You've hit your session limit",
        ] {
            assert_eq!(parse_message(text), None, "{text}");
        }
    }

    #[test]
    fn reset_text_read_in_local_time() {
        let now = istanbul(2026, 10, 22, 20, 5);
        // The example from a real transcript: quotaLimits.resetsAt was 1792701000.
        assert_eq!(parse_reset_text("11:30pm (Europe/Istanbul)", &now), Some(1_792_701_000));
        assert_eq!(parse_reset_text("4am (Europe/Istanbul)", &now), Some(istanbul(2026, 10, 23, 4, 0).timestamp()));
        // A few minutes ago: still today (the hit may be reported a little late).
        assert_eq!(parse_reset_text("8pm (X)", &now), Some(istanbul(2026, 10, 22, 20, 0).timestamp()));
        assert_eq!(parse_reset_text("7:30pm (X)", &now), Some(istanbul(2026, 10, 23, 19, 30).timestamp()));
        assert_eq!(parse_reset_text("12am (X)", &now), Some(istanbul(2026, 10, 23, 0, 0).timestamp()));
        assert_eq!(parse_reset_text("12:15pm", &now), Some(istanbul(2026, 10, 23, 12, 15).timestamp()));
        assert_eq!(parse_reset_text("Oct 25, 9am (Europe/Istanbul)", &now), Some(istanbul(2026, 10, 25, 9, 0).timestamp()));
        assert_eq!(parse_reset_text("Jan 3, 2027, 9am (X)", &now), Some(istanbul(2027, 1, 3, 9, 0).timestamp()));
        let dec = istanbul(2026, 12, 30, 10, 0);
        assert_eq!(parse_reset_text("Jan 3, 9am (X)", &dec), Some(istanbul(2027, 1, 3, 9, 0).timestamp()));
        for bad in ["", "soon", "13pm (X)", "0am", "4:75pm", "Foo 3, 9am", "Oct 32, 9am", "in 3 hours"] {
            assert_eq!(parse_reset_text(bad, &now), None, "{bad}");
        }
    }

    #[test]
    fn reset_phrases() {
        let t = &i18n::EN;
        let now = istanbul(2026, 10, 22, 20, 5);
        assert_eq!(reset_phrase(t, 1_792_701_000, &now), "resets at 23:30");
        assert_eq!(reset_phrase(t, istanbul(2026, 10, 23, 4, 0).timestamp(), &now), "resets tomorrow at 04:00");
        assert_eq!(reset_phrase(t, istanbul(2026, 10, 25, 9, 0).timestamp(), &now), "resets on 25.10 at 09:00");
        assert_eq!(reset_phrase(&i18n::TR, 1_792_701_000, &now), "yenilenme: 23:30");
        let hit = Hit { window: "seven_day_opus".into(), resets_at: Some(1_792_701_000), ..Hit::default() };
        let projects: Vec<String> = ["a", "b", "c", "d", "e"].iter().map(|s| s.to_string()).collect();
        assert_eq!(
            texts(t, &hit, &projects, &now),
            ("Opus limit used up".to_string(), "resets at 23:30".to_string(), "Waiting: a, b, c +2".to_string())
        );
        let hit = Hit { window: "usage".into(), reset_text: Some("4pm (X)".into()), ..Hit::default() };
        assert_eq!(texts(t, &hit, &[], &now), ("Usage limit used up".to_string(), "resets 4pm (X)".to_string(), String::new()));
        assert_eq!(label(&i18n::TR, "five_hour", None), "5 saatlik limit doldu");
        assert_eq!(label(&i18n::TR, "usage", Some("Haiku")), "Haiku limiti doldu");
    }

    /// The error record Claude Code 2.1.301 wrote to a transcript for a 5-hour limit.
    fn record() -> Value {
        json!({
            "type": "assistant", "isApiErrorMessage": true, "apiErrorStatus": 429, "apiError": "usage_limit_reached",
            "error": "rate_limit", "entrypoint": "claude-desktop", "timestamp": "2026-10-22T20:00:37.120Z",
            "message": {"model": "<synthetic>", "content": [{"type": "text", "text": "You've hit your session limit · resets 11:30pm (Europe/Istanbul)"}]},
            "quotaLimits": {"status": "rejected", "resetsAt": 1792701000, "rateLimitType": "five_hour", "overageStatus": "rejected",
                            "overageDisabledReason": "out_of_credits", "isUsingOverage": false, "unifiedRateLimitFallbackAvailable": false}
        })
    }

    #[test]
    fn transcript_record_gives_the_exact_reset() {
        let rec = record();
        let text = "You've hit your session limit · resets 11:30pm (Europe/Istanbul)";
        let lines = format!(
            "{{\"partial line\n{}\n{}\n{{\"type\":\"user\",\"message\":{{\"content\":\"hi\"}}}}\n",
            json!({"isApiErrorMessage": true, "message": {"content": [{"type": "text", "text": "older"}]}}),
            rec
        );
        let found = last_api_error_in(&lines).unwrap();
        assert_eq!(found, rec);
        let now_ms = 1_792_699_237_000; // 20:00:37Z
        assert!(matches_hook(&found, text, now_ms));
        assert!(matches_hook(&found, "", now_ms + 60_000));
        // Another message, written long ago: an older error, not this one.
        assert!(!matches_hook(&found, "You've hit your weekly limit · resets 9am (X)", now_ms + 3_600_000));
        let usage = found.get("apiError").and_then(Value::as_str) == Some("usage_limit_reached");
        let hit = combine(parse_message(text), record_quota(&found), usage).unwrap();
        assert_eq!(
            hit,
            Hit {
                window: "five_hour".into(),
                resets_at: Some(1_792_701_000),
                reset_exact: true,
                reset_text: Some("11:30pm (Europe/Istanbul)".into()),
                model: None
            }
        );
        // The error code alone (a message HeadsUp does not know) is enough; the quota's type wins.
        let mut other = rec.clone();
        other["quotaLimits"]["rateLimitType"] = json!("seven_day");
        let hit = combine(parse_message("You're out of usage credits · resets 4pm"), record_quota(&other), true).unwrap();
        assert_eq!((hit.window.as_str(), hit.resets_at), ("seven_day", Some(1_792_701_000)));
        // Extra usage taking over: the quota says nothing; the message still counts.
        other["quotaLimits"]["isUsingOverage"] = json!(true);
        assert_eq!(combine(None, record_quota(&other), false), None);
        let hit = combine(parse_message(text), record_quota(&other), false).unwrap();
        assert_eq!((hit.resets_at, hit.reset_exact), (None, false));
        // A transient 429: no message match, no error code.
        assert_eq!(combine(parse_message("API Error: Server is temporarily limiting requests (not your usage limit)"), None, false), None);
        assert_eq!(clean_entrypoint(" claude-desktop\"; x"), "claude-desktopx");
    }

    #[test]
    fn stream_json_reports() {
        let info = json!({"status": "rejected", "resetsAt": 1792701000, "rateLimitType": "five_hour", "overageStatus": "rejected", "isUsingOverage": false});
        let ev = json!({"type": "rate_limit_event", "rate_limit_info": info});
        assert_eq!(from_stream(&ev), Some(Hit::at("five_hour", 1_792_701_000)));
        let mut allowed = ev.clone();
        allowed["rate_limit_info"]["status"] = json!("allowed_warning");
        assert_eq!(from_stream(&allowed), None);
        let mut overage = ev.clone();
        overage["rate_limit_info"]["overageStatus"] = json!("allowed");
        assert_eq!(from_stream(&overage), None);
        let msg = json!({
            "type": "assistant", "error": "rate_limit", "is_api_error_message": true, "api_error": "usage_limit_reached",
            "message": {"model": "<synthetic>", "content": [{"type": "text", "text": "You've hit your weekly limit · resets Oct 25, 9am (X)"}]},
            "api_error_params": {"rate_limit_info": {"status": "rejected", "resetsAt": 1792900000, "rateLimitType": "seven_day"}}
        });
        let hit = from_stream(&msg).unwrap();
        assert_eq!((hit.window.as_str(), hit.resets_at, hit.reset_exact), ("seven_day", Some(1_792_900_000), true));
        let result = json!({"type": "result", "subtype": "success", "is_error": true, "api_error": "usage_limit_reached",
                            "result": "You've hit your session limit · resets 11:30pm (X)"});
        assert_eq!(from_stream(&result).unwrap().window, "five_hour");
        let transient = json!({"type": "assistant", "error": "rate_limit",
                               "message": {"content": [{"type": "text", "text": "API Error: Server is temporarily limiting requests (not your usage limit) · x"}]}});
        assert_eq!(from_stream(&transient), None);
        assert_eq!(from_stream(&json!({"type": "result", "is_error": false, "result": "done"})), None);
        assert_eq!(from_stream(&json!({"type": "assistant", "message": {"content": []}})), None);
    }

    fn waiter(id: &str, cwd: &str) -> Waiter {
        Waiter { session_id: id.into(), cwd: cwd.into(), entrypoint: "claude-desktop".into(), at_ms: 1, ..Waiter::default() }
    }

    #[test]
    fn one_alert_per_window_and_reset() {
        let mut hits = Map::new();
        let now = 1_792_700_437_000;
        let at = 1_792_701_000;
        let hit = Hit::at("five_hour", at);
        assert_eq!(note_hit(&mut hits, &hit, Some(&waiter("s1", "C:/p/alpha")), now), Noted { news: true, changed: true });
        // The same reset again (another session, the status line a minute off, the same session).
        assert_eq!(note_hit(&mut hits, &Hit::at("five_hour", at + 60), Some(&waiter("s2", "C:/p/beta")), now + 5_000).news, false);
        assert_eq!(note_hit(&mut hits, &hit, None, now + 6_000), Noted { news: false, changed: false });
        assert_eq!(note_hit(&mut hits, &hit, Some(&waiter("s1", "C:/p/alpha")), now + 7_000).news, false);
        assert_eq!(projects_of(&hits["five_hour"]), vec!["beta", "alpha"]);
        assert_eq!(hits["five_hour"]["resetsAt"], json!(at));
        // No reset known, before the known one: the same limit.
        let unknown = Hit { window: "five_hour".into(), ..Hit::default() };
        assert!(!note_hit(&mut hits, &unknown, None, now + 8_000).news);
        // Another window is its own news; the next 5-hour window is news again.
        assert!(note_hit(&mut hits, &Hit::at("seven_day", at + 86_400), None, now).news);
        let next = Hit::at("five_hour", at + 5 * 3600);
        assert!(note_hit(&mut hits, &next, Some(&waiter("s3", "C:/p/gamma")), (at + 60) * 1000).news);
        assert_eq!(projects_of(&hits["five_hour"]), vec!["gamma"]);
    }

    #[test]
    fn unknown_resets_and_later_exact_ones() {
        let mut hits = Map::new();
        let now = 1_792_700_437_000;
        let unknown = Hit { window: "usage".into(), reset_text: Some("4pm (X)".into()), ..Hit::default() };
        assert!(note_hit(&mut hits, &unknown, None, now).news);
        assert!(!note_hit(&mut hits, &unknown, None, now + 30 * 60_000).news);
        // An hour on, with no reset known, it is news again.
        assert!(note_hit(&mut hits, &unknown, None, now + 61 * 60_000).news);
        // A reset read from the text, then the exact one: filled in, not a second alert.
        let read = Hit { window: "five_hour".into(), resets_at: Some(1_792_701_000), ..Hit::default() };
        assert!(note_hit(&mut hits, &read, None, now).news);
        let exact = Hit::at("five_hour", 1_792_701_000 + 120);
        assert_eq!(note_hit(&mut hits, &exact, None, now + 1_000), Noted { news: false, changed: true });
        assert_eq!(hits["five_hour"]["resetsAt"], json!(1_792_701_120));
        assert_eq!(hits["five_hour"]["resetExact"], json!(true));
        assert_eq!(note_hit(&mut hits, &read, None, now + 2_000), Noted { news: false, changed: false });
        assert_eq!(hits["five_hour"]["resetsAt"], json!(1_792_701_120));
    }

    #[test]
    fn waiters_are_capped_and_hits_forgotten() {
        let mut entry = json!({"sessions": []});
        for i in 0..(MAX_WAITERS + 5) {
            add_waiter(&mut entry, &waiter(&format!("s{i}"), "C:/p/x"));
        }
        let list = waiters_of(&entry);
        assert_eq!(list.len(), MAX_WAITERS);
        assert_eq!(list[0].session_id, "s5");
        // Sessions with no id are told apart by folder.
        add_waiter(&mut entry, &waiter("", "C:/p/y"));
        add_waiter(&mut entry, &waiter("", "C:/p/y"));
        assert_eq!(waiters_of(&entry).iter().filter(|w| w.cwd == "C:/p/y").count(), 1);
        assert_eq!(Waiter::from_json(&json!({"sessionId": "", "cwd": ""})), None);

        let now = 1_792_700_437_000;
        let mut hits = Map::new();
        hits.insert("five_hour".into(), json!({"resetsAt": now / 1000 - 8 * 86_400, "alertedAtMs": 0}));
        hits.insert("seven_day".into(), json!({"resetsAt": now / 1000 - 86_400, "alertedAtMs": 0}));
        hits.insert("usage".into(), json!({"resetsAt": null, "alertedAtMs": now - 8 * 86_400_000}));
        hits.insert("junk".into(), json!(3));
        assert!(prune(&mut hits, now));
        assert_eq!(hits.keys().collect::<Vec<_>>(), vec!["seven_day"]);
        assert!(!prune(&mut hits, now));
    }

    #[test]
    fn limits_in_force() {
        let now = 1_792_700_437_000;
        let st = json!({
            "limitHits": {
                "five_hour": {"resetsAt": 1_792_701_000, "resetExact": true, "alertedAtMs": now - 1000,
                              "sessions": [{"sessionId": "s1", "cwd": "C:/p/alpha", "entrypoint": "cli", "atMs": 5}]},
                "seven_day": {"resetsAt": 1_792_000_000, "alertedAtMs": 0},
                "usage": {"resetsAt": null, "alertedAtMs": now - 10 * 60_000, "model": "Haiku"}
            },
            "windows": {
                "five_hour": {"resetsAt": 1_792_701_000, "usedPercentage": 100},
                "seven_day_opus": {"resetsAt": 1_793_000_000, "usedPercentage": 100},
                "seven_day_sonnet": {"resetsAt": 1_793_000_000, "usedPercentage": 99},
                "spend_limit": {"resetsAt": 1_793_000_000, "usedPercentage": 100}
            }
        });
        let active = active_in(&st, now);
        let windows: Vec<&str> = active.iter().map(|a| a.hit.window.as_str()).collect();
        assert_eq!(windows, vec!["five_hour", "seven_day_opus", "usage"]);
        assert_eq!(active[0].waiters, vec![Waiter { session_id: "s1".into(), cwd: "C:/p/alpha".into(), entrypoint: "cli".into(), at_ms: 5, ..Waiter::default() }]);
        assert_eq!(active[2].hit.model.as_deref(), Some("Haiku"));
        assert!(active_in(&st, 1_793_000_001_000).is_empty());
        assert_eq!(known_reset(&st, "five_hour", now / 1000), Some(1_792_701_000));
        assert_eq!(known_reset(&st, "usage", now / 1000), None);
    }
}
