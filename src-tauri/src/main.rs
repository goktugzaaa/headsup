// HeadsUp — always-on-top, click-through notification layer for the Claude desktop
// app (Chat, Cowork, Code) and Claude Code CLI. Tauri 2 / WebView2 edition.
//
// Signals:
//   1. Windows notification center (UserNotificationListener via a PowerShell bridge):
//      every toast the Claude app emits is mirrored as an overlay banner.
//   2. Claude Code hooks (hooks/headsup-send.ps1): Stop, PermissionRequest and
//      AskUserQuestion (both answerable from the banner), Notification.
//   3. Claude Code status line (rate_limits): limit-reset timers.
//
//   4. Remote sessions (remote.rs): the paired Telegram chat can start or continue Claude
//      Code sessions; the overlay runs `claude -p` headless and relays the result.
//   5. Usage limits (limits.rs, schedule.rs): a limit used up is reported at once; messages from
//      the phone wait for the reset, and sessions that stopped on it are continued then.
//
// Two windows: the overlay ("main", app/overlay.html) and, on demand, the settings window
// ("settings", app/settings.html, settings.rs). Each page's commands check which window calls.
//
// CLI: HeadsUp.exe [--install | --uninstall | --send <type>] [--from-hook]
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod config;
mod events;
mod hooks;
mod hotkey;
mod http;
mod i18n;
mod installer;
mod limits;
mod mask;
mod phone;
mod remote;
mod schedule;
mod settings;
mod telegram;
mod tray;
mod util;
mod win;

use serde::Deserialize;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicIsize, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tauri::{AppHandle, Manager, State};

/// A question / permission request whose hook is waiting for a click on the overlay.
pub struct Pending {
    /// The hook's request (signed reply; `req.pid` is the hook process, watched for exit).
    pub req: http::HookReq,
    pub kind: String,
    pub event: Value,
    /// Unix ms; the hook gets an empty reply when this passes. Typing an answer extends it.
    pub deadline_ms: i64,
    /// A question card answered through a PermissionRequest hook (the PreToolUse hook was
    /// cancelled or skipped): the answer is sent back in PermissionRequest form.
    pub via_permission: bool,
    /// Answers collected so far when a multi-question card is answered on the phone, one
    /// question at a time (question text -> answer).
    pub partial: serde_json::Map<String, Value>,
    /// The card's phone message while it waits for the PC to go idle (phone::push). Whoever
    /// sends it takes it: the deferred send, or closing the card on the PC.
    pub phone_wait: Option<phone::PhoneMsg>,
    /// The card's message went to the phone, or is on its way.
    pub on_phone: bool,
    /// Closed on the PC (its × or the hide hotkey): it never shows there again.
    pub dismissed: bool,
}

/// Clickable area of an interactive card, in CSS pixels of the overlay page.
#[derive(Deserialize, Clone, Debug)]
pub struct Rect {
    pub left: f64,
    pub top: f64,
    pub right: f64,
    pub bottom: f64,
}

pub struct Shared {
    pub port: u16,
    pub config: Mutex<Value>,
    pub lang: Mutex<&'static i18n::Strings>,
    pub lang_code: Mutex<&'static str>,
    pub state: Mutex<Value>,
    pub pending: Mutex<HashMap<String, Pending>>,
    pub seq: AtomicU64,
    pub hot_rects: Mutex<Vec<Rect>>,
    pub capturing: AtomicBool,
    pub active_items: AtomicI64,
    pub last_hook_banner_ms: AtomicI64,
    pub last_permission_ms: AtomicI64,
    pub foreground: Mutex<String>,
    pub listener_status: Mutex<String>,
    pub listener_child: Mutex<Option<std::process::Child>>,
    pub tray: Mutex<Option<tauri::tray::TrayIcon>>,
    pub quitting: AtomicBool,
    /// True while a free-text answer is being typed: the overlay then holds keyboard focus.
    pub typing: AtomicBool,
    pub prev_foreground: AtomicIsize,
    /// Telegram message_id -> pending card id, for edits and typed replies.
    pub tg_msgs: Mutex<HashMap<i64, (String, String)>>,
    pub tg_last_pending: Mutex<Option<String>>,
    /// When the newest card reached the phone (unix ms); compared with remote activity to
    /// decide whether plain text answers the card or goes to the remote session.
    pub tg_last_pending_ms: AtomicI64,
    /// Every Telegram message id that ever carried a card, open or closed.
    pub tg_card_msgs: Mutex<HashSet<i64>>,
    /// session id -> when its PreToolUse question card expired unanswered (unix ms), so the
    /// PermissionRequest that follows does not bring the same question back as a new card.
    pub expired_questions: Mutex<HashMap<String, i64>>,
    /// (card id, question index) after "Other…" was tapped on the phone: the next text
    /// message is the typed answer to that question.
    pub tg_await_text: Mutex<Option<(String, usize)>>,
    /// Force-reply prompt message id -> (card id, question index) it asked about.
    pub tg_prompt_msgs: Mutex<HashMap<i64, (String, usize)>>,
    /// Bumped per non-interactive phone message; a deferred one only goes out if still newest.
    pub phone_generation: AtomicU64,
    /// Remote sessions driven from the phone (see remote.rs).
    pub remote: Mutex<remote::RemoteState>,
    /// Session ids of remote runs: their cards go to the phone at once, their Stop is not relayed.
    pub remote_sessions: Mutex<HashSet<String>>,
    /// The page has registered its event listeners; until then events wait in `early_emits`.
    pub page_ready: AtomicBool,
    pub early_emits: Mutex<Vec<(String, Value)>>,
    /// A limit-reset logo is on screen until a click anywhere (logo.dismiss "anyClick"); the
    /// cursor poll watches the mouse buttons. Clicks before `logo_armed_at` (unix ms) do not count.
    pub logo_any_click: AtomicBool,
    pub logo_armed_at: AtomicI64,
}

impl Shared {
    pub fn t(&self) -> &'static i18n::Strings {
        *self.lang.lock().unwrap()
    }

    pub fn cfg(&self) -> Value {
        self.config.lock().unwrap().clone()
    }

    pub fn reload_config(&self) {
        let c = config::load();
        let (code, strings) = i18n::pick(config::get_str(&c, "language").as_deref(), &util::locale());
        *self.lang.lock().unwrap() = strings;
        *self.lang_code.lock().unwrap() = code;
        *self.config.lock().unwrap() = c;
        // Language or remote switch may have changed: refresh the bot's command menu.
        self.remote.lock().unwrap().commands_registered = false;
    }
}

/// The overlay page's commands do nothing for any other window (the settings window has its
/// own). Tauri lets every window of the app call every command of the app: there is no app
/// ACL manifest, so this check is the boundary.
fn from_overlay(window: &tauri::WebviewWindow) -> bool {
    window.label() == "main"
}

/// The page has registered its event listeners: deliver what arrived before.
#[tauri::command]
fn ready(window: tauri::WebviewWindow, app: AppHandle, shared: State<'_, Arc<Shared>>) {
    if from_overlay(&window) {
        events::page_ready(&app, &shared);
    }
}

#[tauri::command]
fn item_done(window: tauri::WebviewWindow, shared: State<'_, Arc<Shared>>) {
    if from_overlay(&window) && shared.active_items.fetch_sub(1, Ordering::SeqCst) <= 1 {
        shared.active_items.store(0, Ordering::SeqCst);
    }
}

#[tauri::command]
fn rlog(message: String, window: tauri::WebviewWindow) {
    if from_overlay(&window) {
        util::log_line(&["renderer", &util::truncate(&message, 500)]);
    }
}

#[tauri::command]
fn hot_rects(rects: Vec<Rect>, window: tauri::WebviewWindow, shared: State<'_, Arc<Shared>>) {
    if !from_overlay(&window) {
        return;
    }
    *shared.hot_rects.lock().unwrap() = rects
        .into_iter()
        .filter(|r| r.right > r.left && r.bottom > r.top)
        .collect();
}

#[tauri::command]
fn answer(id: String, data: Value, window: tauri::WebviewWindow, app: AppHandle, shared: State<'_, Arc<Shared>>) {
    if !from_overlay(&window) {
        return;
    }
    // A card with HeadsUp's own buttons (continue at a usage-limit reset), not a hook's.
    match id.strip_prefix("offer-") {
        Some(item) => schedule::answer_card(&app, &shared, item, data.get("accept").and_then(Value::as_bool) == Some(true)),
        None => events::finish_interactive(&app, &shared, &id, Some(data)),
    }
}

/// The user closed a card with its ×: it leaves the PC for good (see events::dismiss_card).
#[tauri::command]
fn dismiss_card(id: String, window: tauri::WebviewWindow, app: AppHandle, shared: State<'_, Arc<Shared>>) {
    if from_overlay(&window) {
        events::dismiss_card(&app, &shared, &id);
    }
}

/// Keep a pending card alive longer (the user opened the free-text field).
#[tauri::command]
fn extend_wait(id: String, ms: u64, window: tauri::WebviewWindow, shared: State<'_, Arc<Shared>>) {
    if !from_overlay(&window) {
        return;
    }
    if let Some(p) = shared.pending.lock().unwrap().get_mut(&id) {
        p.deadline_ms = p.deadline_ms.max(util::now_ms() + ms as i64);
    }
}

/// Take keyboard focus for the free-text field (true) and hand it back to the window that
/// had it (false). Only while typing does the overlay ever become the active window.
#[tauri::command]
fn focus_input(active: bool, window: tauri::WebviewWindow, app: AppHandle, shared: State<'_, Arc<Shared>>) {
    if !from_overlay(&window) {
        return;
    }
    let Some(w) = events::window(&app) else { return };
    if active {
        if !shared.typing.swap(true, Ordering::SeqCst) {
            shared.prev_foreground.store(win::foreground_hwnd(), Ordering::SeqCst);
        }
        win::allow_activate(&w);
        let _ = w.set_focus();
    } else if shared.typing.swap(false, Ordering::SeqCst) {
        win::ensure_noactivate(&w);
        win::set_foreground_raw(shared.prev_foreground.swap(0, Ordering::SeqCst));
    }
}

/// The page shows a limit-reset logo that waits for a click (`visible`), or no longer does.
/// `any_click`: a click anywhere on the screen dismisses it, so the mouse buttons are watched.
#[tauri::command]
fn logo_state(visible: bool, any_click: bool, window: tauri::WebviewWindow, shared: State<'_, Arc<Shared>>) {
    if from_overlay(&window) {
        events::watch_logo_clicks(&shared, visible && any_click);
    }
}

// The settings window's commands. Async: they run off the main thread, since some write files,
// start PowerShell (Startup shortcut) or reach the network (phone test).

/// The settings the page shows, and what is derived from elsewhere (never a token).
#[tauri::command]
async fn settings_get(window: tauri::WebviewWindow, shared: State<'_, Arc<Shared>>) -> Result<Value, String> {
    settings::check_window(&window)?;
    Ok(settings::get(&shared))
}

/// Change settings (`patch`: flat keys, `{"notification.durationMs": 6000}`); returns the new state.
#[tauri::command]
async fn settings_set(
    patch: Value,
    window: tauri::WebviewWindow,
    app: AppHandle,
    shared: State<'_, Arc<Shared>>,
) -> Result<Value, String> {
    settings::check_window(&window)?;
    settings::set(&app, &shared, &patch)?;
    Ok(settings::get(&shared))
}

/// A button (test banner, pairing code, Codex, Startup, open file...); returns the new state.
#[tauri::command]
async fn settings_action(
    name: String,
    window: tauri::WebviewWindow,
    app: AppHandle,
    shared: State<'_, Arc<Shared>>,
) -> Result<Value, String> {
    settings::check_window(&window)?;
    settings::action(&app, &shared, &name)?;
    Ok(settings::get(&shared))
}

/// Our app (HeadsUp, or Claude Overlay 2.x) answers the unauthenticated health check on the port.
fn overlay_answers(port: u16) -> bool {
    http::health(port, 1500)
        .and_then(|b| serde_json::from_str::<Value>(&b).ok())
        .map_or(false, |h| h.get("engine").and_then(Value::as_str) == Some("tauri"))
}

/// A copy answers on the port but not as this install (an older version that does not
/// sign, or one using another key).
fn older_overlay_running(port: u16) -> bool {
    overlay_answers(port) && http::post(port, "reload-config", "{}", 1500).is_err()
}

/// The port from the environment override, else from `cfg`, else the default.
fn configured_port(cfg: &Value) -> u16 {
    util::env_setting("PORT")
        .and_then(|p| p.parse().ok())
        .unwrap_or(config::get_u64(cfg, "port", 47821) as u16)
}

/// First start after an upgrade from Claude Overlay 2.x: move its data folder (settings,
/// Telegram pairing, key) to the new one. Not while that version still runs: its hooks would
/// lose their scripts and config mid-session, so it is asked to quit first. false: stop here.
fn migrate_from_2x(from_hook: bool) -> bool {
    let no_install = util::env_setting("NO_INSTALL").is_some();
    if !util::legacy_data_pending() {
        // An earlier folder stays behind (its settings were copied) or the old exe recreated
        // it: keep the hooks of sessions opened before the upgrade from starting that exe.
        if !no_install {
            util::disarm_legacy_data();
        }
        return true;
    }
    let notice = |text: &str| {
        if !from_hook {
            win::message_box("HeadsUp", text);
        }
    };
    if no_install {
        // A test run must neither take the data away from the installed 2.x nor create the
        // new folder (which would keep the real upgrade from moving the old one).
        notice("HEADSUP_NO_INSTALL (or CLAUDE_OVERLAY_NO_INSTALL) is set, but the data of Claude Overlay 2.x has not been copied to %USERPROFILE%\\.headsup yet, so this test run stops here.\n\nStart HeadsUp once without it to upgrade.");
        return false;
    }
    // 2.x's hooks (still loaded in open Claude Code sessions) start the old exe whenever the
    // port does not answer, unless their spawn stamp is under 10 s old. It is kept fresh from
    // here until the port is ours (bound right after this returns).
    if util::hold_legacy_spawns() {
        // A hook started 2.x a moment ago: let it come up, so it is seen (and asked to quit)
        // below instead of taking the port after the move.
        std::thread::sleep(std::time::Duration::from_millis(3000));
        util::hold_legacy_spawns();
    }
    // Only the health check and an unsigned quit: a signed request would create a new key,
    // and with it the new folder, before the old one is moved. 2.x listens on the port from
    // its own config.
    let old_cfg = util::legacy_data_source()
        .and_then(|d| util::read_json(&d.join("config.json")))
        .unwrap_or(Value::Null);
    let old_port = config::get_u64(&old_cfg, "port", 47821) as u16;
    let port = configured_port(&old_cfg);
    let ports: Vec<u16> = if port == old_port { vec![port] } else { vec![old_port, port] };
    let running = || ports.iter().any(|&p| overlay_answers(p));
    if running() {
        for &p in &ports {
            http::quit_unsigned(p);
        }
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            util::hold_legacy_spawns();
            std::thread::sleep(std::time::Duration::from_millis(300));
            if !running() {
                break;
            }
            if std::time::Instant::now() > deadline {
                notice("Claude Overlay (the previous version), or a copy of HeadsUp from another Windows account, is still running and did not quit when asked.\n\nQuit it from its tray icon, then start HeadsUp again. Claude Code sessions that are still open start the old version again through their hooks: if it keeps coming back, close them first. Your settings and Telegram pairing move over on that start.");
                return false;
            }
        }
        // It stopped answering: give it a moment to finish exiting.
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
    util::hold_legacy_spawns();
    if let Err(e) = util::migrate_legacy_data() {
        notice(&format!(
            "The settings of Claude Overlay (the previous version) could not be copied to %USERPROFILE%\\.headsup: {e}.\n\nNothing was changed. Close any program that has its config.json open, then start HeadsUp again."
        ));
        return false;
    }
    util::retire_legacy_exe(&old_cfg);
    util::hold_legacy_spawns();
    true
}

fn clear_markers() {
    let _ = std::fs::remove_file(util::marker_path(util::MARKER_STOPPED));
    let _ = std::fs::remove_file(util::marker_path(util::MARKER_UNINSTALLED));
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    // A hook starts the overlay from the project folder: never keep that folder open (it
    // could not be deleted or renamed while the overlay runs).
    if let Some(dir) = std::env::current_exe().ok().and_then(|p| p.parent().map(std::path::Path::to_path_buf)) {
        let _ = std::env::set_current_dir(dir);
    }
    // Started by a hook: not after the user quit from the tray or removed the hooks.
    let from_hook = args.iter().any(|a| a == "--from-hook");
    // Before anything reads config, state, the key or a marker.
    if !migrate_from_2x(from_hook) {
        return;
    }
    // A panic (e.g. inside Tauri's setup, when WebView2 is missing) aborts the process
    // (panic = "abort") and release builds have no console: log it, and tell the user, or for
    // a hook start leave the marker that stops hooks from retrying for a while.
    std::panic::set_hook(Box::new(move |info| {
        util::log_line(&["fatal", &info.to_string()]);
        if from_hook {
            let _ = std::fs::write(std::env::temp_dir().join("headsup-spawn.failed"), "");
        } else {
            win::message_box("HeadsUp stopped because of an error", &info.to_string());
        }
    }));
    if from_hook
        && (util::marker_path(util::MARKER_UNINSTALLED).exists() || util::marker_path(util::MARKER_STOPPED).exists())
    {
        return;
    }
    config::ensure_user_data();
    let cfg = config::load();
    let port = configured_port(&cfg);

    let installing = args.iter().any(|a| a == "--install" || a == "--install-codex");
    if installing && older_overlay_running(port) {
        // Our scripts only accept signed replies, which that copy cannot give: installing now
        // would break every card answer until it is quit.
        win::message_box(
            "HeadsUp",
            "Another copy of HeadsUp or Claude Overlay (an older version, or one from another Windows account) is running. Nothing was installed.\n\nQuit it from its tray icon, then run this command again.",
        );
        return;
    }
    if args.iter().any(|a| a == "--install") {
        clear_markers();
        let text = match installer::install() {
            Ok(lines) => lines.join("\n"),
            Err(e) => format!("Error: {e}"),
        };
        let _ = http::post(port, "reload-config", "{}", 1500);
        let _ = http::post(port, "test-notify", "{}", 1500);
        win::message_box("HeadsUp installed", &text);
        return;
    }
    if args.iter().any(|a| a == "--install-codex") {
        let text = match installer::install_codex() { Ok(m) => m, Err(e) => format!("Error: {e}") };
        win::message_box("Codex CLI integration", &text);
        return;
    }
    if args.iter().any(|a| a == "--uninstall-codex") {
        win::message_box("Codex CLI integration", &installer::uninstall_codex());
        return;
    }
    if args.iter().any(|a| a == "--uninstall") {
        let lines = installer::uninstall();
        let _ = http::post(port, "quit", "{}", 1500);
        win::message_box("HeadsUp removed", &lines.join("\n"));
        return;
    }
    if let Some(i) = args.iter().position(|a| a == "--send") {
        let kind = args.get(i + 1).cloned().unwrap_or_else(|| "test-notify".to_string());
        // A card holds the request until it is answered or expires.
        let interactive = matches!(kind.as_str(), "test-question" | "test-permission" | "question" | "permission");
        let timeout_ms = if interactive { 900_000 } else { 2500 };
        if let Err(e) = http::post(port, &kind, "{}", timeout_ms) {
            win::message_box("HeadsUp did not respond", &e);
        }
        return;
    }

    if !from_hook {
        // Started by hand or at logon: the user wants it running (and hooked up) again.
        clear_markers();
    }

    // Binding the port doubles as the single-instance lock: a second copy started by hand
    // opens the running one's Settings window and exits (a copy started by a hook exits quietly).
    let server = match tiny_http::Server::http(("127.0.0.1", port)) {
        Ok(s) => s,
        Err(e) => {
            if from_hook || http::post(port, "open-settings", "{}", 1500).is_ok() {
                return;
            }
            // Something else holds the port: say so instead of exiting silently.
            let text = if overlay_answers(port) {
                format!(
                    "Another copy of HeadsUp or Claude Overlay (an older version, or one from another Windows account) is already running on port {port}.\n\nQuit it from its tray icon, then start this one again."
                )
            } else {
                format!(
                    "Port {port} on 127.0.0.1 is in use by another program ({e}).\n\nSet \"port\" in %USERPROFILE%\\.headsup\\config.json to a free port."
                )
            };
            util::log_line(&["startup-error", &format!("port {port}: {e}")]);
            win::message_box("HeadsUp could not start", &text);
            return;
        }
    };
    // Scripts and key are written only by the copy that owns the port, so the hooks on disk
    // always match the overlay that answers them.
    hooks::write_scripts();
    let _ = util::secret();
    if win::is_elevated() {
        util::log_line(&["startup", "elevated"]);
    }

    let (code, strings) = i18n::pick(config::get_str(&cfg, "language").as_deref(), &util::locale());
    let shared = Arc::new(Shared {
        port,
        config: Mutex::new(cfg),
        lang: Mutex::new(strings),
        lang_code: Mutex::new(code),
        state: Mutex::new(events::load_state()),
        pending: Mutex::new(HashMap::new()),
        // Card ids are part of Telegram button data: start from the clock so a button left over
        // from before a restart can never match a new card.
        seq: AtomicU64::new(util::now_ms().max(0) as u64),
        hot_rects: Mutex::new(Vec::new()),
        capturing: AtomicBool::new(false),
        active_items: AtomicI64::new(0),
        last_hook_banner_ms: AtomicI64::new(0),
        last_permission_ms: AtomicI64::new(0),
        foreground: Mutex::new(String::new()),
        listener_status: Mutex::new("starting".to_string()),
        listener_child: Mutex::new(None),
        tray: Mutex::new(None),
        quitting: AtomicBool::new(false),
        typing: AtomicBool::new(false),
        prev_foreground: AtomicIsize::new(0),
        tg_msgs: Mutex::new(HashMap::new()),
        tg_last_pending: Mutex::new(None),
        tg_last_pending_ms: AtomicI64::new(0),
        tg_card_msgs: Mutex::new(HashSet::new()),
        expired_questions: Mutex::new(HashMap::new()),
        tg_await_text: Mutex::new(None),
        tg_prompt_msgs: Mutex::new(HashMap::new()),
        phone_generation: AtomicU64::new(0),
        remote: Mutex::new(remote::RemoteState::new()),
        remote_sessions: Mutex::new(HashSet::new()),
        page_ready: AtomicBool::new(false),
        early_emits: Mutex::new(Vec::new()),
        logo_any_click: AtomicBool::new(false),
        logo_armed_at: AtomicI64::new(0),
    });

    let setup_shared = shared.clone();
    let result = tauri::Builder::default()
        .manage(shared)
        .invoke_handler(tauri::generate_handler![
            ready,
            item_done,
            rlog,
            hot_rects,
            answer,
            dismiss_card,
            extend_wait,
            focus_input,
            logo_state,
            settings_get,
            settings_set,
            settings_action
        ])
        .setup(move |app| {
            let handle = app.handle().clone();
            let window = handle.get_webview_window("main").ok_or("main window missing")?;
            win::ensure_noactivate(&window);
            events::place_on_cursor_monitor(&handle, true);
            let _ = window.set_ignore_cursor_events(true);
            let _ = window.set_always_on_top(true);
            // The window stays visible (fully transparent) while idle: hiding and re-showing
            // a transparent window on Windows stops clicks from reaching the page.
            let _ = window.show();
            win::ensure_noactivate(&window);
            {
                let w = window.clone();
                std::thread::spawn(move || {
                    std::thread::sleep(std::time::Duration::from_millis(800));
                    win::ensure_noactivate(&w);
                });
            }
            http::serve(server, handle.clone(), setup_shared.clone());
            {
                // Should the page never report ready, stop holding events back after a while.
                let (app, shared) = (handle.clone(), setup_shared.clone());
                std::thread::spawn(move || {
                    std::thread::sleep(std::time::Duration::from_secs(10));
                    events::page_ready(&app, &shared);
                });
            }
            tray::build(&handle, &setup_shared)?;
            events::start_limit_timer(handle.clone(), setup_shared.clone());
            schedule::start(handle.clone(), setup_shared.clone());
            events::start_toast_listener(handle.clone(), setup_shared.clone());
            events::start_cursor_poll(handle.clone(), setup_shared.clone());
            telegram::start(handle.clone(), setup_shared.clone());
            hotkey::start(handle.clone(), setup_shared.clone());
            events::first_run(handle, setup_shared);
            Ok(())
        })
        .run(tauri::generate_context!());
    if let Err(e) = result {
        // Release builds have no console: a failed start must not be silent.
        util::log_line(&["fatal", &e.to_string()]);
        win::message_box("HeadsUp could not start", &e.to_string());
    }
}
