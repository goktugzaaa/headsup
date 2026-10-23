//! The settings window (label "settings", app/settings.html): created on demand from its entry
//! in tauri.conf.json, plus what its page calls. Every change is checked against a whitelist,
//! written to config.json and applied the way tray > Reload config applies it. Tokens can be
//! set or cleared but are never read back; buttons reuse the tray's handlers.
use crate::{config, events, installer, util, Shared};
use serde_json::{json, Map, Value};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tauri::{AppHandle, Manager, WebviewWindowBuilder};

pub const LABEL: &str = "settings";

/// What a setting accepts.
#[derive(Clone, Copy, Debug)]
enum Kind {
    Bool,
    /// A whole number within these bounds (inclusive).
    Int(u64, u64),
    /// Exactly one of these spellings.
    OneOf(&'static [&'static str]),
    /// toasts.watchApps: app names or app ids.
    Apps,
    /// An https address without user:password@ (the ntfy server; an ntfy token goes to it).
    Url,
    /// An ntfy topic; empty turns ntfy off.
    Topic,
    /// A token: set (non-empty) or cleared (empty), never read back.
    Secret,
    /// A keyboard shortcut (hotkey::parse); empty turns it off.
    Hotkey,
}

const MAX_APPS: usize = 20;
const MAX_APP_CHARS: usize = 60;
const MAX_TEXT_CHARS: usize = 200;

/// Everything the settings window may change. Anything else (port, overlayExe, overlayArgs,
/// installed*, original*, startupWanted, the paired chat id, remote.projects, ...) is refused.
const SETTINGS: &[(&str, Kind)] = &[
    ("language", Kind::OneOf(&["auto", "en", "tr"])),
    ("sound", Kind::Bool),
    ("logo.style", Kind::OneOf(&["headsup", "claude"])),
    ("notification.durationMs", Kind::Int(3_000, 30_000)),
    ("notification.showLastMessage", Kind::Bool),
    ("limits.five_hour", Kind::Bool),
    ("limits.seven_day", Kind::Bool),
    ("limits.spend_limit", Kind::Bool),
    ("limits.continueAtReset", Kind::OneOf(&["ask", "always", "off"])),
    ("logo.dismiss", Kind::OneOf(&["anyClick", "logoClick", "auto"])),
    ("logo.durationMs", Kind::Int(3_000, 60_000)),
    ("events.stop", Kind::Bool),
    ("events.permission_request", Kind::Bool),
    ("events.question", Kind::Bool),
    ("events.permission_prompt", Kind::Bool),
    ("events.agent_needs_input", Kind::Bool),
    ("events.idle_prompt", Kind::Bool),
    ("events.quota_auto_resume_fired", Kind::Bool),
    ("events.limit_hit", Kind::Bool),
    ("toasts.enabled", Kind::Bool),
    ("toasts.watchApps", Kind::Apps),
    ("interactive.enabled", Kind::Bool),
    ("interactive.waitMs", Kind::Int(10_000, 120_000)),
    ("interactive.skipWhenActive", Kind::Bool),
    ("hotkeys.hide", Kind::Hotkey),
    ("phone.enabled", Kind::Bool),
    ("phone.onlyWhenIdleMs", Kind::Int(0, 3_600_000)),
    ("phone.events.stop", Kind::Bool),
    ("phone.events.permission", Kind::Bool),
    ("phone.events.question", Kind::Bool),
    ("phone.events.limit", Kind::Bool),
    ("phone.events.limitHit", Kind::Bool),
    ("phone.telegram.token", Kind::Secret),
    ("phone.ntfy.server", Kind::Url),
    ("phone.ntfy.topic", Kind::Topic),
    ("phone.ntfy.token", Kind::Secret),
    // Switched through the tray's toggle (pairing check, banner, stopping a running task).
    ("remote.enabled", Kind::Bool),
    ("remote.permissionMode", Kind::OneOf(&["default", "plan", "acceptEdits"])),
    ("remote.maxRunMinutes", Kind::Int(5, 240)),
    ("remote.continueAtReset", Kind::Bool),
];

/// The window is being built (a second request meanwhile is dropped).
static OPENING: AtomicBool = AtomicBool::new(false);

// ---------------------------------------------------------------- window

/// Show the settings window, creating it if needed; an open one is brought to the front.
pub fn open(app: &AppHandle, shared: &Shared) {
    if let Some(w) = app.get_webview_window(LABEL) {
        let _ = w.unminimize();
        let _ = w.show();
        let _ = w.set_focus();
        return;
    }
    let title = shared.t().settings_title;
    let app = app.clone();
    // Building a webview on the main thread (tray events run there) deadlocks on Windows.
    std::thread::spawn(move || {
        if OPENING.swap(true, Ordering::SeqCst) {
            return;
        }
        let built = build(&app, title);
        OPENING.store(false, Ordering::SeqCst);
        match built {
            Ok(w) => {
                let _ = w.set_focus();
            }
            Err(e) => util::log_line(&["settings-error", &e]),
        }
    });
}

/// From its tauri.conf.json entry (created there with `create: false`): WebView2 needs the same
/// browser arguments in every window of the process, and they are kept in one place there.
fn build(app: &AppHandle, title: &str) -> Result<tauri::WebviewWindow, String> {
    let conf = app
        .config()
        .app
        .windows
        .iter()
        .find(|w| w.label == LABEL)
        .cloned()
        .ok_or("no settings window in tauri.conf.json")?;
    WebviewWindowBuilder::from_config(app, &conf)
        .map_err(|e| e.to_string())?
        .title(title)
        // The page never leaves the app's own pages (links open in the browser through
        // settings_action instead).
        .on_navigation(|url| url.scheme() == "tauri" || url.host_str() == Some("tauri.localhost"))
        .build()
        .map_err(|e| e.to_string())
}

/// After Reload config: the window title follows the language.
pub fn on_reload(app: &AppHandle, shared: &Shared) {
    if let Some(w) = app.get_webview_window(LABEL) {
        let _ = w.set_title(shared.t().settings_title);
    }
}

/// The settings commands answer only to the settings window, and only it gets them.
pub fn check_window(window: &tauri::WebviewWindow) -> Result<(), String> {
    if window.label() == LABEL {
        Ok(())
    } else {
        Err("not allowed from this window".to_string())
    }
}

// ---------------------------------------------------------------- reading

fn describe(kind: Kind) -> Value {
    match kind {
        Kind::Bool => json!({"type": "bool"}),
        Kind::Int(min, max) => json!({"type": "int", "min": min, "max": max}),
        Kind::OneOf(options) => json!({"type": "choice", "options": options}),
        Kind::Apps => json!({"type": "apps", "maxItems": MAX_APPS, "maxChars": MAX_APP_CHARS}),
        Kind::Url => json!({"type": "url", "maxChars": MAX_TEXT_CHARS}),
        Kind::Topic => json!({"type": "topic", "maxChars": 64}),
        Kind::Secret => json!({"type": "secret"}),
        Kind::Hotkey => json!({"type": "hotkey"}),
    }
}

/// The value the page shows for `key`: what the overlay actually goes by, so a value it would
/// not accept shows as the one it uses instead.
fn current(cfg: &Value, defaults: &Value, key: &str, kind: Kind) -> Value {
    match kind {
        Kind::Bool => json!(config::get_bool(cfg, key, config::get_bool(defaults, key, false))),
        Kind::Int(..) => json!(config::get_u64(cfg, key, config::get_u64(defaults, key, 0))),
        Kind::OneOf(options) => {
            let s = config::get_str(cfg, key).unwrap_or_default();
            json!(match key {
                "logo.style" => config::logo_style(cfg),
                "logo.dismiss" => config::logo_dismiss(cfg),
                // i18n::pick: "auto" (or nothing) follows the OS, "tr" is Turkish, anything else English.
                "language" if s.trim().is_empty() || s == "auto" => "auto",
                "language" if s.eq_ignore_ascii_case("tr") => "tr",
                "language" => "en",
                _ => options.iter().find(|o| **o == s).copied().unwrap_or(options[0]),
            })
        }
        Kind::Apps => json!(config::get_str_list(cfg, key)),
        Kind::Url | Kind::Topic | Kind::Hotkey => json!(config::get_str(cfg, key).unwrap_or_default()),
        Kind::Secret => Value::Null,
    }
}

fn secret_set(cfg: &Value, key: &str) -> bool {
    config::get_str(cfg, key).is_some_and(|s| !s.trim().is_empty())
}

/// What the page shows: the settings it can change (never a token or the paired chat id), how
/// each may be changed, and what is derived from elsewhere.
pub fn get(shared: &Shared) -> Value {
    let cfg = shared.cfg();
    let defaults = config::defaults();
    let (mut values, mut schema) = (Map::new(), Map::new());
    for &(key, kind) in SETTINGS {
        schema.insert(key.to_string(), describe(kind));
        if !matches!(kind, Kind::Secret) {
            values.insert(key.to_string(), current(&cfg, &defaults, key, kind));
        }
    }
    // On only while it is usable (a paired chat; not switched off for this session).
    values.insert("remote.enabled".to_string(), json!(crate::remote::enabled(shared)));
    let tg = crate::telegram::creds(&cfg);
    json!({
        "version": env!("CARGO_PKG_VERSION"),
        "lang": *shared.lang_code.lock().unwrap(),
        "values": values,
        "schema": schema,
        "telegram": {"tokenSet": tg.is_some(), "paired": tg.is_some_and(|c| !c.chat_id.is_empty())},
        "ntfy": {"tokenSet": secret_set(&cfg, "phone.ntfy.token")},
        // Whether hotkeys.hide is registered (another app may own the same shortcut).
        "hotkey": crate::hotkey::status_json(),
        "codex": installer::codex_installed(),
        "startup": installer::startup_enabled(),
    })
}

// ---------------------------------------------------------------- changing

/// "123456789:AA…": the bot id, a colon and the secret part.
fn telegram_token_ok(s: &str) -> bool {
    s.split_once(':').is_some_and(|(id, secret)| {
        !id.is_empty()
            && id.chars().all(|c| c.is_ascii_digit())
            && secret.len() >= 20
            && secret.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    })
}

/// The value to write for `key`, or why `value` is refused. Errors name the key, never the value.
fn check(key: &str, kind: Kind, value: &Value) -> Result<Value, String> {
    let refuse = |why: String| Err(format!("{key}: {why}"));
    let text = || value.as_str().map(str::trim);
    let plain = |s: &str| !s.chars().any(|c| c.is_whitespace() || c.is_control());
    match kind {
        Kind::Bool => value.as_bool().map(Value::Bool).ok_or(format!("{key}: expected true or false")),
        Kind::Int(min, max) => match value.as_u64() {
            Some(n) if (min..=max).contains(&n) => Ok(json!(n)),
            _ => refuse(format!("expected a whole number from {min} to {max}")),
        },
        Kind::OneOf(options) => match value.as_str() {
            Some(s) if options.contains(&s) => Ok(json!(s)),
            _ => refuse(format!("expected one of {}", options.join(", "))),
        },
        Kind::Apps => {
            let Some(list) = value.as_array() else { return refuse("expected a list of app names".into()) };
            let mut apps: Vec<&str> = Vec::new();
            for name in list {
                let Some(name) = name.as_str().map(str::trim) else { return refuse("expected a list of app names".into()) };
                if name.chars().count() > MAX_APP_CHARS || name.chars().any(char::is_control) {
                    return refuse(format!("an app name has more than {MAX_APP_CHARS} characters"));
                }
                if !name.is_empty() && !apps.iter().any(|a| a.eq_ignore_ascii_case(name)) {
                    apps.push(name);
                }
            }
            if apps.len() > MAX_APPS {
                return refuse(format!("at most {MAX_APPS} apps"));
            }
            Ok(json!(apps))
        }
        Kind::Url => match text() {
            Some(s) if s.chars().count() <= MAX_TEXT_CHARS && plain(s) => {
                let lower = s.to_ascii_lowercase();
                // The host part (up to the first /): no user:password@, which would reach the
                // log with a send error.
                match lower.strip_prefix("https://").map(|h| h.split('/').next().unwrap_or("")) {
                    Some(host) if !host.is_empty() && !host.contains('@') => Ok(json!(s)),
                    _ => refuse("expected an address starting with https://".into()),
                }
            }
            _ => refuse("expected an address starting with https://".into()),
        },
        Kind::Topic => match text() {
            Some(s) if s.len() <= 64 && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') => Ok(json!(s)),
            _ => refuse("expected up to 64 letters, digits, - or _".into()),
        },
        Kind::Secret => match text() {
            Some("") => Ok(json!("")),
            Some(s) if key == "phone.telegram.token" && !telegram_token_ok(s) => {
                refuse("not a Telegram bot token (123456789:AA…)".into())
            }
            Some(s) if s.len() <= 256 && plain(s) => Ok(json!(s)),
            _ => refuse("expected a token without spaces".into()),
        },
        // Written in its canonical spelling ("ctrl+shift+h" -> "Ctrl+Shift+H").
        Kind::Hotkey => match value.as_str().map(crate::hotkey::parse) {
            Some(Ok(k)) => Ok(json!(k.map(|k| k.name).unwrap_or_default())),
            Some(Err(why)) => refuse(why),
            None => refuse("expected a shortcut such as Ctrl+Shift+H, or nothing".into()),
        },
    }
}

/// Every key of `patch` (flat, "a.b": value) checked; nothing is written unless all pass.
fn validate(patch: &Value) -> Result<Vec<(&'static str, Value)>, String> {
    let obj = patch.as_object().filter(|o| !o.is_empty()).ok_or("expected an object of settings")?;
    obj.iter()
        .map(|(k, v)| {
            let &(key, kind) = SETTINGS
                .iter()
                .find(|(name, _)| name == k)
                .ok_or_else(|| format!("{}: not a setting that can be changed here", util::truncate(k, 60)))?;
            Ok((key, check(key, kind, v)?))
        })
        .collect()
}

/// Set `path` ("a.b.c") in `cfg`, creating the objects on the way; a non-object in the way is
/// replaced.
fn put(cfg: &mut Value, path: &str, value: Value) {
    let (parents, leaf) = match path.rsplit_once('.') {
        Some((p, l)) => (p.split('.').collect::<Vec<_>>(), l),
        None => (Vec::new(), path),
    };
    let mut node = cfg;
    for part in parents {
        if !node.is_object() {
            *node = json!({});
        }
        node = node.as_object_mut().unwrap().entry(part.to_string()).or_insert_with(|| json!({}));
    }
    if !node.is_object() {
        *node = json!({});
    }
    node.as_object_mut().unwrap().insert(leaf.to_string(), value);
}

/// The bot id of a Telegram token ("123456789:AA…" -> "123456789").
fn bot_id(token: &str) -> Option<&str> {
    token.trim().split_once(':').map(|(id, _)| id)
}

/// Apply a change from the settings window: checked, written to config.json, then everything
/// reloaded as tray > Reload config does (look, menu; the Telegram poll and pairing code only
/// when the phone switch or the bot token changed).
pub fn set(app: &AppHandle, shared: &Arc<Shared>, patch: &Value) -> Result<(), String> {
    let changes = validate(patch)?;
    let (remote, rest): (Vec<_>, Vec<_>) = changes.into_iter().partition(|(k, _)| *k == "remote.enabled");
    if !rest.is_empty() {
        let keys: Vec<&str> = rest.iter().map(|(k, _)| *k).collect();
        {
            let path = util::config_path();
            let _lock = util::lock_config();
            let mut cfg = match util::read_json(&path) {
                Some(v) if v.is_object() => v,
                // Never overwrite a config.json that exists but does not parse.
                _ if path.exists() => return Err(crate::remote::s(shared).config_unreadable.to_string()),
                _ => json!({}),
            };
            // Another bot (or none) is another pairing: the chat pairs with it again (the code
            // is shown), and remote sessions are switched on again afterwards, as after any
            // new pairing. A token renewed for the same bot keeps the pairing.
            if let Some((_, new)) = rest.iter().find(|(k, _)| *k == "phone.telegram.token") {
                let new = new.as_str().unwrap_or("");
                let old = config::get_str(&cfg, "phone.telegram.token").unwrap_or_default();
                if new.is_empty() || bot_id(new) != bot_id(&old) {
                    put(&mut cfg, "phone.telegram.chatId", json!(""));
                    put(&mut cfg, "remote.enabled", json!(false));
                }
            }
            for (key, value) in rest {
                put(&mut cfg, key, value);
            }
            util::write_json(&path, &cfg).map_err(|e| format!("config.json: {e}"))?;
        }
        // Names only: values (tokens) never reach the log.
        util::log_line(&["settings", &keys.join(" ")]);
        let telegram = keys.iter().any(|k| *k == "phone.enabled" || k.starts_with("phone.telegram."));
        events::reload(app, shared, telegram);
    }
    if let Some((_, on)) = remote.first() {
        let on = on.as_bool() == Some(true);
        if on != crate::remote::enabled(shared) {
            // The tray's toggle: its banner says how it went. Turning it off always works.
            crate::tray::on_menu(app, shared, "remote");
            if on != crate::remote::enabled(shared) {
                let rs = crate::remote::s(shared);
                let paired = crate::telegram::creds(&shared.cfg()).is_some_and(|c| !c.chat_id.is_empty());
                return Err((if paired { rs.config_unreadable } else { rs.pair_first }).to_string());
            }
        }
    }
    Ok(())
}

/// A button of the settings window; the tray's handler wherever there is one.
pub fn action(app: &AppHandle, shared: &Arc<Shared>, name: &str) -> Result<(), String> {
    let tray = |id: &str| crate::tray::on_menu(app, shared, id);
    match name {
        "test_banner" => tray("test_notify"),
        "test_logo" => tray("test_logo"),
        "test_limit_offer" => events::handle_event(app, shared, "test-limit-offer", &json!({})),
        "phone_test" => tray("phone_test"),
        "pairing_code" => tray("phone_pair"),
        "open_config" => tray("open_config"),
        "open_log" => tray("open_log"),
        "codex_on" | "codex_off" => {
            let on = name == "codex_on";
            // The tray item toggles; its banners say how it went (and why it did not).
            if installer::codex_installed() != on {
                tray("codex");
                if installer::codex_installed() != on {
                    return Err(shared.t().error.to_string());
                }
            }
        }
        "startup_on" | "startup_off" => installer::set_startup(name == "startup_on", shared.t())?,
        "open_readme" => util::open_path(concat!(env!("CARGO_PKG_REPOSITORY"), "#readme")),
        "open_readme_config" => util::open_path(concat!(env!("CARGO_PKG_REPOSITORY"), "#configuration")),
        _ => return Err(format!("unknown action: {}", util::truncate(name, 40))),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_setting_has_a_default_and_a_valid_one() {
        let d = config::defaults();
        for &(key, kind) in SETTINGS {
            let v = config::get(&d, key).unwrap_or_else(|| panic!("{key} missing from config.default.json"));
            if !matches!(kind, Kind::Secret) {
                assert!(check(key, kind, v).is_ok(), "{key}: default {v} refused");
            }
        }
    }

    #[test]
    fn whitelist_types_and_ranges() {
        let ok = |p: Value| validate(&p).map(|c| c.into_iter().map(|(k, v)| (k.to_string(), v)).collect::<Vec<_>>());
        assert_eq!(ok(json!({"sound": false})).unwrap(), vec![("sound".to_string(), json!(false))]);
        assert!(ok(json!({"sound": "false"})).is_err());
        assert!(ok(json!({"notification.durationMs": 2999})).is_err());
        assert!(ok(json!({"notification.durationMs": 30000})).is_ok());
        assert!(ok(json!({"notification.durationMs": 6000.5})).is_err());
        assert!(ok(json!({"phone.onlyWhenIdleMs": 0})).is_ok());
        assert!(ok(json!({"remote.maxRunMinutes": 241})).is_err());
        assert!(ok(json!({"logo.dismiss": "logoClick"})).is_ok());
        // Canonical spelling only.
        assert!(ok(json!({"logo.dismiss": "logoclick"})).is_err());
        assert!(ok(json!({"remote.permissionMode": "bypassPermissions"})).is_err());
        assert!(ok(json!({"limits.continueAtReset": "always"})).is_ok());
        assert!(ok(json!({"limits.continueAtReset": "Always"})).is_err());
        assert!(ok(json!({"remote.continueAtReset": false})).is_ok());
        assert_eq!(ok(json!({"hotkeys.hide": " ctrl+alt+f9 "})).unwrap(), vec![("hotkeys.hide".to_string(), json!("Ctrl+Alt+F9"))]);
        assert_eq!(ok(json!({"hotkeys.hide": ""})).unwrap(), vec![("hotkeys.hide".to_string(), json!(""))]);
        assert!(ok(json!({"hotkeys.hide": "H"})).is_err());
        assert!(ok(json!({"hotkeys.hide": false})).is_err());
        // Never writable, unknown, empty, or one bad key spoiling the patch.
        for p in [
            json!({"port": 1}),
            json!({"overlayExe": "x"}),
            json!({"installedVersion": 9}),
            json!({"originalStatusLine": "x"}),
            json!({"startupWanted": true}),
            json!({"startupDisabled": false}),
            json!({"phone.telegram.chatId": "1"}),
            json!({"phone": {"enabled": true}}),
            json!({"remote.projects": []}),
            json!({}),
            json!([]),
            json!({"sound": true, "port": 1}),
        ] {
            assert!(ok(p.clone()).is_err(), "{p}");
        }
    }

    #[test]
    fn text_lists_and_secrets() {
        let one = |k: &str, v: Value| validate(&json!({ k: v })).map(|mut c| c.remove(0).1);
        assert_eq!(one("toasts.watchApps", json!([" Claude ", "", "claude", "Codex"])).unwrap(), json!(["Claude", "Codex"]));
        let many: Vec<String> = (0..21).map(|i| format!("app{i}")).collect();
        assert!(one("toasts.watchApps", json!(many)).is_err());
        assert!(one("toasts.watchApps", json!(["x".repeat(61)])).is_err());
        assert!(one("toasts.watchApps", json!("Claude")).is_err());
        assert_eq!(one("phone.ntfy.server", json!(" https://ntfy.example.org ")).unwrap(), json!("https://ntfy.example.org"));
        assert!(one("phone.ntfy.server", json!("ftp://x")).is_err());
        // https only (an ntfy token goes to it), and no user:password@.
        assert!(one("phone.ntfy.server", json!("http://ntfy.example.org")).is_err());
        assert!(one("phone.ntfy.server", json!("https://user:pw@ntfy.example.org")).is_err());
        assert!(one("phone.ntfy.server", json!("https://ntfy.example.org/a@b")).is_ok());
        assert!(one("phone.ntfy.server", json!("https://")).is_err());
        assert!(one("phone.ntfy.server", json!("")).is_err());
        assert_eq!(one("phone.ntfy.topic", json!("")).unwrap(), json!(""));
        assert!(one("phone.ntfy.topic", json!("a b")).is_err());
        assert!(one("phone.ntfy.topic", json!("x".repeat(65))).is_err());
        let token = format!("123456789:{}", "A".repeat(35));
        assert_eq!(one("phone.telegram.token", json!(format!(" {token} "))).unwrap(), json!(token));
        assert_eq!(one("phone.telegram.token", json!("")).unwrap(), json!(""));
        assert!(one("phone.telegram.token", json!("hunter2")).is_err());
        assert!(one("phone.ntfy.token", json!("tk_abc def")).is_err());
        assert_eq!(one("phone.ntfy.token", json!("tk_abc")).unwrap(), json!("tk_abc"));
        // The refusal never echoes the token.
        let e = one("phone.telegram.token", json!("secret-value-123")).unwrap_err();
        assert!(!e.contains("secret-value"), "{e}");
    }

    #[test]
    fn bot_ids() {
        assert_eq!(bot_id(" 123:AAA "), Some("123"));
        assert_ne!(bot_id("123:AAA"), bot_id("456:AAA"));
        assert_eq!(bot_id("123:AAA"), bot_id("123:BBB"));
        assert_eq!(bot_id(""), None);
    }

    #[test]
    fn put_creates_and_replaces_parents() {
        let mut c = json!({"phone": {"enabled": true, "telegram": {"chatId": "42"}}, "x": 1});
        put(&mut c, "phone.telegram.token", json!("t"));
        put(&mut c, "logo.style", json!("headsup"));
        put(&mut c, "sound", json!(false));
        assert_eq!(c, json!({"phone": {"enabled": true, "telegram": {"chatId": "42", "token": "t"}}, "x": 1, "logo": {"style": "headsup"}, "sound": false}));
        let mut c = json!({"limits": 5});
        put(&mut c, "limits.five_hour", json!(false));
        assert_eq!(c, json!({"limits": {"five_hour": false}}));
    }

    #[test]
    fn current_values_follow_what_the_overlay_uses() {
        let d = config::defaults();
        let c = config::deep_merge(&d, &json!({"language": "TR", "logo": {"style": "HeadsUp", "dismiss": "nope"}, "remote": {"permissionMode": "auto"}, "sound": "yes"}));
        let cur = |k: &str| {
            let &(key, kind) = SETTINGS.iter().find(|(n, _)| *n == k).unwrap();
            current(&c, &d, key, kind)
        };
        assert_eq!(cur("language"), json!("tr"));
        assert_eq!(cur("logo.style"), json!("headsup"));
        assert_eq!(cur("logo.dismiss"), json!("anyClick"));
        assert_eq!(cur("remote.permissionMode"), json!("default"));
        assert_eq!(cur("sound"), json!(true));
        assert_eq!(cur("interactive.skipWhenActive"), json!(true));
        assert_eq!(cur("toasts.watchApps"), json!(["Claude"]));
        assert_eq!(cur("phone.ntfy.server"), json!("https://ntfy.sh"));
    }
}
