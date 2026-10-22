//! Everything that turns an incoming signal into something on screen: hook events,
//! interactive cards (question / permission), mirrored toasts, limit-reset timers and the
//! cursor polling that makes cards clickable.
use crate::http::HookReq;
use crate::phone::{self, PhoneMsg};
use crate::{config, i18n, installer, mask, util, Pending, Shared};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader};
use std::process::Stdio;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;
use tauri::{AppHandle, Emitter, Manager};

pub const WINDOW_HEIGHT_LOGICAL: f64 = 640.0;

/// Open interactive cards at most; more requests get `{}` (Claude Code shows its own dialog).
const MAX_PENDING: usize = 16;
/// Permission card text limits (C5): the PC card scrolls, the phone message must stay short.
pub const CARD_TEXT_PC: usize = 1200;
pub const CARD_TEXT_PHONE: usize = 900;

fn num_i64(v: &Value) -> Option<i64> {
    v.as_i64().or_else(|| v.as_f64().map(|f| f as i64))
}

pub fn window(app: &AppHandle) -> Option<tauri::WebviewWindow> {
    app.get_webview_window("main")
}

// ---------------------------------------------------------------- window placement

/// Keep the overlay strip at the top of the monitor the cursor is on.
pub fn place_on_cursor_monitor(app: &AppHandle, force: bool) {
    let Some(win) = window(app) else { return };
    let monitors = app.available_monitors().unwrap_or_default();
    let cursor = app.cursor_position().ok();
    let found = cursor.and_then(|c| {
        monitors
            .iter()
            .find(|m| {
                let p = m.position();
                let s = m.size();
                c.x >= p.x as f64
                    && c.x < p.x as f64 + s.width as f64
                    && c.y >= p.y as f64
                    && c.y < p.y as f64 + s.height as f64
            })
            .cloned()
    });
    let mon = found
        .or_else(|| app.primary_monitor().ok().flatten())
        .or_else(|| monitors.first().cloned());
    let Some(mon) = mon else { return };
    let p = *mon.position();
    let s = *mon.size();
    let height = (WINDOW_HEIGHT_LOGICAL * mon.scale_factor()).round() as u32;
    let same_pos = win.outer_position().map(|c| c.x == p.x && c.y == p.y).unwrap_or(false);
    let same_size = win
        .outer_size()
        .map(|c| c.width == s.width && c.height == height)
        .unwrap_or(false);
    if force || !same_pos || !same_size {
        let _ = win.set_position(tauri::PhysicalPosition::new(p.x, p.y));
        let _ = win.set_size(tauri::PhysicalSize::new(s.width, height));
    }
}

pub fn show_overlay(app: &AppHandle, shared: &Shared) {
    place_on_cursor_monitor(app, false);
    if let Some(w) = window(app) {
        // Whoever set TOPMOST last wins, so re-assert it on every event: tao's flag, and the
        // z-order itself (tao skips the call while its flag is already set).
        // Above other always-on-top windows too (a borderless game or player that set TOPMOST
        // later): the overlay is transparent and click-through, so this covers nothing.
        let _ = w.set_always_on_top(true);
        let buried = crate::win::is_buried(&w);
        if buried {
            util::log_line(&["zorder-restored", "event"]);
        }
        crate::win::raise_topmost(&w, buried);
        if !shared.typing.load(Ordering::SeqCst) {
            crate::win::ensure_noactivate(&w);
        }
    }
}

// ---------------------------------------------------------------- banners

pub fn emit(app: &AppHandle, shared: &Shared, channel: &str, payload: Value) {
    shared.active_items.fetch_add(1, Ordering::SeqCst);
    show_overlay(app, shared);
    emit_page(app, shared, channel, payload);
}

/// Send an event to the page. Tauri drops events nobody listens to yet, so until the page
/// says it is ready (a hook may start the overlay and post a card at once) they are queued.
fn emit_page(app: &AppHandle, shared: &Shared, channel: &str, payload: Value) {
    {
        let mut queue = shared.early_emits.lock().unwrap();
        if !shared.page_ready.load(Ordering::SeqCst) {
            queue.push((channel.to_string(), payload));
            return;
        }
    }
    let _ = app.emit(channel, payload);
}

/// The page has registered its listeners (or the fallback timer gave up waiting): deliver
/// everything that arrived before, in order, then the look to use. The look is sent on every
/// call: a page that was reloaded starts with the default one.
pub fn page_ready(app: &AppHandle, shared: &Shared) {
    let style = style_payload(&shared.cfg());
    let mut queue = shared.early_emits.lock().unwrap();
    if !shared.page_ready.swap(true, Ordering::SeqCst) {
        for (channel, payload) in queue.drain(..) {
            let _ = app.emit(&channel, payload);
        }
    }
    let _ = app.emit("style", style);
}

/// The page's look (accent colour) follows logo.style. Which logo each banner shows is decided
/// here, per banner (`icon`); the page never sees the config.
fn style_payload(cfg: &Value) -> Value {
    json!({"style": config::logo_style(cfg)})
}

/// Send the look again after the config was reloaded.
fn send_style(app: &AppHandle, shared: &Shared) {
    emit_page(app, shared, "style", style_payload(&shared.cfg()));
}

/// The logo a banner shows: Claude's for news from Claude (Claude Code hooks, the Claude app's
/// toasts, limit resets) while logo.style is "claude"; otherwise, and for HeadsUp's own
/// messages, Codex and other apps' toasts, the HeadsUp mark.
fn icon(cfg: &Value, from_claude: bool) -> &'static str {
    if from_claude && config::logo_style(cfg) == "claude" {
        "claude"
    } else {
        "headsup"
    }
}

/// Hook events come from Claude Code unless an adapter for another agent says otherwise (the
/// Codex adapter sends "agent": "codex").
fn from_claude_code(event: &Value) -> bool {
    event
        .get("agent")
        .and_then(Value::as_str)
        .is_none_or(|a| a.trim().is_empty() || a.trim().eq_ignore_ascii_case("claude"))
}

/// A toast from the Claude desktop app: its display name, or its app id (the Store package
/// "Claude_…", or the older installer's "…AnthropicClaude…").
fn claude_app(name: &str, aumid: &str) -> bool {
    let id = aumid.trim().to_ascii_lowercase();
    name.trim().eq_ignore_ascii_case("claude") || id.starts_with("claude_") || id.contains("anthropic")
}

/// A banner from HeadsUp itself or from anything other than Claude: it shows the HeadsUp mark.
pub fn notify(
    app: &AppHandle,
    shared: &Shared,
    title: &str,
    subtitle: &str,
    body: &str,
    kind: &str,
    duration_ms: Option<u64>,
) {
    banner(app, shared, false, title, subtitle, body, kind, duration_ms);
}

/// A banner; `from_claude`: it reports something from Claude (see `icon`).
#[allow(clippy::too_many_arguments)]
fn banner(
    app: &AppHandle,
    shared: &Shared,
    from_claude: bool,
    title: &str,
    subtitle: &str,
    body: &str,
    kind: &str,
    duration_ms: Option<u64>,
) {
    let cfg = shared.cfg();
    let t = shared.t();
    let dur = duration_ms.unwrap_or_else(|| config::get_u64(&cfg, "notification.durationMs", 6000));
    let title = if title.is_empty() { t.app_name } else { title };
    emit(
        app,
        shared,
        "notify",
        json!({
            "title": title,
            "subtitle": subtitle,
            "body": body,
            "kind": kind,
            "icon": icon(&cfg, from_claude),
            "nowLabel": t.now,
            "closeLabel": close_label(shared),
            "durationMs": dur,
            "sound": config::get_bool(&cfg, "sound", true),
        }),
    );
}

/// The limit-reset logo. With logo.dismiss "anyClick" or "logoClick" it stays until clicked
/// (the page reports it with `logo_state`); with "auto" it leaves after logo.durationMs.
pub fn show_logo(app: &AppHandle, shared: &Shared, caption: &str) {
    let cfg = shared.cfg();
    let t = shared.t();
    let dismiss = config::logo_dismiss(&cfg);
    emit(
        app,
        shared,
        "logo",
        json!({
            "caption": caption,
            // A limit reset is news from Claude.
            "icon": icon(&cfg, true),
            "dismiss": dismiss,
            "hint": match dismiss {
                "anyClick" => t.logo_hint_any,
                "logoClick" => t.logo_hint_logo,
                _ => "",
            },
            "durationMs": config::get_u64(&cfg, "logo.durationMs", 9000),
            "sound": config::get_bool(&cfg, "sound", true),
            "closeLabel": close_label(shared),
        }),
    );
}

/// Tooltip of the × on banners, cards and the logo; it names the hide hotkey while that works.
fn close_label(shared: &Shared) -> String {
    let t = shared.t();
    match crate::hotkey::active() {
        Some(key) => format!("{} · {}", t.close, t.hide_all_key.replace("{key}", &key)),
        None => t.close.to_string(),
    }
}

/// A click anywhere must not dismiss the logo in its first moments: it may be one that was
/// already under way when the logo appeared.
const LOGO_CLICK_GUARD_MS: i64 = 1500;

/// The page shows a limit-reset logo that waits for a click anywhere (`on`), or no longer
/// does. Called again when a second reset updates the logo: the guard time starts over.
pub fn watch_logo_clicks(shared: &Shared, on: bool) {
    if on {
        shared.logo_armed_at.store(util::now_ms() + LOGO_CLICK_GUARD_MS, Ordering::SeqCst);
    }
    shared.logo_any_click.store(on, Ordering::SeqCst);
}

fn with_project(label: &str, cwd: Option<&Value>) -> String {
    let p = util::project_name(cwd);
    if p.is_empty() {
        label.to_string()
    } else {
        format!("{label} · {p}")
    }
}

/// Invisible and bidi control characters that could hide or reorder what a card shows
/// (Trojan Source), plus control characters other than newline and tab.
fn hidden_char(c: char) -> bool {
    matches!(
        c as u32,
        0x00AD | 0x061C | 0x180E | 0x200B..=0x200F | 0x202A..=0x202E | 0x2060..=0x2064 | 0x2066..=0x2069 | 0xFEFF | 0x2028 | 0x2029
    ) || (c.is_control() && c != '\n' && c != '\t')
}

/// Make hidden characters visible as ⟦U+XXXX⟧ (a "\r\n" pair stays a plain line break).
pub fn reveal(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.replace("\r\n", "\n").chars() {
        if hidden_char(c) {
            out.push_str(&format!("⟦U+{:04X}⟧", c as u32));
        } else {
            out.push(c);
        }
    }
    out
}

/// Cut `s` to `limit` chars; a cut is never silent.
pub fn cut(s: &str, limit: usize) -> String {
    let total = s.chars().count();
    if total <= limit {
        return s.to_string();
    }
    let mut out: String = s.chars().take(limit).collect();
    out.push_str(&format!("… (+{} more chars)", total - limit));
    out
}

/// What a permission card asks to run, in full (C5): line breaks kept, hidden characters
/// made visible, Write/Edit content previewed, every field of MCP and unknown tools listed,
/// cut to `limit` chars with a visible marker. The PC card and the phone both use this.
pub fn permission_text(name: &str, input: Option<&Value>, limit: usize) -> String {
    let obj = input.and_then(Value::as_object);
    let s = |k: &str| obj.and_then(|o| o.get(k)).and_then(Value::as_str).unwrap_or("");
    let first = |keys: &[&str]| keys.iter().map(|k| s(k)).find(|v| !v.is_empty()).unwrap_or("");
    // One field per line: a value's own line breaks must not look like another field.
    let one_line = |v: &str| v.replace("\r\n", "\n").replace('\n', "⏎");
    let text = match name {
        "Bash" | "PowerShell" => format!("{name}: {}", first(&["command", "description"])),
        "Write" => {
            let content = s("content");
            format!("Write: {} ({} chars)\n{}", first(&["file_path", "path"]), content.chars().count(), cut(content, 300))
        }
        "Edit" => {
            let all = obj.and_then(|o| o.get("replace_all")).and_then(Value::as_bool) == Some(true);
            format!(
                "Edit: {}{}\n− {}\n+ {}",
                s("file_path"),
                if all { " (all occurrences)" } else { "" },
                cut(s("old_string"), 200),
                cut(s("new_string"), 200)
            )
        }
        "MultiEdit" => {
            let edits = obj.and_then(|o| o.get("edits")).and_then(Value::as_array);
            let n = edits.map_or(0, Vec::len);
            let mut t = format!("MultiEdit: {} ({n} edits)", s("file_path"));
            for e in edits.into_iter().flatten().take(3) {
                let es = |k: &str| e.get(k).and_then(Value::as_str).unwrap_or("");
                t.push_str(&format!("\n− {}\n+ {}", cut(es("old_string"), 120), cut(es("new_string"), 120)));
            }
            if n > 3 {
                t.push_str(&format!("\n… (+{} more edits)", n - 3));
            }
            t
        }
        "NotebookEdit" => {
            let mode = s("edit_mode");
            let mut t = format!("NotebookEdit: {}", s("notebook_path"));
            if !mode.is_empty() {
                t.push_str(&format!(" ({mode})"));
            }
            let src = s("new_source");
            if !src.is_empty() {
                t.push('\n');
                t.push_str(&cut(src, 300));
            }
            t
        }
        "Read" => format!("Read: {}", first(&["file_path", "notebook_path"])),
        "WebFetch" => format!("WebFetch: {}", s("url")),
        "WebSearch" => format!("WebSearch: {}", s("query")),
        "Agent" | "Task" => {
            let prompt = s("prompt");
            let mut t = format!("{name}: {}", s("description"));
            if !prompt.is_empty() {
                t.push('\n');
                t.push_str(&cut(prompt, 300));
            }
            t
        }
        "Glob" | "Grep" => {
            let path = s("path");
            if path.is_empty() {
                format!("{name}: {}", s("pattern"))
            } else {
                format!("{name}: {} · {path}", s("pattern"))
            }
        }
        _ => {
            let mut t = if name.is_empty() { "tool".to_string() } else { name.to_string() };
            for (k, v) in obj.into_iter().flatten() {
                let value = match v {
                    Value::String(x) => x.clone(),
                    other => other.to_string(),
                };
                t.push_str(&format!("\n{}: {}", cut(&one_line(k), 60), cut(&one_line(&value), 200)));
            }
            t
        }
    };
    cut(&reveal(&text), limit)
}

/// Human-readable one-liner for a tool call (logs and remote progress lines).
pub fn describe_tool(name: &str, input: Option<&Value>) -> String {
    let obj = input.and_then(Value::as_object);
    // Masked before cutting, so a secret at the cut is never left partly visible.
    let short = |s: &str| util::truncate(&mask::secrets(&s.split_whitespace().collect::<Vec<_>>().join(" ")), 160);
    let field = |keys: &[&str]| -> String {
        keys.iter()
            .find_map(|k| obj.and_then(|o| o.get(*k)).and_then(Value::as_str).map(short))
            .unwrap_or_default()
    };
    match name {
        "Bash" | "PowerShell" => format!("{name}: {}", field(&["command", "description"])),
        "Edit" | "Write" | "Read" | "NotebookEdit" | "MultiEdit" => {
            format!("{name}: {}", field(&["file_path", "notebook_path"]))
        }
        "WebFetch" => format!("WebFetch: {}", field(&["url"])),
        "WebSearch" => format!("WebSearch: {}", field(&["query"])),
        "Agent" | "Task" => format!("{name}: {}", field(&["description", "prompt"])),
        "Glob" | "Grep" => format!("{name}: {}", field(&["pattern"])),
        _ => {
            let name = if name.is_empty() { "tool" } else { name };
            let first_str = obj.and_then(|o| o.values().find_map(|v| v.as_str().map(short)));
            match first_str {
                Some(s) => format!("{name}: {s}"),
                None => match obj {
                    Some(o) if !o.is_empty() => {
                        format!("{name}: {}", util::truncate(&mask::secrets(&Value::Object(o.clone()).to_string()), 160))
                    }
                    _ => name.to_string(),
                },
            }
        }
    }
}

// ---------------------------------------------------------------- requests from hooks

pub fn handle_request(app: &AppHandle, shared: &Arc<Shared>, kind: &str, body: Value, req: HookReq) {
    match kind {
        "question" | "permission" => {
            let tool = body.get("tool_name").and_then(Value::as_str).unwrap_or("");
            util::log_line(&[kind, tool, &util::project_name(body.get("cwd"))]);
            if kind == "permission" && tool == "AskUserQuestion" {
                // Claude Code asks permission to show its own question picker. That happens when
                // the PreToolUse hook did not answer (it was cancelled by its timeout, or skipped).
                // Answering this request with allow + updatedInput.answers answers the question,
                // so the open card for the same session moves over to this request; with no open
                // card, a new question card is shown that answers through this request.
                let sid = body.get("session_id").and_then(Value::as_str).map(String::from);
                let adopt = {
                    let pending = shared.pending.lock().unwrap();
                    pending
                        .iter()
                        .find(|(_, p)| {
                            p.kind == "question"
                                && !p.via_permission
                                && p.event.get("session_id").and_then(Value::as_str) == sid.as_deref()
                                && p.event.pointer("/tool_input/questions") == body.pointer("/tool_input/questions")
                        })
                        .map(|(id, _)| id.clone())
                };
                if let Some(id) = adopt {
                    let old = {
                        let mut pending = shared.pending.lock().unwrap();
                        match pending.get_mut(&id) {
                            Some(p) => {
                                p.via_permission = true;
                                p.deadline_ms = p.deadline_ms.max(util::now_ms() + 60_000);
                                Some(std::mem::replace(&mut p.req, req))
                            }
                            None => {
                                drop(pending);
                                start_interactive(app, shared, "question", body, req, true, false);
                                return;
                            }
                        }
                    };
                    util::log_line(&["question-adopted", &id]);
                    if let Some(old) = old {
                        crate::http::respond_json(old, "{}".to_string());
                    }
                    return;
                }
                let just_expired = sid
                    .as_deref()
                    .and_then(|s| shared.expired_questions.lock().unwrap().get(s).copied())
                    .map(|at| util::now_ms() - at < 60_000)
                    .unwrap_or(false);
                if just_expired {
                    // Nobody answered the card in time: let Claude Code's own picker take over.
                    crate::http::respond_json(req, "{}".to_string());
                    return;
                }
                start_interactive(app, shared, "question", body, req, true, false);
                return;
            }
            start_interactive(app, shared, kind, body, req, false, false);
        }
        "test-question" => start_interactive(
            app,
            shared,
            "question",
            json!({
                "cwd": "C:/demo/project",
                "tool_name": "AskUserQuestion",
                "tool_input": {"questions": [{
                    "question": "Which database should we use?",
                    "header": "Database",
                    "options": [
                        {"label": "Postgres", "description": "Managed, scalable"},
                        {"label": "SQLite", "description": "Single file, zero setup"},
                        {"label": "MySQL", "description": ""}
                    ]
                }]}
            }),
            req,
            false,
            true,
        ),
        "test-permission" => start_interactive(
            app,
            shared,
            "permission",
            json!({"cwd": "C:/demo/project", "tool_name": "Bash", "tool_input": {"command": "npm install && npm run build"}}),
            req,
            false,
            true,
        ),
        _ => {
            handle_event(app, shared, kind, &body);
            crate::http::respond_json(req, "{\"ok\":true}".to_string());
        }
    }
}

pub fn handle_event(app: &AppHandle, shared: &Arc<Shared>, kind: &str, event: &Value) {
    let cfg = shared.cfg();
    let t = shared.t();
    if kind == "status" {
        // The status line posts on every refresh: log only when the limits change.
        static LAST: std::sync::Mutex<String> = std::sync::Mutex::new(String::new());
        let summary = summarize_limits(event.get("status"));
        let mut last = LAST.lock().unwrap_or_else(|e| e.into_inner());
        if *last != summary {
            util::log_line(&["status", &summary]);
            *last = summary;
        }
    } else {
        util::log_line(&[
            kind,
            event
                .get("notification_type")
                .or_else(|| event.get("tool_name"))
                .and_then(Value::as_str)
                .unwrap_or(""),
            &util::project_name(event.get("cwd")),
        ]);
    }
    match kind {
        "stop" => {
            if !config::get_bool(&cfg, "events.stop", true)
                || event.get("stop_hook_active").and_then(Value::as_bool).unwrap_or(false)
            {
                return;
            }
            shared.last_hook_banner_ms.store(util::now_ms(), Ordering::SeqCst);
            let full = if config::get_bool(&cfg, "notification.showLastMessage", true) {
                util::clean_message(event.get("last_assistant_message").and_then(Value::as_str).unwrap_or(""))
            } else {
                String::new()
            };
            let body = util::truncate(&full, 180);
            // Adapters for other agents (Codex) send their own title.
            let title = event.get("title").and_then(Value::as_str).unwrap_or(t.code_name);
            let subtitle = with_project(t.task_done, event.get("cwd"));
            banner(app, shared, from_claude_code(event), title, &subtitle, &body, "done", None);
            let project = util::project_name(event.get("cwd"));
            if crate::remote::is_remote_session(shared, event.get("session_id").and_then(Value::as_str)) {
                return; // the remote runner reports this run's result itself
            }
            // Masked before cutting: a secret that straddles the cut is still recognised whole.
            let (title, body) = (mask::secrets(title), util::truncate(&mask::secrets(&full), 180));
            phone::push(shared, PhoneMsg {
                urgent: false,
                kind: "stop",
                text: format!("✅ {title} · {subtitle}\n{body}"),
                html: Some(format!("✅ {}\n{}\n{}", phone::head(&title, &project), phone::esc(t.task_done), phone::esc(&body))),
                buttons: vec![],
                pending_id: None,
            });
        }
        "notification" => {
            let nt = event.get("notification_type").and_then(Value::as_str).unwrap_or("");
            if nt == "quota_auto_resume_fired" {
                if config::get_bool(&cfg, "events.quota_auto_resume_fired", true) {
                    show_logo(app, shared, t.limit_resumed);
                }
                return;
            }
            if !nt.is_empty() && !config::get_bool(&cfg, &format!("events.{nt}"), false) {
                return;
            }
            if nt == "permission_prompt"
                && util::now_ms() - shared.last_permission_ms.load(Ordering::SeqCst) < 8000
            {
                return; // PermissionRequest already showed the richer card
            }
            shared.last_hook_banner_ms.store(util::now_ms(), Ordering::SeqCst);
            let label = match nt {
                "permission_prompt" => t.permission,
                "idle_prompt" => t.waiting_for_you,
                "agent_needs_input" => t.needs_input,
                _ => t.notification,
            };
            let title = event.get("title").and_then(Value::as_str).unwrap_or(t.code_name);
            let body = util::truncate(
                &util::clean_message(event.get("message").and_then(Value::as_str).unwrap_or("")),
                160,
            );
            let kind = if nt == "permission_prompt" { "permission" } else { "info" };
            banner(app, shared, from_claude_code(event), title, &with_project(label, event.get("cwd")), &body, kind, None);
        }
        "status" => handle_status(app, shared, event.get("status").unwrap_or(event)),
        // A sample of a Claude task-finished banner (titled "Claude"), so it shows the logo such
        // banners get with the current logo.style.
        "test-notify" => banner(
            app,
            shared,
            false,
            t.app_name,
            &format!("{} · {}", t.task_done, t.test),
            t.test_body,
            "done",
            None,
        ),
        "test-logo" => show_logo(app, shared, t.limit_five_hour),
        "reload-config" => reload(app, shared, true),
        "open-settings" => crate::settings::open(app, shared),
        "quit" => quit(app, shared),
        _ => {}
    }
}

/// Apply config.json again. `telegram`: also retry a token Telegram rejected and show the
/// pairing code again while no chat is paired (telegram::on_reload). Reload config always does;
/// the settings window only when it changed the phone switch or the bot token, so other
/// switches do not bring those banners back.
pub fn reload(app: &AppHandle, shared: &Arc<Shared>, telegram: bool) {
    shared.reload_config();
    if !crate::remote::enabled(shared) {
        crate::remote::stop_running(app, shared, false);
        crate::remote::clear_selection(shared);
    }
    // The page needs no config: never hand it the phone token or remote settings. It
    // only learns the look (logo.style).
    send_style(app, shared);
    resume_toast_listener(app, shared);
    crate::hotkey::reload();
    crate::tray::rebuild_menu(app, shared);
    if telegram {
        crate::telegram::on_reload(app, shared);
    }
    crate::settings::on_reload(app, shared);
}

// ---------------------------------------------------------------- interactive cards

fn start_interactive(
    app: &AppHandle,
    shared: &Arc<Shared>,
    kind: &str,
    event: Value,
    req: HookReq,
    via_permission: bool,
    // A test card (`--send test-question`): shown even with the terminal that sent it in front.
    test: bool,
) {
    let cfg = shared.cfg();
    let t = shared.t();
    if shared.pending.lock().unwrap().len() >= MAX_PENDING {
        // Too many open cards (a request flood): leave this one to Claude Code's own dialog.
        util::log_line(&["pending-cap", kind]);
        crate::http::respond_json(req, "{}".to_string());
        return;
    }
    // A remote run has nobody at its terminal: its cards always show and always reach the
    // phone, whatever the local switches say, or the request would be denied silently.
    let remote = crate::remote::is_remote_session(shared, event.get("session_id").and_then(Value::as_str));
    let enabled_key = if kind == "question" { "events.question" } else { "events.permission_request" };
    if !remote && !config::get_bool(&cfg, enabled_key, true) {
        crate::http::respond_json(req, "{}".to_string());
        return;
    }
    let skip = config::get_str_list(&cfg, "interactive.skipWhenFocused");
    let fg = shared.foreground.lock().unwrap().clone();
    // A focused Claude window / terminal only means "you can see the dialog" while someone
    // is actually at the PC; after a short idle time the card (and the phone) take over.
    // With a phone channel configured the card is never skipped: a question often arrives
    // seconds after you sent the prompt, and only an open card can reach the phone later.
    let active_ms = config::get_u64(&cfg, "interactive.skipOnlyIfActiveMs", 30000);
    let at_pc = active_ms == 0 || crate::win::idle_ms() < active_ms;
    // Only a phone channel that is on, ready and wants this kind of card changes the rules.
    let phone_hold = phone::configured(shared) && config::get_bool(&cfg, &format!("phone.events.{kind}"), true);
    // interactive.skipWhenActive off: the card always shows, whatever is in front.
    let focus_skip = !test
        && !phone_hold
        && at_pc
        && config::get_bool(&cfg, "interactive.skipWhenActive", true)
        && skip.iter().any(|s| s.eq_ignore_ascii_case(&fg));
    if !remote && (!config::get_bool(&cfg, "interactive.enabled", true) || focus_skip) {
        // The Claude app / a terminal is in front: its own dialog is visible, answer there.
        util::log_line(&[&format!("{kind}-skip"), if fg.is_empty() { "disabled" } else { &fg }]);
        shared.last_hook_banner_ms.store(util::now_ms(), Ordering::SeqCst);
        if kind == "permission" {
            shared.last_permission_ms.store(util::now_ms(), Ordering::SeqCst);
        }
        crate::http::respond_json(req, "{}".to_string());
        return;
    }

    let mut wait_ms = config::get_u64(&cfg, "interactive.waitMs", 20000).max(3000);
    // With a phone channel configured the card stays open long enough to be answered from
    // the phone; the PC card can still be answered at any time.
    if phone_hold || remote {
        wait_ms = wait_ms.max(config::get_u64(&cfg, "phone.holdMs", 480000));
    }
    let id = (shared.seq.fetch_add(1, Ordering::SeqCst) + 1).to_string();
    shared.pending.lock().unwrap().insert(
        id.clone(),
        Pending {
            req,
            kind: kind.to_string(),
            event: event.clone(),
            deadline_ms: util::now_ms() + wait_ms as i64,
            via_permission,
            partial: serde_json::Map::new(),
            phone_wait: None,
            on_phone: false,
            dismissed: false,
        },
    );
    shared.last_hook_banner_ms.store(util::now_ms(), Ordering::SeqCst);
    if kind == "permission" {
        shared.last_permission_ms.store(util::now_ms(), Ordering::SeqCst);
    }
    {
        let app = app.clone();
        let shared = shared.clone();
        let id = id.clone();
        std::thread::spawn(move || {
            let mut next_check = util::now_ms() + 2000;
            // (pid, since when) of a hook found gone.
            let mut gone: Option<(u32, i64)> = None;
            loop {
                let state = shared
                    .pending
                    .lock()
                    .unwrap()
                    .get(&id)
                    .map(|p| (p.deadline_ms - util::now_ms(), p.req.pid, p.kind == "question" && !p.via_permission));
                let Some((remaining, pid, adoptable)) = state else { return };
                if remaining <= 0 {
                    finish_interactive(&app, &shared, &id, None);
                    return;
                }
                let now = util::now_ms();
                if pid != 0 && now >= next_check {
                    next_check = now + 2000;
                    if crate::win::process_alive(pid) {
                        gone = None;
                    } else {
                        // A PreToolUse question whose hook timed out is normally picked up by
                        // the PermissionRequest that follows (with a new hook): give it a moment.
                        let since = match gone {
                            Some((p, at)) if p == pid => at,
                            _ => now,
                        };
                        gone = Some((pid, since));
                        if !adoptable || now - since >= 4000 {
                            close_orphaned(&app, &shared, &id, pid);
                            return;
                        }
                    }
                }
                std::thread::sleep(Duration::from_millis(remaining.clamp(10, 1000) as u64));
            }
        });
    }

    let mut payload = json!({
        "id": id,
        "kind": kind,
        // Cards only ever come from Claude Code.
        "icon": icon(&cfg, true),
        "waitMs": wait_ms,
        "nowLabel": t.now,
        "closeLabel": close_label(shared),
        "sound": config::get_bool(&cfg, "sound", true),
        "labels": {"allow": t.allow, "deny": t.deny, "answerInApp": t.answer_in_app, "answered": t.answered, "ok": "OK",
                   "other": t.other, "send": t.send, "typePlaceholder": t.type_placeholder, "answeredOnPhone": t.answered_on_phone},
        "title": t.code_name,
    });
    if kind == "question" {
        let questions: Vec<Value> = event
            .pointer("/tool_input/questions")
            .and_then(Value::as_array)
            .map(|qs| {
                qs.iter()
                    .filter_map(|q| {
                        let raw = q.get("question").and_then(Value::as_str).unwrap_or("");
                        let question = util::clean_message(raw);
                        if question.is_empty() {
                            return None;
                        }
                        let options: Vec<Value> = q
                            .get("options")
                            .and_then(Value::as_array)
                            .map(|os| {
                                os.iter()
                                    .filter_map(|o| {
                                        let label = o.get("label").and_then(Value::as_str).unwrap_or("");
                                        if label.is_empty() {
                                            return None;
                                        }
                                        Some(json!({
                                            "label": label,
                                            "description": o.get("description").and_then(Value::as_str).unwrap_or(""),
                                        }))
                                    })
                                    .collect()
                            })
                            .unwrap_or_default();
                        Some(json!({
                            "question": question,
                            // Claude Code matches answers by the exact original question text.
                            "key": raw,
                            "header": q.get("header").and_then(Value::as_str).unwrap_or(""),
                            "multiSelect": q.get("multiSelect").and_then(Value::as_bool).unwrap_or(false),
                            "options": options,
                        }))
                    })
                    .collect()
            })
            .unwrap_or_default();
        if questions.is_empty() {
            finish_interactive(app, shared, &id, None);
            return;
        }
        let subtitle = with_project(t.question, event.get("cwd"));
        let (text, html, buttons) = phone_question(shared, &id, &event, 0, &serde_json::Map::new()).unwrap_or_default();
        payload["subtitle"] = json!(subtitle);
        payload["questions"] = Value::Array(questions);
        emit(app, shared, "interactive", payload);
        let urgent = crate::remote::is_remote_session(shared, event.get("session_id").and_then(Value::as_str));
        phone::push(shared, PhoneMsg { urgent, kind: "question", text, html: Some(html), buttons, pending_id: Some(id.clone()) });

    } else {
        let subtitle = with_project(t.permission, event.get("cwd"));
        let tool = event.get("tool_name").and_then(Value::as_str).unwrap_or("");
        payload["subtitle"] = json!(subtitle);
        // The PC card shows the text unmasked: the user must see what they approve.
        payload["body"] = json!(permission_text(tool, event.get("tool_input"), CARD_TEXT_PC));
        emit(app, shared, "interactive", payload);
        let project = util::project_name(event.get("cwd"));
        let body_text = cut(&mask::secrets(&permission_text(tool, event.get("tool_input"), usize::MAX)), CARD_TEXT_PHONE);
        let text = format!("🔐 {} · {subtitle}\n{body_text}", t.code_name);
        let html = format!("🔐 {}\n{}\n<pre>{}</pre>", phone::head(t.code_name, &project), phone::esc(t.permission), phone::esc(&body_text));
        let buttons = vec![vec![
            (t.allow.to_string(), format!("p:{id}:a")),
            (t.deny.to_string(), format!("p:{id}:d")),
        ]];
        let urgent = crate::remote::is_remote_session(shared, event.get("session_id").and_then(Value::as_str));
        phone::push(shared, PhoneMsg { urgent, kind: "permission", text, html: Some(html), buttons, pending_id: Some(id.clone()) });
    }
}

/// Longest answer shown on an "answered" line of a phone question.
const ANSWER_SHOWN: usize = 80;

/// "2/3 ✓ answer" for question `qi` of `total` answered on the phone: one line, secrets
/// masked, a long answer cut. Plain text: escape it for Telegram HTML.
pub fn answered_text(qi: usize, total: usize, answer: &str) -> String {
    let one = mask::secrets(answer).split_whitespace().collect::<Vec<_>>().join(" ");
    let shown = if one.chars().count() > ANSWER_SHOWN { format!("{}…", util::truncate(&one, ANSWER_SHOWN - 1)) } else { one };
    if total > 1 {
        format!("{}/{total} ✓ {shown}", qi + 1)
    } else {
        format!("✓ {shown}")
    }
}

/// An "answered" line for every question of `qs` with an answer in `done` (question text ->
/// answer), in question order.
fn answered_lines(qs: &[Value], done: &serde_json::Map<String, Value>) -> Vec<String> {
    qs.iter()
        .enumerate()
        .filter_map(|(i, q)| {
            let answer = done.get(q.get("question").and_then(Value::as_str)?)?.as_str()?;
            Some(answered_text(i, qs.len(), answer))
        })
        .collect()
}

/// Title line of question `qi`'s phone message ("❓ Claude Code · project · Asking a
/// question · 2/3"), as (plain text, HTML).
fn question_title(shared: &Shared, event: &Value, total: usize, qi: usize) -> (String, String) {
    let t = shared.t();
    let project = util::project_name(event.get("cwd"));
    let progress = if total > 1 { format!(" · {}/{}", qi + 1, total) } else { String::new() };
    (
        format!("❓ {} · {}{progress}\n", t.code_name, t.question),
        format!("❓ {} · {}{progress}\n", phone::head(t.code_name, &project), phone::esc(t.question)),
    )
}

/// A question's header and text for the phone, as (plain text, HTML). Raw text is cut
/// before escaping, so the message stays far below Telegram's limit.
fn question_body(q: &Value) -> (String, String) {
    let question = cut(&mask::secrets(&util::clean_message(q.get("question").and_then(Value::as_str).unwrap_or(""))), 1500);
    let header = cut(&mask::secrets(q.get("header").and_then(Value::as_str).unwrap_or("")), 100);
    let mut html = String::new();
    if !header.is_empty() {
        html.push_str(&format!("<b>{}</b>\n", phone::esc(&header)));
    }
    html.push_str(&format!("{}\n", phone::esc(&question)));
    (format!("{question}\n"), html)
}

/// Phone message for question `qi` of a question card: (plain text, Telegram HTML, buttons).
/// Multi-question cards go to the phone one question at a time; the answers recorded so far
/// (`done`: question text -> answer) are listed above the question.
pub fn phone_question(
    shared: &Shared,
    id: &str,
    event: &Value,
    qi: usize,
    done: &serde_json::Map<String, Value>,
) -> Option<(String, String, Vec<Vec<(String, String)>>)> {
    let t = shared.t();
    let qs = event.pointer("/tool_input/questions").and_then(Value::as_array)?;
    let q = qs.get(qi)?;
    let (mut text, mut html) = question_title(shared, event, qs.len(), qi);
    let lines = answered_lines(qs, done);
    if !lines.is_empty() {
        let lines = lines.join("\n");
        text.push_str(&format!("{lines}\n\n"));
        html.push_str(&format!("{}\n\n", phone::esc_raw(&lines)));
    }
    let (body_text, body_html) = question_body(q);
    text.push_str(&body_text);
    html.push_str(&body_html);
    // Multi-select: options are ticked one by one and sent with Done.
    let multi = q.get("multiSelect").and_then(Value::as_bool).unwrap_or(false);
    let ticked = if multi { crate::telegram::multi_selected(id, qi) } else { Vec::new() };
    let mut buttons = Vec::new();
    if let Some(os) = q.get("options").and_then(Value::as_array) {
        for (i, o) in os.iter().enumerate() {
            let label = o.get("label").and_then(Value::as_str).unwrap_or("");
            if label.is_empty() {
                continue;
            }
            let desc = o.get("description").and_then(Value::as_str).unwrap_or("");
            let line = if desc.is_empty() { format!("{}. {}", i + 1, label) } else { format!("{}. {} — {}", i + 1, label, desc) };
            let line = cut(&mask::secrets(&line), 300);
            text.push_str(&line);
            text.push('\n');
            html.push_str(&format!("{}\n", phone::esc(&line)));
            let shown = util::truncate(&mask::secrets(label), 60);
            if multi {
                let mark = if ticked.contains(&i) { "☑" } else { "☐" };
                buttons.push(vec![(format!("{mark} {shown}"), format!("qm:{id}:{qi}:{i}"))]);
            } else {
                buttons.push(vec![(shown, format!("q:{id}:{qi}:{i}"))]);
            }
        }
    }
    if multi {
        buttons.push(vec![(format!("✓ {} ({})", t.tg_done, ticked.len()), format!("qd:{id}:{qi}"))]);
    }
    // Claude's question tool always accepts a typed answer.
    buttons.push(vec![(format!("✍️ {}", t.other), format!("qo:{id}:{qi}"))]);
    let hint = if multi { t.tg_hint_multi } else { t.tg_hint_reply };
    text.push_str(hint);
    html.push_str(&format!("\n<i>{}</i>", phone::esc(hint)));
    Some((text, html, buttons))
}

/// Question `qi` once it is answered on the phone, for the message that asked it: the
/// question with the recorded answer under it and no options (Telegram HTML).
pub fn phone_question_answered(shared: &Shared, event: &Value, qi: usize, answer: &str) -> Option<String> {
    let qs = event.pointer("/tool_input/questions").and_then(Value::as_array)?;
    let q = qs.get(qi)?;
    let (_, mut html) = question_title(shared, event, qs.len(), qi);
    html.push_str(&question_body(q).1);
    html.push_str(&format!("<b>{}</b>", phone::esc_raw(&answered_text(qi, qs.len(), answer))));
    Some(html)
}

/// The phone recorded answers for card `id` up to question `index`: the PC card moves on
/// too. `answers` holds every answer recorded so far (question text -> answer).
pub fn card_progress(app: &AppHandle, shared: &Shared, id: &str, index: usize, answers: &serde_json::Map<String, Value>) {
    emit_page(app, shared, "card-progress", json!({"id": id, "index": index, "answers": answers}));
}

/// A question card answered on the PC after some of its questions were answered on the
/// phone: the PC sends the whole set, and the answers recorded on the phone stand.
fn merge_phone_answers(mut data: Value, partial: &serde_json::Map<String, Value>) -> Value {
    if let Some(answers) = data.get_mut("answers").and_then(Value::as_object_mut) {
        for (k, v) in partial {
            answers.insert(k.clone(), v.clone());
        }
    }
    data
}

/// Resolve a pending card: `data` is the click ({answers} or {behavior}); None = timeout.
pub fn finish_interactive(app: &AppHandle, shared: &Arc<Shared>, id: &str, data: Option<Value>) {
    let pending = shared.pending.lock().unwrap().remove(id);
    let Some(p) = pending else { return };
    let data = match data {
        Some(d) if p.kind == "question" && !p.partial.is_empty() => Some(merge_phone_answers(d, &p.partial)),
        d => d,
    };
    let mut out = json!({});
    if let Some(d) = &data {
        if p.kind == "question" {
            if let Some(answers) = d.get("answers").filter(|a| a.is_object()) {
                let mut updated = p.event.get("tool_input").cloned().unwrap_or_else(|| json!({}));
                if !updated.is_object() {
                    updated = json!({});
                }
                updated["answers"] = answers.clone();
                out = if p.via_permission {
                    json!({"hookSpecificOutput": {
                        "hookEventName": "PermissionRequest",
                        "decision": {"behavior": "allow", "updatedInput": updated},
                    }})
                } else {
                    json!({"hookSpecificOutput": {
                        "hookEventName": "PreToolUse",
                        "permissionDecision": "allow",
                        "updatedInput": updated,
                    }})
                };
            }
        } else if let Some(b) = d.get("behavior").and_then(Value::as_str) {
            if b == "allow" {
                out = json!({"hookSpecificOutput": {"hookEventName": "PermissionRequest", "decision": {"behavior": "allow"}}});
            } else if b == "deny" {
                out = json!({"hookSpecificOutput": {"hookEventName": "PermissionRequest", "decision": {"behavior": "deny", "message": "Denied from HeadsUp"}}});
            }
        }
    }
    let answered = out.as_object().map(|o| !o.is_empty()).unwrap_or(false);
    if !answered && p.kind == "question" && !p.via_permission {
        if let Some(sid) = p.event.get("session_id").and_then(Value::as_str) {
            let mut ex = shared.expired_questions.lock().unwrap();
            ex.retain(|_, at| util::now_ms() - *at < 120_000);
            ex.insert(sid.to_string(), util::now_ms());
        }
    }
    // Only what happened, never the answer text: typed answers stay out of the log.
    let outcome = match (&data, p.kind.as_str()) {
        (None, _) => "timeout".to_string(),
        (Some(d), "permission") => d.get("behavior").and_then(Value::as_str).unwrap_or("none").to_string(),
        (Some(d), _) => format!("answered={}", d.get("answers").and_then(Value::as_object).map_or(0, |m| m.len())),
    };
    util::log_line(&[&format!("{}-answer", p.kind), id, &outcome]);
    crate::http::respond_json(p.req, out.to_string());
    emit_page(app, shared, "resolve", json!({"id": id, "answered": answered}));
    util::log_line(&["resolve-sent", id]);
    let t = shared.t();
    let summary = match (p.kind.as_str(), data.as_ref().filter(|_| answered)) {
        ("permission", Some(d)) => {
            if d.get("behavior").and_then(Value::as_str) == Some("allow") {
                format!("✓ {}", t.allow)
            } else {
                format!("✗ {}", t.deny)
            }
        }
        ("question", Some(d)) => d
            .get("answers")
            .and_then(Value::as_object)
            .map(|m| {
                // In question order (the map itself is sorted by key).
                let order: Vec<String> = p
                    .event
                    .pointer("/tool_input/questions")
                    .and_then(Value::as_array)
                    .map(|qs| qs.iter().filter_map(|q| q.get("question").and_then(Value::as_str).map(String::from)).collect())
                    .unwrap_or_default();
                let shown = |a: &str| phone::esc(&cut(&mask::secrets(a), 300));
                let mut all: Vec<String> = order.iter().filter_map(|k| m.get(k)).filter_map(Value::as_str).map(shown).collect();
                if all.is_empty() {
                    all = m.values().filter_map(Value::as_str).map(shown).collect();
                }
                format!("✓ {}", all.join(" · "))
            })
            .unwrap_or_else(|| "✓".to_string()),
        _ => t.tg_expired.to_string(),
    };
    phone::resolved(shared, id, summary);
}

/// The hook behind a card has exited: the request was answered or cancelled in the
/// terminal. Close the card without a reply, on the PC and on the phone.
fn close_orphaned(app: &AppHandle, shared: &Arc<Shared>, id: &str, pid: u32) {
    let p = {
        let mut pending = shared.pending.lock().unwrap();
        // Adopted by a newer hook meanwhile: that one is alive.
        if pending.get(id).map(|p| p.req.pid) != Some(pid) {
            return;
        }
        pending.remove(id)
    };
    let Some(p) = p else { return };
    util::log_line(&["card-closed", id, &p.kind, "hook-exited"]);
    drop(p);
    emit_page(app, shared, "resolve", json!({"id": id, "answered": true}));
    phone::resolved(shared, id, shared.t().tg_closed.to_string());
}

/// The user closed card `id` on the PC (its × or the hide hotkey): it never shows there again.
/// With a phone that can answer it (a paired Telegram chat) and the card on its way there, it
/// stays open for the phone; a message still waiting for the PC to go idle goes out now.
/// Otherwise the hook gets `{}` at once and Claude Code shows its own dialog.
pub fn dismiss_card(app: &AppHandle, shared: &Arc<Shared>, id: &str) {
    // Before the pending lock: it reads the config.
    let phone_answers = phone::answers_cards(shared);
    let (held, send) = {
        let mut pending = shared.pending.lock().unwrap();
        let Some(p) = pending.get_mut(id).filter(|p| !p.dismissed) else { return };
        p.dismissed = true;
        keep_for_phone(phone_answers, &mut p.on_phone, &mut p.phone_wait)
    };
    if held {
        util::log_line(&["card-dismissed", id, "phone"]);
        if let Some(msg) = send {
            phone::send_now(shared, msg);
        }
    } else {
        util::log_line(&["card-dismissed", id, "released"]);
        // No answer: the same reply as a timeout, so the question does not come back as a card
        // through the permission request that follows.
        finish_interactive(app, shared, id, Some(json!({})));
    }
}

/// What closing a card on the PC does with it: (it stays open for the phone, its message to
/// send there now). It stays when the phone can answer it and its message went there or was
/// still waiting to go; a waiting message is taken, so the deferred send finds nothing left.
fn keep_for_phone(phone_answers: bool, on_phone: &mut bool, phone_wait: &mut Option<PhoneMsg>) -> (bool, Option<PhoneMsg>) {
    if !phone_answers {
        return (false, None);
    }
    let send = phone_wait.take();
    *on_phone |= send.is_some();
    (*on_phone, send)
}

/// The hide hotkey: every banner, card and the limit-reset logo leave the screen. Open cards go
/// as with their ×: on to the phone, or back to Claude Code's own dialog (dismiss_card).
pub fn hide_all(app: &AppHandle, shared: &Arc<Shared>) {
    let ids: Vec<String> = shared
        .pending
        .lock()
        .unwrap()
        .iter()
        .filter(|(_, p)| !p.dismissed)
        .map(|(id, _)| id.clone())
        .collect();
    util::log_line(&["hide-all", &ids.len().to_string()]);
    // The page first: it drops these cards for good (even one whose own event is still on its
    // way), so a card released below never shows "Answer in the app" first.
    emit_page(app, shared, "hide-all", json!({"ids": ids}));
    for id in &ids {
        dismiss_card(app, shared, id);
    }
}

// ---------------------------------------------------------------- limits

/// Rate-limit windows the status line reports; any other name is ignored.
pub const LIMIT_WINDOWS: [&str; 5] = ["five_hour", "seven_day", "seven_day_opus", "seven_day_sonnet", "spend_limit"];

pub fn load_state() -> Value {
    let mut st = util::read_json(&util::state_path()).unwrap_or_else(|| json!({}));
    if !st.is_object() {
        st = json!({});
    }
    match st.get_mut("windows").and_then(Value::as_object_mut) {
        // Drop windows an older version accepted from anyone.
        Some(ws) => ws.retain(|k, _| LIMIT_WINDOWS.contains(&k.as_str())),
        None => st["windows"] = json!({}),
    }
    st
}

fn save_state(st: &Value) {
    let _ = util::write_json(&util::state_path(), st);
}

fn window_label(shared: &Shared, name: &str) -> &'static str {
    let t = shared.t();
    match name {
        "five_hour" => t.limit_five_hour,
        "seven_day" => t.limit_seven_day,
        "spend_limit" => t.limit_spend,
        _ => t.limit_generic,
    }
}

pub fn handle_status(app: &AppHandle, shared: &Arc<Shared>, status: &Value) {
    let Some(rl) = status.get("rate_limits").and_then(Value::as_object) else { return };
    let mut expired: Vec<(String, i64)> = Vec::new();
    {
        let mut st = shared.state.lock().unwrap();
        if !st.get("windows").map(Value::is_object).unwrap_or(false) {
            st["windows"] = json!({});
        }
        let windows = st["windows"].as_object_mut().unwrap();
        let now = util::now_secs();
        for (name, info) in rl.iter().filter(|(n, _)| LIMIT_WINDOWS.contains(&n.as_str())) {
            let Some(incoming) = info.get("resets_at").and_then(num_i64) else { continue };
            // Not a real window: long past, or further ahead than any limit period.
            if incoming < now - 86_400 || incoming > now + 40 * 86_400 {
                continue;
            }
            let w = windows.entry(name.clone()).or_insert_with(|| json!({}));
            if !w.is_object() {
                *w = json!({});
            }
            let prev = w.get("resetsAt").and_then(num_i64);
            let fired = w.get("firedFor").and_then(num_i64);
            if let Some(p) = prev {
                // A new window started while we were not watching: fire for the old one.
                if incoming > p && fired != Some(p) && p * 1000 <= util::now_ms() {
                    expired.push((name.clone(), p));
                }
            }
            if prev != Some(incoming) {
                w["usedPercentage"] = json!(0);
            }
            w["resetsAt"] = json!(incoming);
            if let Some(used) = info.get("used_percentage").and_then(Value::as_f64).filter(|u| (0.0..=100.0).contains(u)) {
                let cur = w.get("usedPercentage").and_then(Value::as_f64).unwrap_or(0.0);
                w["usedPercentage"] = json!(used.max(cur));
            }
        }
        save_state(&st);
    }
    for (name, at) in expired {
        fire_reset(app, shared, &name, at);
    }
}

fn fire_reset(app: &AppHandle, shared: &Arc<Shared>, name: &str, resets_at: i64) {
    let used = {
        let mut st = shared.state.lock().unwrap();
        let used = {
            let Some(w) = st.get_mut("windows").and_then(|w| w.get_mut(name)) else { return };
            if w.get("firedFor").and_then(num_i64) == Some(resets_at) {
                return;
            }
            w["firedFor"] = json!(resets_at);
            w.get("usedPercentage").and_then(Value::as_f64).unwrap_or(0.0)
        };
        save_state(&st);
        used
    };
    // A reset that happened hours ago (PC was off) is not news any more.
    if util::now_ms() - resets_at * 1000 > 3 * 3600 * 1000 {
        return;
    }
    let cfg = shared.cfg();
    if !config::get_bool(&cfg, &format!("limits.{name}"), true) {
        return;
    }
    if used < config::get_f64(&cfg, "limits.minUsedPercentage", 0.0) {
        return;
    }
    util::log_line(&["reset", name, &resets_at.to_string(), &format!("used={used}")]);
    show_logo(app, shared, window_label(shared, name));
    phone::push(shared, PhoneMsg { urgent: false, kind: "limit", text: format!("🟠 {}", window_label(shared, name)), html: Some(format!("🟠 <b>{}</b>", phone::esc(window_label(shared, name)))), buttons: vec![], pending_id: None });
}

pub fn start_limit_timer(app: AppHandle, shared: Arc<Shared>) {
    std::thread::spawn(move || loop {
        std::thread::sleep(Duration::from_secs(1));
        if shared.quitting.load(Ordering::SeqCst) {
            return;
        }
        let due: Vec<(String, i64)> = {
            let st = shared.state.lock().unwrap();
            st.get("windows")
                .and_then(Value::as_object)
                .map(|ws| {
                    ws.iter()
                        .filter_map(|(n, w)| {
                            let at = w.get("resetsAt").and_then(num_i64)?;
                            let fired = w.get("firedFor").and_then(num_i64);
                            if fired != Some(at) && at * 1000 <= util::now_ms() {
                                Some((n.clone(), at))
                            } else {
                                None
                            }
                        })
                        .collect()
                })
                .unwrap_or_default()
        };
        for (n, at) in due {
            fire_reset(&app, &shared, &n, at);
        }
    });
}

fn summarize_limits(status: Option<&Value>) -> String {
    let Some(rl) = status.and_then(|s| s.get("rate_limits")).and_then(Value::as_object) else {
        return "no-rate-limits".to_string();
    };
    rl.iter()
        .map(|(k, v)| {
            format!(
                "{k}={}%@{}",
                v.get("used_percentage").map(|x| x.to_string()).unwrap_or_default(),
                v.get("resets_at").map(|x| x.to_string()).unwrap_or_default()
            )
        })
        .collect::<Vec<_>>()
        .join(" ")
}

// ---------------------------------------------------------------- toast listener bridge

/// The listener mirrors toasts and also reports the window in front, which
/// interactive.skipWhenActive needs: it runs while either is on.
fn listener_wanted(cfg: &Value) -> bool {
    config::get_bool(cfg, "toasts.enabled", true) || config::get_bool(cfg, "interactive.skipWhenActive", true)
}

pub fn start_toast_listener(app: AppHandle, shared: Arc<Shared>) {
    std::thread::spawn(move || loop {
        if shared.quitting.load(Ordering::SeqCst) {
            return;
        }
        let cfg = shared.cfg();
        if !listener_wanted(&cfg) {
            *shared.listener_status.lock().unwrap() = "disabled".to_string();
            return;
        }
        let script = util::hooks_dir().join("headsup-toast-listener.ps1");
        if !script.exists() {
            *shared.listener_status.lock().unwrap() = "missing".to_string();
            return;
        }
        let mut child = match util::powershell()
            .arg("-File")
            .arg(&script)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
        {
            Ok(c) => c,
            Err(e) => {
                util::log_line(&["listener-error", &e.to_string()]);
                *shared.listener_status.lock().unwrap() = "error".to_string();
                return;
            }
        };
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        *shared.listener_child.lock().unwrap() = Some(child);
        if let Some(err) = stderr {
            std::thread::spawn(move || {
                for line in BufReader::new(err).lines().map_while(Result::ok) {
                    let l = line.trim();
                    if !l.is_empty() {
                        util::log_line(&["listener-stderr", &util::truncate(l, 300)]);
                    }
                }
            });
        }
        if let Some(out) = stdout {
            for line in BufReader::new(out).lines().map_while(Result::ok) {
                let l = line.trim();
                if !l.is_empty() {
                    handle_listener_line(&app, &shared, l);
                }
            }
        }
        let code = {
            let mut guard = shared.listener_child.lock().unwrap();
            let c = guard.as_mut().and_then(|c| c.wait().ok()).and_then(|s| s.code());
            *guard = None;
            c
        };
        util::log_line(&["listener-exit", &code.map(|c| c.to_string()).unwrap_or_else(|| "null".to_string())]);
        if shared.quitting.load(Ordering::SeqCst) || *shared.listener_status.lock().unwrap() != "allowed" {
            return;
        }
        std::thread::sleep(Duration::from_secs(5)); // crashed: restart
    });
}

/// toasts.enabled (or interactive.skipWhenActive) was switched on (settings window, Reload
/// config) after the listener had stopped because both were off: start it now rather than at
/// the next start of the overlay.
fn resume_toast_listener(app: &AppHandle, shared: &Arc<Shared>) {
    if !listener_wanted(&shared.cfg()) {
        return;
    }
    {
        let mut status = shared.listener_status.lock().unwrap();
        if *status != "disabled" {
            return;
        }
        *status = "starting".to_string();
    }
    start_toast_listener(app.clone(), shared.clone());
}

fn handle_listener_line(app: &AppHandle, shared: &Arc<Shared>, line: &str) {
    let Ok(msg) = serde_json::from_str::<Value>(line) else { return };
    match msg.get("event").and_then(Value::as_str).unwrap_or("") {
        "foreground" => {
            *shared.foreground.lock().unwrap() =
                msg.get("process").and_then(Value::as_str).unwrap_or("").to_string();
        }
        "access" => {
            let status = msg.get("status").and_then(Value::as_str).unwrap_or("");
            let allowed = status == "Allowed";
            *shared.listener_status.lock().unwrap() =
                if allowed { "allowed".to_string() } else { "denied".to_string() };
            util::log_line(&["listener-access", status]);
            // Running only for skipWhenActive: no mirrored toasts to miss, so no banner.
            if !allowed && config::get_bool(&shared.cfg(), "toasts.enabled", true) {
                let t = shared.t();
                notify(app, shared, "HeadsUp", t.access_denied, t.access_denied_body, "permission", Some(12000));
            }
        }
        "error" => util::log_line(&[
            "listener-error",
            &util::truncate(msg.get("message").and_then(Value::as_str).unwrap_or(""), 300),
        ]),
        "toast" => handle_toast(app, shared, &msg),
        _ => {}
    }
}

fn handle_toast(app: &AppHandle, shared: &Arc<Shared>, msg: &Value) {
    let cfg = shared.cfg();
    // Switched off while the listener runs (it keeps running: it also reports the window in
    // front for interactive.skipWhenFocused).
    if !config::get_bool(&cfg, "toasts.enabled", true) {
        return;
    }
    let mut watch = config::get_str_list(&cfg, "toasts.watchApps");
    if watch.is_empty() {
        watch.push("Claude".to_string());
    }
    let app_name = msg.get("app").and_then(Value::as_str).unwrap_or("").to_string();
    let aumid = msg.get("aumid").and_then(Value::as_str).unwrap_or("");
    // Exact app name or app id (case-insensitive): "Claude" must not also match "Claude Helper".
    let (name_l, aumid_l) = (app_name.trim().to_lowercase(), aumid.trim().to_lowercase());
    let watched = watch.iter().any(|w| {
        let w = w.trim().to_lowercase();
        !w.is_empty() && (w == name_l || w == aumid_l)
    });
    if !watched {
        return; // other apps' toasts are none of our business, not even in the log
    }
    let texts: Vec<String> = msg
        .get("texts")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).filter(|s| !s.is_empty()).map(String::from).collect())
        .unwrap_or_default();
    let chars: usize = texts.iter().map(|s| s.chars().count()).sum();
    util::log_line(&["toast", &app_name, &format!("chars={chars}")]);

    let dedupe = config::get_u64(&cfg, "toasts.dedupeWindowMs", 6000) as i64;
    if util::now_ms() - shared.last_hook_banner_ms.load(Ordering::SeqCst) < dedupe {
        return; // a hook already showed richer info
    }
    // Hold briefly: when a Claude Code hook for the same moment arrives, it wins (it carries
    // the actual message / command). Chat and Cowork have no hooks, so their toast shows.
    let arrived = util::now_ms();
    let hold = config::get_u64(&cfg, "toasts.holdMs", 1500);
    let from_claude = claude_app(&app_name, aumid);
    let app = app.clone();
    let shared = shared.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(hold));
        if shared.last_hook_banner_ms.load(Ordering::SeqCst) >= arrived - 500 {
            return;
        }
        show_toast_banner(&app, &shared, &app_name, &texts, from_claude);
    });
}

fn show_toast_banner(app: &AppHandle, shared: &Arc<Shared>, app_name: &str, texts: &[String], from_claude: bool) {
    let t = shared.t();
    let head = texts.first().cloned().unwrap_or_default();
    let mut body = texts.iter().skip(1).cloned().collect::<Vec<_>>().join(" · ");
    let probe = format!("{head} {body}").to_lowercase();
    let (kind, label) = if probe.contains("izin") || probe.contains("permission") {
        ("permission", t.permission)
    } else if i18n::is_generic_waiting(&body) {
        body.clear();
        ("done", t.reply_ready)
    } else if body.contains('?') {
        ("question", t.question)
    } else if probe.contains("tamamland")
        || probe.contains("complete")
        || probe.contains("finished")
        || probe.contains("done")
    {
        ("done", t.task_done)
    } else {
        ("done", t.reply_ready)
    };
    let subtitle = if head.is_empty() { label.to_string() } else { format!("{label} · {head}") };
    let title = if app_name.is_empty() { t.app_name } else { app_name };
    banner(app, shared, from_claude, title, &subtitle, &body, kind, None);
    let (subtitle, body) = (mask::secrets(&subtitle), mask::secrets(&body));
    phone::push(shared, PhoneMsg { urgent: false, kind: "stop", text: format!("💬 {title} · {subtitle}\n{body}"), html: Some(format!("💬 <b>{}</b> · {}\n{}", phone::esc(title), phone::esc(&subtitle), phone::esc(&body))), buttons: vec![], pending_id: None });
}

// ---------------------------------------------------------------- cursor polling

fn set_capture(app: &AppHandle, shared: &Shared, flag: bool) {
    if shared.capturing.swap(flag, Ordering::SeqCst) == flag {
        return;
    }
    util::log_line(&["mouse", if flag { "capture" } else { "passthrough" }]);
    if let Some(w) = window(app) {
        let _ = w.set_ignore_cursor_events(!flag);
        if !shared.typing.load(Ordering::SeqCst) {
            crate::win::ensure_noactivate(&w);
        }
    }
}

/// While interactive cards (or a limit-reset logo that waits for a click on it) are on
/// screen, poll the cursor and lift the click-through only over them. No OS-level mouse hook
/// involved; a logo that waits for a click anywhere is served by polling the mouse buttons.
pub fn start_cursor_poll(app: AppHandle, shared: Arc<Shared>) {
    std::thread::spawn(move || {
        let mut clicks = crate::win::ClickWatch::default();
        let mut tick: u32 = 0;
        let mut buried_logged = false;
        loop {
            std::thread::sleep(Duration::from_millis(40));
            if shared.quitting.load(Ordering::SeqCst) {
                return;
            }
            poll_logo_click(&app, &shared, &mut clicks);
            // About once a second while something is on screen: a window that went fullscreen
            // after the banner appeared can end up above the overlay; put it back on top.
            tick = tick.wrapping_add(1);
            if tick % 25 == 0 && shared.active_items.load(Ordering::SeqCst) > 0 {
                if let Some(win) = window(&app) {
                    if crate::win::is_buried(&win) {
                        crate::win::raise_topmost(&win, true);
                        if !buried_logged {
                            util::log_line(&["zorder-restored"]);
                            buried_logged = true;
                        }
                    } else {
                        buried_logged = false;
                    }
                }
            }
            let rects = shared.hot_rects.lock().unwrap().clone();
            if rects.is_empty() {
                set_capture(&app, &shared, false);
                continue;
            }
            let Some(win) = window(&app) else { continue };
            if !shared.typing.load(Ordering::SeqCst) {
                crate::win::ensure_noactivate(&win);
            }
            let (Ok(cur), Ok(pos), Ok(scale)) = (app.cursor_position(), win.outer_position(), win.scale_factor()) else {
                continue;
            };
            let x = (cur.x - pos.x as f64) / scale;
            let y = (cur.y - pos.y as f64) / scale;
            let inside = rects
                .iter()
                .any(|r| x >= r.left - 4.0 && x <= r.right + 4.0 && y >= r.top - 4.0 && y <= r.bottom + 4.0);
            set_capture(&app, &shared, inside);
        }
    });
}

/// logo.dismiss "anyClick": the overlay is click-through, so a click anywhere on the screen
/// (any mouse button) is seen here, as a button going down, and the page is told to hide the
/// logo. Clicks within the guard time after the logo appeared do not count.
fn poll_logo_click(app: &AppHandle, shared: &Shared, clicks: &mut crate::win::ClickWatch) {
    if !shared.logo_any_click.load(Ordering::SeqCst) {
        return;
    }
    // Polled during the guard time too, so a press made (or still held) then is not reported
    // once it is over.
    let clicked = clicks.poll();
    if clicked
        && util::now_ms() >= shared.logo_armed_at.load(Ordering::SeqCst)
        && shared.logo_any_click.swap(false, Ordering::SeqCst)
    {
        util::log_line(&["logo", "dismissed-by-click"]);
        emit_page(app, shared, "logo-dismiss", json!({}));
    }
}

// ---------------------------------------------------------------- lifecycle

/// Close every open card that belongs to this session (its run was stopped or ended), so it
/// neither blocks its hook for minutes nor captures later Telegram messages.
pub fn cancel_pending_for_session(app: &AppHandle, shared: &Arc<Shared>, session_id: &str) {
    let ids: Vec<String> = shared
        .pending
        .lock()
        .unwrap()
        .iter()
        .filter(|(_, p)| p.event.get("session_id").and_then(Value::as_str) == Some(session_id))
        .map(|(id, _)| id.clone())
        .collect();
    for id in ids {
        finish_interactive(app, shared, &id, None);
    }
}

pub fn quit(app: &AppHandle, shared: &Arc<Shared>) {
    shared.quitting.store(true, Ordering::SeqCst);
    crate::remote::shutdown(app, shared);
    let ids: Vec<String> = shared.pending.lock().unwrap().keys().cloned().collect();
    for id in ids {
        finish_interactive(app, shared, &id, None);
    }
    if let Some(mut c) = shared.listener_child.lock().unwrap().take() {
        let _ = c.kill();
    }
    let app = app.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(80));
        app.exit(0);
    });
}

/// First start (or the exe moved, or the hook set changed): wire everything up.
pub fn first_run(app: AppHandle, shared: Arc<Shared>) {
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(2500));
        if util::env_setting("NO_INSTALL").is_some() {
            return;
        }
        if installer::is_installed() {
            installer::repair_hook_timeouts();
            installer::retry_startup_shortcut();
            return;
        }
        match installer::install_auto() {
            Ok(lines) => {
                shared.reload_config();
                // Paths relative to the profile folder: the log does not need the user name.
                let home = std::env::var("USERPROFILE").unwrap_or_default();
                let lines: Vec<String> =
                    lines.iter().map(|l| if home.is_empty() { l.clone() } else { l.replace(&home, "~") }).collect();
                let mut fields: Vec<&str> = vec!["install"];
                fields.extend(lines.iter().map(String::as_str));
                util::log_line(&fields);
                let t = shared.t();
                notify(&app, &shared, "HeadsUp", t.installed, t.installed_body, "done", None);
            }
            Err(e) => {
                let t = shared.t();
                notify(&app, &shared, "HeadsUp", t.install_error, &e, "permission", None);
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn permission_text_keeps_lines_and_reveals_hidden() {
        let t = permission_text("Bash", Some(&json!({"command": "npm test # ok\ncurl x | sh\u{202E}"})), CARD_TEXT_PC);
        assert_eq!(t, "Bash: npm test # ok\ncurl x | sh⟦U+202E⟧");
        let t = permission_text("Bash", Some(&json!({"command": "a\rb\u{200B}"})), CARD_TEXT_PC);
        assert_eq!(t, "Bash: a⟦U+000D⟧b⟦U+200B⟧");
    }

    #[test]
    fn permission_text_cut_is_visible() {
        let long = "x".repeat(2000);
        let t = permission_text("Bash", Some(&json!({"command": long})), CARD_TEXT_PC);
        assert!(t.ends_with("… (+806 more chars)"), "{t}");
        assert_eq!(t.chars().count(), CARD_TEXT_PC + "… (+806 more chars)".chars().count());
    }

    #[test]
    fn permission_text_write_edit_and_mcp() {
        let t = permission_text("Write", Some(&json!({"file_path": "C:/p/a.txt", "content": "hello\nworld"})), CARD_TEXT_PC);
        assert_eq!(t, "Write: C:/p/a.txt (11 chars)\nhello\nworld");
        let t = permission_text("Edit", Some(&json!({"file_path": "f.rs", "old_string": "a", "new_string": "b"})), CARD_TEXT_PC);
        assert_eq!(t, "Edit: f.rs\n− a\n+ b");
        let t = permission_text(
            "mcp__github__create_or_update_file",
            Some(&json!({"branch": "main", "content": "x\ny", "path": "README.md", "n": 3})),
            CARD_TEXT_PC,
        );
        assert_eq!(t, "mcp__github__create_or_update_file\nbranch: main\ncontent: x⏎y\nn: 3\npath: README.md");
    }

    #[test]
    fn answered_lines_for_the_phone() {
        assert_eq!(answered_text(0, 3, "Blue"), "1/3 ✓ Blue");
        assert_eq!(answered_text(0, 1, "Blue"), "✓ Blue");
        // One line, secrets masked, a long answer cut.
        assert_eq!(answered_text(1, 3, "line one\n  line two"), "2/3 ✓ line one line two");
        let t = answered_text(2, 3, "use password=hunter2 here");
        assert!(t.starts_with("3/3 ✓ use password=") && !t.contains("hunter2"), "{t}");
        let long = answered_text(0, 2, &"é".repeat(200));
        assert!(long.ends_with('…') && long.chars().count() == "1/2 ✓ ".chars().count() + ANSWER_SHOWN, "{long}");
        // In question order, only the answered ones.
        let qs = vec![json!({"question": "A?"}), json!({"question": "B?"}), json!({"question": "C?"})];
        let done = json!({"C?": "z", "A?": "x"});
        assert_eq!(answered_lines(&qs, done.as_object().unwrap()), vec!["1/3 ✓ x", "3/3 ✓ z"]);
        assert!(answered_lines(&qs, &serde_json::Map::new()).is_empty());
    }

    #[test]
    fn phone_answers_stand_in_a_pc_submission() {
        let partial = json!({"A?": "phone"}).as_object().unwrap().clone();
        let merged = merge_phone_answers(json!({"answers": {"A?": "pc", "B?": "pc"}}), &partial);
        assert_eq!(merged, json!({"answers": {"A?": "phone", "B?": "pc"}}));
        let merged = merge_phone_answers(json!({"answers": {"B?": "pc"}}), &partial);
        assert_eq!(merged, json!({"answers": {"A?": "phone", "B?": "pc"}}));
        // Anything without answers is left alone.
        assert_eq!(merge_phone_answers(json!({"behavior": "allow"}), &partial), json!({"behavior": "allow"}));
    }

    #[test]
    fn closed_cards_stay_for_the_phone_only_when_it_can_answer() {
        let msg = || PhoneMsg { urgent: false, kind: "question", text: "q".into(), html: None, buttons: vec![], pending_id: Some("7".into()) };
        // On the phone already: it stays, nothing more is sent.
        let (mut on, mut wait) = (true, None);
        assert!(matches!(keep_for_phone(true, &mut on, &mut wait), (true, None)));
        // Waiting for the PC to go idle: it goes now, once.
        let (mut on, mut wait) = (false, Some(msg()));
        let (held, send) = keep_for_phone(true, &mut on, &mut wait);
        assert!(held && on && wait.is_none() && send.is_some_and(|m| m.pending_id.as_deref() == Some("7")));
        assert!(matches!(keep_for_phone(true, &mut on, &mut wait), (true, None)));
        // Never meant for the phone (its kind switched off there): released.
        let (mut on, mut wait) = (false, None);
        assert!(matches!(keep_for_phone(true, &mut on, &mut wait), (false, None)));
        // No phone that can answer (off, ntfy only): released, and a waiting message stays put
        // (it goes with the card).
        let (mut on, mut wait) = (true, Some(msg()));
        assert!(matches!(keep_for_phone(false, &mut on, &mut wait), (false, None)));
        assert!(wait.is_some());
    }

    #[test]
    fn claude_logo_only_for_claude_sources() {
        assert!(from_claude_code(&json!({"cwd": "C:/p"})));
        assert!(from_claude_code(&json!({"agent": "Claude"})));
        assert!(!from_claude_code(&json!({"agent": "codex", "title": "Codex"})));
        assert!(claude_app("Claude", ""));
        assert!(claude_app("", "Claude_pzs8sxrjxfjjc!Claude"));
        assert!(claude_app("", "com.squirrel.AnthropicClaude.claude"));
        assert!(!claude_app("Claude Helper", "Microsoft.WindowsTerminal_8wekyb3d8bbwe!App"));
        assert!(!claude_app("Codex", ""));
        let claude = json!({"logo": {"style": "claude"}});
        let headsup = json!({"logo": {"style": "headsup"}});
        assert_eq!(icon(&claude, true), "claude");
        assert_eq!(icon(&claude, false), "headsup");
        assert_eq!(icon(&headsup, true), "headsup");
        assert_eq!(icon(&json!({}), true), "headsup");
    }
}
