//! Writes the Claude Code hooks into ~/.claude/settings.json, wraps the status line,
//! records where the overlay exe lives (for the hooks' autostart) and adds a Startup
//! shortcut. Everything is reversible with `uninstall`.
use crate::{config, util};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

/// Bump when the set of hooks changes so existing installs re-run the installer.
/// 9: StopFailure (rate_limit) and the quota_auto_resume_stale / _disabled notifications.
pub const INSTALL_VERSION: u64 = 9;
/// Notification types the Notification hook is sent: Claude Code's own continue-at-reset reports
/// all three quota_auto_resume_* (only _fired shows anything; the others are logged).
const NOTIFICATION_MATCHER: &str =
    "permission_prompt|idle_prompt|agent_needs_input|quota_auto_resume_fired|quota_auto_resume_stale|quota_auto_resume_disabled";
/// Our hook scripts as every version wrote them into settings.json (always with forward
/// slashes): the %APPDATA% copy (or a repo checkout), the old Electron build's resources
/// folder, and our exact script names in any other `hooks` folder (an Electron dev run from
/// a checkout with another name). The claude-overlay-* names are Claude Overlay 2.x's
/// (LEGACY_MARKERS): an upgrade strips them and installs the new ones. A user's own hook does
/// not match just because its name starts the same way.
const MARKERS: [&str; 4] = [
    "HeadsUp/hooks/headsup-",
    "/hooks/headsup-send.ps1",
    "/hooks/headsup-statusline.ps1",
    "/hooks/headsup-codex.ps1",
];
const LEGACY_MARKERS: [&str; 5] = [
    "claude-notification/hooks/claude-overlay-",
    "/resources/hooks/claude-overlay-",
    "/hooks/claude-overlay-send.ps1",
    "/hooks/claude-overlay-statusline.ps1",
    "/hooks/claude-overlay-codex.ps1",
];
const CODEX_SCRIPT: &str = "headsup-codex.ps1";
/// The Codex adapter of Claude Overlay 2.x: a notify running it is ours too.
const LEGACY_CODEX_SCRIPT: &str = "claude-overlay-codex.ps1";

fn is_our_codex(stmt: &str) -> bool {
    stmt.contains(CODEX_SCRIPT) || stmt.contains(LEGACY_CODEX_SCRIPT)
}

fn claude_dir() -> PathBuf {
    if let Some(d) = std::env::var_os("CLAUDE_CONFIG_DIR") {
        return PathBuf::from(d);
    }
    let home = std::env::var_os("USERPROFILE")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    home.join(".claude")
}

pub fn settings_path() -> PathBuf {
    claude_dir().join("settings.json")
}

fn backup_path() -> PathBuf {
    claude_dir().join("settings.json.headsup.bak")
}

/// The backup taken before the very first install: Claude Overlay 2.x's if there is one
/// (that name stays, it holds the user's settings from before any version of ours).
fn existing_backup() -> Option<PathBuf> {
    [claude_dir().join("settings.json.claude-overlay.bak"), backup_path()]
        .into_iter()
        .find(|p| p.exists())
}

fn startup_dir() -> Option<PathBuf> {
    std::env::var_os("APPDATA").map(|a| {
        PathBuf::from(a)
            .join("Microsoft")
            .join("Windows")
            .join("Start Menu")
            .join("Programs")
            .join("Startup")
    })
}

fn shortcut_path() -> Option<PathBuf> {
    startup_dir().map(|d| d.join("HeadsUp.lnk"))
}

/// Claude Overlay 2.x's Startup shortcut. It starts the old exe, so it is replaced by ours
/// and removed with it.
fn legacy_shortcut_path() -> Option<PathBuf> {
    startup_dir().map(|d| d.join("Claude Overlay.lnk"))
}

fn fwd(p: &Path) -> String {
    p.to_string_lossy().replace('\\', "/")
}

/// The hook command line. Claude Code runs it through Git Bash (or PowerShell), and both
/// expand `$name` and (bash) backticks inside double quotes: such a path goes in single
/// quotes, which both shells take literally. Every other path keeps the double quotes.
fn ps_command(script: &Path, arg: Option<&str>) -> String {
    let path = fwd(script);
    let expands = path.contains(|c| c == '$' || c == '`');
    let quoted = if expands && !path.contains('\'') {
        format!("'{path}'")
    } else {
        if expands {
            util::log_line(&["install-warn", "the hooks folder path has $ or ` and a quote; hooks may not run"]);
        }
        format!("\"{path}\"")
    };
    let mut s = format!("powershell -NoProfile -ExecutionPolicy Bypass -File {quoted}");
    if let Some(a) = arg {
        s.push(' ');
        s.push_str(a);
    }
    s
}

fn is_our_str(s: &str) -> bool {
    MARKERS.iter().chain(LEGACY_MARKERS.iter()).any(|m| s.contains(m))
}

fn is_legacy_str(s: &str) -> bool {
    LEGACY_MARKERS.iter().any(|m| s.contains(m))
}

/// settings.json still runs a Claude Overlay 2.x hook or status line.
fn has_legacy_wiring(settings: &Value) -> bool {
    let is_legacy = |v: Option<&Value>| v.and_then(Value::as_str).map_or(false, is_legacy_str);
    let status = is_legacy(settings.pointer("/statusLine/command"));
    let hooks = settings.get("hooks").and_then(Value::as_object).map_or(false, |events| {
        events
            .values()
            .filter_map(Value::as_array)
            .flatten()
            .filter_map(|entry| entry.get("hooks").and_then(Value::as_array))
            .flatten()
            .any(|h| is_legacy(h.get("command")))
    });
    status || hooks
}

fn is_our_command(cmd: &Value) -> bool {
    cmd.as_str().map(is_our_str).unwrap_or(false)
}

/// Remove our hook commands. A matcher entry keeps every other hook it holds (the user may
/// have added theirs to the same entry); only an entry left with no hooks at all goes.
fn strip_our_hooks(settings: &mut Value) {
    if let Some(hooks) = settings.get_mut("hooks").and_then(Value::as_object_mut) {
        let keys: Vec<String> = hooks.keys().cloned().collect();
        for k in keys {
            let Some(list) = hooks.get_mut(&k).and_then(Value::as_array_mut) else {
                continue;
            };
            let before = list.len();
            list.retain_mut(|entry| {
                let Some(hs) = entry.get_mut("hooks").and_then(Value::as_array_mut) else {
                    return true;
                };
                let had = hs.len();
                hs.retain(|h| !is_our_command(h.get("command").unwrap_or(&Value::Null)));
                !(hs.is_empty() && had > 0)
            });
            if list.is_empty() && before > 0 {
                hooks.remove(&k);
            }
        }
    }
    let empty = settings
        .get("hooks")
        .and_then(Value::as_object)
        .map(|o| o.is_empty())
        .unwrap_or(false);
    if empty {
        if let Some(o) = settings.as_object_mut() {
            o.remove("hooks");
        }
    }
}

fn push_hook(settings: &mut Value, event: &str, entry: Value) {
    let root = settings.as_object_mut().expect("settings is an object");
    let hooks = root.entry("hooks").or_insert_with(|| json!({}));
    if !hooks.is_object() {
        *hooks = json!({});
    }
    let list = hooks
        .as_object_mut()
        .unwrap()
        .entry(event)
        .or_insert_with(|| json!([]));
    if !list.is_array() {
        *list = json!([]);
    }
    list.as_array_mut().unwrap().push(entry);
}

/// A PowerShell single-quoted string. PowerShell also ends such a string at the typographic
/// quotes ‘ ’ ‚ ‛, so those are doubled too.
fn psq(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for c in s.chars() {
        if matches!(c, '\'' | '\u{2018}' | '\u{2019}' | '\u{201A}' | '\u{201B}') {
            out.push(c);
        }
        out.push(c);
    }
    out.push('\'');
    out
}

fn create_shortcut(exe: &Path) -> bool {
    let (Some(dir), Some(lnk)) = (startup_dir(), shortcut_path()) else {
        return false;
    };
    if !dir.exists() {
        return false;
    }
    let workdir = exe.parent().map(|p| p.to_path_buf()).unwrap_or_default();
    let script = format!(
        "$s = (New-Object -ComObject WScript.Shell).CreateShortcut({}); $s.TargetPath = {}; $s.Arguments = ''; $s.WorkingDirectory = {}; $s.WindowStyle = 7; $s.Description = 'HeadsUp notifications'; $s.Save()",
        psq(&lnk.to_string_lossy()),
        psq(&exe.to_string_lossy()),
        psq(&workdir.to_string_lossy())
    );
    util::powershell()
        .args(["-Command", &script])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

pub fn current_exe() -> PathBuf {
    std::env::current_exe().unwrap_or_else(|_| PathBuf::from("HeadsUp.exe"))
}

/// Install or repair everything (tray "Reinstall", `--install`).
pub fn install() -> Result<Vec<String>, String> {
    install_with(true)
}

/// The automatic install at start (first run, a new version or a moved exe). Unlike an
/// explicit install it does not bring back a Startup shortcut the user deleted.
pub fn install_auto() -> Result<Vec<String>, String> {
    install_with(false)
}

fn install_with(explicit: bool) -> Result<Vec<String>, String> {
    config::ensure_user_data();
    crate::hooks::write_scripts();
    // The hooks sign their requests with this key: make sure it exists with the hooks.
    let _ = util::secret();
    let mut lines = Vec::new();
    let exe = current_exe();

    let lock = util::lock_config();
    let mut cfg = match util::read_json(&util::config_path()) {
        Some(v) if v.is_object() => v,
        _ if util::config_path().exists() => {
            return Err("config.json could not be read, so nothing was changed".to_string());
        }
        _ => json!({}),
    };
    let first_install = cfg.get("installedVersion").is_none();
    // Switched off in the settings window: no install brings the shortcut back.
    let startup_off = cfg.get("startupDisabled").and_then(Value::as_bool) == Some(true);

    let settings_file = settings_path();
    let _ = std::fs::create_dir_all(claude_dir());
    let mut settings = match util::read_json(&settings_file) {
        Some(v) => v,
        None if settings_file.exists() => {
            return Err("settings.json could not be read, so it was not changed".to_string());
        }
        None => json!({}),
    };
    if !settings.is_object() {
        settings = json!({});
    }
    if settings_file.exists() && existing_backup().is_none() {
        let _ = std::fs::copy(&settings_file, backup_path());
    }

    // Strips Claude Overlay 2.x's hooks as well (MARKERS): an upgrade re-points them.
    strip_our_hooks(&mut settings);
    let hooks = util::hooks_dir();
    let send = hooks.join("headsup-send.ps1");
    push_hook(
        &mut settings,
        "Stop",
        json!({"hooks": [{"type": "command", "command": ps_command(&send, Some("stop")), "timeout": 15}]}),
    );
    push_hook(
        &mut settings,
        "Notification",
        json!({
            "matcher": NOTIFICATION_MATCHER,
            "hooks": [{"type": "command", "command": ps_command(&send, Some("notification")), "timeout": 15}]
        }),
    );
    // A turn that ended on a rate-limit error: a used-up usage limit is reported (limits.rs).
    push_hook(
        &mut settings,
        "StopFailure",
        json!({
            "matcher": "rate_limit",
            "hooks": [{"type": "command", "command": ps_command(&send, Some("stopfailure")), "timeout": 15}]
        }),
    );
    // Interactive: the hook waits up to interactive.waitMs for a click on the overlay and
    // returns the answer / decision. With no click it prints nothing and Claude Code's own
    // dialog proceeds.
    // Generous: a typed answer or an answer from the phone can take a while.
    let hook_timeout = expected_hook_timeout();
    push_hook(
        &mut settings,
        "PermissionRequest",
        json!({"hooks": [{"type": "command", "command": ps_command(&send, Some("permission")), "timeout": hook_timeout}]}),
    );
    push_hook(
        &mut settings,
        "PreToolUse",
        json!({
            "matcher": "AskUserQuestion",
            "hooks": [{"type": "command", "command": ps_command(&send, Some("question")), "timeout": hook_timeout}]
        }),
    );

    // Status line: wrap whatever the user already had so their status line keeps working.
    let current = settings
        .get("statusLine")
        .filter(|s| s.get("type").and_then(Value::as_str) == Some("command"))
        .and_then(|s| s.get("command"))
        .and_then(Value::as_str)
        .map(String::from);
    match &current {
        Some(c) if is_our_str(c) => {
            // Already wrapped (by this version or by Claude Overlay 2.x, whose wrapper is
            // never taken for the user's own command). Make sure the stored original is a
            // real user command; older versions could store one of our own wrappers here. The
            // backup taken on the very first install holds the user's real status line.
            let stored_ok = cfg
                .get("originalStatusLineCommand")
                .and_then(Value::as_str)
                .map(|s| !is_our_str(s))
                .unwrap_or(false);
            if !stored_ok {
                let from_backup = existing_backup()
                    .and_then(|p| util::read_json(&p))
                    .and_then(|b| b.get("statusLine")?.get("command")?.as_str().map(String::from))
                    .filter(|c| !is_our_str(c));
                cfg["originalStatusLineCommand"] = from_backup.map(Value::String).unwrap_or(Value::Null);
            }
        }
        _ => {
            cfg["originalStatusLineCommand"] =
                current.clone().map(Value::String).unwrap_or(Value::Null);
        }
    }
    let mut status_line = settings
        .get("statusLine")
        .cloned()
        .filter(Value::is_object)
        .unwrap_or_else(|| json!({}));
    status_line["type"] = json!("command");
    status_line["command"] = json!(ps_command(&hooks.join("headsup-statusline.ps1"), None));
    settings["statusLine"] = status_line;

    util::write_json(&settings_file, &settings).map_err(|e| format!("settings.json: {e}"))?;
    lines.push(format!("Claude Code hooks installed: {}", settings_file.display()));
    if cfg.get("originalStatusLineCommand").and_then(Value::as_str).is_some() {
        lines.push("Existing status line kept and wrapped.".to_string());
    }

    migrate_codex_args(&mut cfg);
    cfg["overlayExe"] = json!(exe.to_string_lossy());
    cfg["overlayArgs"] = json!([]);
    cfg["installedAt"] = json!(util::now_iso());
    cfg["installedVersion"] = json!(INSTALL_VERSION);
    util::write_json(&util::config_path(), &cfg).map_err(|e| format!("config.json: {e}"))?;
    drop(lock);
    // Hooks are in place again: hooks may start the overlay.
    let _ = std::fs::remove_file(util::marker_path(util::MARKER_UNINSTALLED));
    if let Some(line) = repoint_codex() {
        lines.push(line);
    }

    // Claude Overlay 2.x's shortcut counts as the user's choice to start at logon; it starts
    // the old exe, so it goes either way. When ours then cannot be created, that choice is
    // kept (startupWanted) and the shortcut is tried again on every start until it exists.
    let legacy_shortcut = legacy_shortcut_path().filter(|p| p.exists());
    let wanted = startup_wanted();
    let shortcut_exists = shortcut_path().map_or(false, |p| p.exists()) || legacy_shortcut.is_some() || wanted;
    if let Some(old) = &legacy_shortcut {
        let _ = std::fs::remove_file(old);
    }
    if shortcut_exists || ((explicit || first_install) && !startup_off) {
        let created = create_shortcut(&exe);
        if created {
            lines.push("Added to Windows startup.".to_string());
        } else {
            lines.push("Could not create the startup shortcut.".to_string());
        }
        let keep = !created && (wanted || legacy_shortcut.is_some());
        if keep != wanted {
            set_flags(&[("startupWanted", keep)]);
        }
    } else {
        lines.push("Startup shortcut not restored (it was removed).".to_string());
    }
    Ok(lines)
}

/// A Startup shortcut is owed: Claude Overlay 2.x's was replaced, and ours could not be created.
fn startup_wanted() -> bool {
    util::read_json(&util::config_path()).and_then(|c| c.get("startupWanted")?.as_bool()) == Some(true)
}

/// Set (true) or remove (false) top-level flags of config.json. Read fresh: install_codex may
/// have changed config.json since the install read it.
fn set_flags(flags: &[(&str, bool)]) {
    let _lock = util::lock_config();
    let Some(mut cfg) = util::read_json(&util::config_path()).filter(Value::is_object) else {
        return;
    };
    let Some(o) = cfg.as_object_mut() else { return };
    let mut changed = false;
    for &(key, on) in flags {
        changed |= if on { o.insert(key.to_string(), json!(true)) != Some(json!(true)) } else { o.remove(key).is_some() };
    }
    if !changed {
        return;
    }
    if let Err(e) = util::write_json(&util::config_path(), &cfg) {
        util::log_line(&["install-warn", &format!("config.json: {e}")]);
    }
}

/// "Start with Windows" (settings window): our Startup shortcut is there.
pub fn startup_enabled() -> bool {
    shortcut_path().is_some_and(|p| p.exists())
}

/// Settings window: add or remove the Startup shortcut (Claude Overlay 2.x's goes too when it
/// is switched off). Either way the user has decided: no shortcut is owed any more, and one
/// switched off stays off through Reinstall hooks and `--install` (startupDisabled).
pub fn set_startup(on: bool, t: &crate::i18n::Strings) -> Result<(), String> {
    if on {
        if !startup_enabled() && !create_shortcut(&current_exe()) {
            return Err(t.startup_create_failed.to_string());
        }
    } else {
        for lnk in [shortcut_path(), legacy_shortcut_path()].into_iter().flatten().filter(|p| p.exists()) {
            std::fs::remove_file(&lnk).map_err(|e| format!("{} ({e})", t.startup_remove_failed))?;
        }
    }
    set_flags(&[("startupWanted", false), ("startupDisabled", !on)]);
    util::log_line(&["startup", if on { "on" } else { "off" }]);
    Ok(())
}

/// Self-heal at start: a Startup shortcut still owed (startupWanted) is created now.
pub fn retry_startup_shortcut() {
    if !startup_wanted() {
        return;
    }
    if shortcut_path().map_or(false, |p| p.exists()) || create_shortcut(&current_exe()) {
        set_flags(&[("startupWanted", false)]);
        util::log_line(&["install-repair", "startup shortcut created"]);
    }
}

/// A Codex notify of ours that runs another script (Claude Overlay 2.x's, or a hooks folder
/// that moved) is pointed at this one; the user's earlier notify stays stored for disable.
fn repoint_codex() -> Option<String> {
    let Ok(Some((_, text))) = read_codex_config(&codex_config_path()) else {
        return None;
    };
    if !codex_needs_repoint(&text, &codex_notify_line()) {
        return None;
    }
    Some(match install_codex() {
        Ok(m) => m,
        Err(e) => format!("Codex notify not updated: {e}"),
    })
}

/// `want`: our notify line as this install writes it.
fn codex_needs_repoint(text: &str, want: &str) -> bool {
    codex_notify_stmts(text).map_or(false, |stmts| {
        stmts.iter().any(|s| {
            let stmt = text[s.start..s.end].trim();
            is_our_codex(stmt) && stmt != want
        })
    })
}

/// Remove everything `install` added: hooks, status line wrapper, Codex notify, Startup
/// shortcut and the overlayExe the hooks start. The caller then quits the overlay; the
/// `uninstalled` marker keeps hooks still loaded in open Claude Code sessions (and the
/// status line) from starting it again.
pub fn uninstall() -> Vec<String> {
    let mut lines = Vec::new();
    // First, so a hook that fires meanwhile does not start the overlay again. Claude Overlay
    // 2.x's hooks never read that marker: a 2.x folder left behind loses what they start.
    let _ = std::fs::write(util::marker_path(util::MARKER_UNINSTALLED), util::now_iso());
    util::disarm_legacy_data();
    if codex_installed() {
        lines.push(uninstall_codex());
    }
    let _lock = util::lock_config();
    let mut cfg = match util::read_json(&util::config_path()) {
        Some(v) if v.is_object() => v,
        _ if util::config_path().exists() => {
            lines.push("config.json could not be read, so the hooks were not changed.".to_string());
            return lines;
        }
        _ => json!({}),
    };
    let settings_file = settings_path();
    match util::read_json(&settings_file) {
        Some(mut settings) if settings.is_object() => {
            strip_our_hooks(&mut settings);
            let ours = settings
                .get("statusLine")
                .and_then(|s| s.get("command"))
                .map(is_our_command)
                .unwrap_or(false);
            if ours {
                match cfg.get("originalStatusLineCommand").and_then(Value::as_str) {
                    Some(orig) => settings["statusLine"]["command"] = json!(orig),
                    None => {
                        if let Some(o) = settings.as_object_mut() {
                            o.remove("statusLine");
                        }
                    }
                }
            }
            match util::write_json(&settings_file, &settings) {
                Ok(()) => {
                    lines.push("Hooks and status line restored.".to_string());
                    if let Some(o) = cfg.as_object_mut() {
                        o.remove("originalStatusLineCommand");
                    }
                }
                Err(e) => lines.push(format!("settings.json could not be written: {e}")),
            }
        }
        _ if settings_file.exists() => {
            lines.push("settings.json could not be read, so it was not changed.".to_string());
        }
        _ => {}
    }
    for lnk in [shortcut_path(), legacy_shortcut_path()].into_iter().flatten() {
        if std::fs::remove_file(&lnk).is_ok() {
            lines.push("Startup shortcut removed.".to_string());
        }
    }
    if let Some(o) = cfg.as_object_mut() {
        for k in ["installedAt", "installedVersion", "overlayExe", "overlayArgs", "startupWanted"] {
            o.remove(k);
        }
    }
    let _ = util::write_json(&util::config_path(), &cfg);
    lines
}

/// Self-heal at start: raise the timeout of our question / permission hooks when the card
/// hold grew (phone.holdMs). Never adds back a hook the user removed. True when
/// settings.json changed.
pub fn repair_hook_timeouts() -> bool {
    let path = settings_path();
    let Some(mut settings) = util::read_json(&path) else {
        return false;
    };
    let want = expected_hook_timeout();
    let mut changed = false;
    for event in ["PreToolUse", "PermissionRequest"] {
        let Some(entries) = settings.pointer_mut(&format!("/hooks/{event}")).and_then(Value::as_array_mut) else {
            continue;
        };
        for entry in entries {
            let Some(hs) = entry.get_mut("hooks").and_then(Value::as_array_mut) else {
                continue;
            };
            for h in hs.iter_mut().filter_map(Value::as_object_mut) {
                let ours = h.get("command").map(is_our_command).unwrap_or(false);
                if ours && h.get("timeout").and_then(Value::as_u64).unwrap_or(0) < want {
                    h.insert("timeout".to_string(), json!(want));
                    changed = true;
                }
            }
        }
    }
    if changed {
        match util::write_json(&path, &settings) {
            Ok(()) => util::log_line(&["install-repair", "hook timeouts raised"]),
            Err(e) => {
                util::log_line(&["install-repair", &format!("settings.json: {e}")]);
                return false;
            }
        }
    }
    changed
}

// ---------------------------------------------------------------- Codex CLI (optional)

fn codex_config_path() -> PathBuf {
    let home = std::env::var_os("USERPROFILE")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    home.join(".codex").join("config.toml")
}

fn codex_script() -> PathBuf {
    util::hooks_dir().join(CODEX_SCRIPT)
}

fn codex_notify_line() -> String {
    format!(
        "notify = [\"powershell\", \"-NoProfile\", \"-ExecutionPolicy\", \"Bypass\", \"-File\", \"{}\"]",
        fwd(&codex_script()).replace('\\', "\\\\").replace('"', "\\\"")
    )
}

fn is_codex_notify_line(line: &str) -> bool {
    let t = line.trim_start();
    t.starts_with("notify") && t[6..].trim_start().starts_with('=')
}

/// config.toml as text: None when it does not exist. Any other read error (a file that is
/// not UTF-8, a sharing violation) is an error, so the file is never rewritten from a
/// guess. A leading UTF-8 BOM is split off (true) and written back as it was.
fn read_codex_config(path: &Path) -> Result<Option<(bool, String)>, String> {
    match std::fs::read_to_string(path) {
        Ok(s) => Ok(Some(match s.strip_prefix('\u{feff}') {
            Some(rest) => (true, rest.to_string()),
            None => (false, s),
        })),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("{} could not be read ({e}), so it was not changed", path.display())),
    }
}

pub fn codex_installed() -> bool {
    matches!(
        read_codex_config(&codex_config_path()),
        Ok(Some((_, text))) if text.lines().any(|l| is_codex_notify_line(l) && is_our_codex(l))
    )
}

/// Byte range of one top-level `notify = ...` statement: its whole lines, line breaks
/// included (a multi-line array spans several).
struct TomlStmt {
    start: usize,
    end: usize,
}

/// The top-level `notify` statements of a TOML document (those before the first `[table]`
/// header). No TOML crate: strings, comments and bracket depth are followed, so a
/// multi-line array or a `[` inside a string is handled. Err when the top level cannot be
/// followed (an unterminated string or array, a stray `]`): the file is then left alone.
fn codex_notify_stmts(text: &str) -> Result<Vec<TomlStmt>, String> {
    let mut out = Vec::new();
    let mut depth: i32 = 0;
    let mut ml: Option<u8> = None;
    let mut cur: Option<usize> = None;
    let mut pos = 0;
    for line in text.split_inclusive('\n') {
        let start = pos;
        pos += line.len();
        if depth == 0 && ml.is_none() {
            let t = line.trim_start();
            if t.starts_with('[') {
                break;
            }
            if is_codex_notify_line(t) {
                cur = Some(start);
            }
        }
        scan_toml_line(line.as_bytes(), &mut depth, &mut ml)?;
        if depth < 0 {
            return Err("unbalanced ] at the top level".to_string());
        }
        if depth == 0 && ml.is_none() {
            if let Some(s) = cur.take() {
                out.push(TomlStmt { start: s, end: pos });
            }
        }
    }
    if depth != 0 || ml.is_some() {
        return Err("unterminated array or string at the top level".to_string());
    }
    Ok(out)
}

/// Follow one line: `depth` counts open [ / {, `ml` is the quote byte of an open
/// multi-line string. Byte-wise: every delimiter is ASCII, so UTF-8 text cannot confuse it.
fn scan_toml_line(b: &[u8], depth: &mut i32, ml: &mut Option<u8>) -> Result<(), String> {
    let triple = |i: usize, q: u8| b.len() >= i + 3 && b[i] == q && b[i + 1] == q && b[i + 2] == q;
    let mut i = 0;
    while i < b.len() {
        if let Some(q) = *ml {
            if q == b'"' && b[i] == b'\\' {
                i += 2;
            } else if triple(i, q) {
                // Up to two more quotes right before the end belong to the string.
                i += 3;
                let mut extra = 0;
                while extra < 2 && i < b.len() && b[i] == q {
                    i += 1;
                    extra += 1;
                }
                *ml = None;
            } else {
                i += 1;
            }
            continue;
        }
        match b[i] {
            b'#' => break,
            q @ (b'"' | b'\'') => {
                if triple(i, q) {
                    *ml = Some(q);
                    i += 3;
                    continue;
                }
                i += 1;
                loop {
                    if i >= b.len() || b[i] == b'\n' || b[i] == b'\r' {
                        return Err("unterminated string".to_string());
                    }
                    if q == b'"' && b[i] == b'\\' {
                        i += 2;
                        continue;
                    }
                    i += 1;
                    if b[i - 1] == q {
                        break;
                    }
                }
            }
            b'[' | b'{' => {
                *depth += 1;
                i += 1;
            }
            b']' | b'}' => {
                *depth -= 1;
                i += 1;
            }
            _ => i += 1,
        }
    }
    Ok(())
}

/// The strings of a `notify = [ ... ]` statement, decoded (basic and literal strings, single-
/// and multi-line). None when the value is not a plain array of strings.
fn toml_string_array(stmt: &str) -> Option<Vec<String>> {
    let c: Vec<char> = stmt.split_once('=')?.1.chars().collect();
    let mut i = 0;
    let skip = |i: &mut usize| {
        while *i < c.len() {
            if c[*i].is_whitespace() {
                *i += 1;
            } else if c[*i] == '#' {
                while *i < c.len() && c[*i] != '\n' {
                    *i += 1;
                }
            } else {
                break;
            }
        }
    };
    skip(&mut i);
    if c.get(i) != Some(&'[') {
        return None;
    }
    i += 1;
    let mut out = Vec::new();
    loop {
        skip(&mut i);
        match c.get(i)? {
            ']' => {
                i += 1;
                break;
            }
            '"' | '\'' => out.push(toml_string(&c, &mut i)?),
            _ => return None,
        }
        skip(&mut i);
        match c.get(i)? {
            ',' => i += 1,
            ']' => {
                i += 1;
                break;
            }
            _ => return None,
        }
    }
    skip(&mut i);
    (i == c.len()).then_some(out)
}

/// One TOML string starting at `c[*i]` (a quote).
fn toml_string(c: &[char], i: &mut usize) -> Option<String> {
    let q = c[*i];
    let multi = c.get(*i + 1) == Some(&q) && c.get(*i + 2) == Some(&q);
    *i += if multi { 3 } else { 1 };
    if multi {
        // A line break right after the opening quotes is not part of the string.
        if c.get(*i) == Some(&'\r') && c.get(*i + 1) == Some(&'\n') {
            *i += 2;
        } else if c.get(*i) == Some(&'\n') {
            *i += 1;
        }
    }
    let mut s = String::new();
    loop {
        let ch = *c.get(*i)?;
        if ch == q {
            if !multi {
                *i += 1;
                return Some(s);
            }
            let run = c[*i..].iter().take_while(|&&x| x == q).count();
            if run >= 3 {
                // Up to two quotes before the closing three are content.
                for _ in 0..(run - 3).min(2) {
                    s.push(q);
                }
                *i += 3 + (run - 3).min(2);
                return Some(s);
            }
            for _ in 0..run {
                s.push(q);
            }
            *i += run;
            continue;
        }
        if !multi && (ch == '\n' || ch == '\r') {
            return None;
        }
        if q == '"' && ch == '\\' {
            *i += 1;
            let e = *c.get(*i)?;
            *i += 1;
            match e {
                'b' => s.push('\u{8}'),
                't' => s.push('\t'),
                'n' => s.push('\n'),
                'f' => s.push('\u{c}'),
                'r' => s.push('\r'),
                'e' => s.push('\u{1b}'),
                '"' => s.push('"'),
                '\\' => s.push('\\'),
                'u' | 'U' => {
                    let n = if e == 'u' { 4 } else { 8 };
                    let hex: String = c.get(*i..*i + n)?.iter().collect();
                    s.push(char::from_u32(u32::from_str_radix(&hex, 16).ok()?)?);
                    *i += n;
                }
                // Line-ending backslash in a multi-line string: drop the break and the
                // whitespace that follows.
                w if multi && w.is_whitespace() => {
                    *i -= 1;
                    while c.get(*i).map_or(false, |x| x.is_whitespace()) {
                        *i += 1;
                    }
                }
                _ => return None,
            }
            continue;
        }
        s.push(ch);
        *i += 1;
    }
}

/// A stored original notify that is one complete statement (older versions could store a
/// lone `notify = [` from a multi-line array; restoring that would break config.toml).
fn is_complete_notify(stmt: &str) -> bool {
    let text = format!("{stmt}\n");
    matches!(codex_notify_stmts(&text), Ok(v) if v.len() == 1 && v[0].start == 0 && v[0].end == text.len())
}

/// Older versions stored only the TOML text of the chained notify; the Codex hook runs it
/// from its argument list (null: not a plain list of strings, so not chained).
fn migrate_codex_args(cfg: &mut Value) {
    if cfg.get("originalCodexNotifyArgs").is_some() {
        return;
    }
    let Some(orig) = cfg.get("originalCodexNotify").and_then(Value::as_str) else {
        return;
    };
    let args = toml_string_array(orig).map_or(Value::Null, |a| json!(a));
    cfg["originalCodexNotifyArgs"] = args;
}

/// Adds our `notify` command to ~/.codex/config.toml (top-level key, so it goes before any
/// `[table]`). A different existing `notify` is remembered and restored on disable.
pub fn install_codex() -> Result<String, String> {
    crate::hooks::write_scripts();
    let _ = util::secret();
    let path = codex_config_path();
    let (bom, text) = read_codex_config(&path)?.unwrap_or((false, String::new()));
    let stmts = codex_notify_stmts(&text)
        .map_err(|e| format!("{} could not be parsed ({e}), so it was not changed", path.display()))?;
    let lock = util::lock_config();
    let mut cfg = match util::read_json(&util::config_path()) {
        Some(v) if v.is_object() => v,
        _ if util::config_path().exists() => {
            return Err("config.json could not be read, so nothing was changed".to_string());
        }
        _ => json!({}),
    };
    let ours_present = stmts.iter().any(|s| is_our_codex(&text[s.start..s.end]));
    let theirs = stmts
        .iter()
        .map(|s| text[s.start..s.end].trim())
        .find(|s| !is_our_codex(s));
    match theirs {
        Some(orig) => {
            let args = toml_string_array(orig);
            if args.is_none() {
                util::log_line(&["codex-chain", "the existing notify is not a list of strings: restored on disable, not chained"]);
            }
            cfg["originalCodexNotify"] = json!(orig);
            cfg["originalCodexNotifyArgs"] = args.map_or(Value::Null, |a| json!(a));
        }
        None if !ours_present => {
            if let Some(o) = cfg.as_object_mut() {
                o.remove("originalCodexNotify");
                o.remove("originalCodexNotifyArgs");
            }
        }
        None => {}
    }
    // Saved before config.toml changes, so the original is never lost.
    util::write_json(&util::config_path(), &cfg).map_err(|e| format!("config.json: {e}"))?;
    drop(lock);

    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let [old_backup, backup] = codex_backups(&path);
    if path.exists() && !backup.exists() && !old_backup.exists() {
        let _ = std::fs::copy(&path, &backup);
    }
    let out = codex_enabled_text(bom, &text, &stmts);
    util::write_atomic(&path, out.as_bytes()).map_err(|e| e.to_string())?;
    Ok(format!("Codex notify set in {}", path.display()))
}

/// Backups of config.toml from before the first enable: Claude Overlay 2.x's name (kept: it
/// is the older copy) and ours. Only one is ever written.
fn codex_backups(path: &Path) -> [PathBuf; 2] {
    [path.with_extension("toml.claude-overlay.bak"), path.with_extension("toml.headsup.bak")]
}

/// config.toml with our notify first and every other top-level `notify` taken out.
fn codex_enabled_text(bom: bool, text: &str, stmts: &[TomlStmt]) -> String {
    let nl = if text.contains("\r\n") { "\r\n" } else { "\n" };
    let mut out = String::with_capacity(text.len() + 200);
    if bom {
        out.push('\u{feff}');
    }
    out.push_str(&codex_notify_line());
    out.push_str(nl);
    let mut last = 0;
    for s in stmts {
        out.push_str(&text[last..s.start]);
        last = s.end;
    }
    out.push_str(&text[last..]);
    if !out.ends_with('\n') {
        out.push_str(nl);
    }
    out
}

/// config.toml without our notify; `restore` (the user's earlier one) takes its place.
fn codex_disabled_text(bom: bool, text: &str, ours: &[&TomlStmt], restore: Option<&str>) -> String {
    let nl = if text.contains("\r\n") { "\r\n" } else { "\n" };
    let mut out = String::with_capacity(text.len());
    if bom {
        out.push('\u{feff}');
    }
    let mut last = 0;
    for (n, s) in ours.iter().enumerate() {
        out.push_str(&text[last..s.start]);
        if n == 0 {
            if let Some(o) = restore {
                out.push_str(o);
                out.push_str(nl);
            }
        }
        last = s.end;
    }
    out.push_str(&text[last..]);
    out
}

pub fn uninstall_codex() -> String {
    let path = codex_config_path();
    let (bom, text) = match read_codex_config(&path) {
        Ok(Some(t)) => t,
        Ok(None) => return "Codex config not found.".to_string(),
        Err(e) => return format!("{e}."),
    };
    let stmts = match codex_notify_stmts(&text) {
        Ok(s) => s,
        Err(e) => return format!("{} could not be parsed ({e}), so it was not changed.", path.display()),
    };
    let ours: Vec<&TomlStmt> = stmts.iter().filter(|s| is_our_codex(&text[s.start..s.end])).collect();
    if ours.is_empty() {
        return "Codex notify was not set by HeadsUp.".to_string();
    }
    let lock = util::lock_config();
    let mut cfg = match util::read_json(&util::config_path()) {
        Some(v) if v.is_object() => v,
        _ if util::config_path().exists() => {
            return "config.json could not be read, so the Codex config was not changed.".to_string();
        }
        _ => json!({}),
    };
    // Bring back the user's notify, unless they set another one meanwhile (two would be a
    // duplicate key) or the stored one is not a complete statement.
    let stored = cfg.get("originalCodexNotify").and_then(Value::as_str).map(String::from);
    let restore = stored
        .clone()
        .filter(|o| ours.len() == stmts.len() && is_complete_notify(o));
    let out = codex_disabled_text(bom, &text, &ours, restore.as_deref());
    if let Err(e) = util::write_atomic(&path, out.as_bytes()) {
        return format!("{} could not be written: {e}", path.display());
    }
    if let Some(o) = cfg.as_object_mut() {
        o.remove("originalCodexNotify");
        o.remove("originalCodexNotifyArgs");
    }
    let _ = util::write_json(&util::config_path(), &cfg);
    drop(lock);
    match (restore.is_some(), stored.is_some()) {
        (true, _) => "Codex notify removed; your earlier notify is back.".to_string(),
        (false, true) => match codex_backups(&path).into_iter().find(|b| b.exists()) {
            Some(b) => format!(
                "Codex notify removed. Your earlier notify could not be restored; {} has it.",
                b.file_name().unwrap_or_default().to_string_lossy()
            ),
            None => "Codex notify removed. Your earlier notify could not be restored.".to_string(),
        },
        (false, false) => "Codex notify removed.".to_string(),
    }
}

/// Seconds the question/permission hooks may run: longer than the longest card hold plus
/// the time a typed answer may take, so Claude Code never cancels a hook that is waiting.
pub fn expected_hook_timeout() -> u64 {
    let merged = config::load();
    let hold = config::get_u64(&merged, "phone.holdMs", 480000).max(config::get_u64(&merged, "interactive.waitMs", 20000));
    (hold + 999) / 1000 + 260
}

/// This exe, at this install version, is the one the hooks start, and nothing still points at
/// Claude Overlay 2.x: its hooks, status line or Startup shortcut (put back when the old exe
/// ran its own installer after the upgrade), or a Codex notify of ours naming another script.
/// Hooks the user removed afterwards are not checked: a self-heal never brings them back.
pub fn is_installed() -> bool {
    let Some(cfg) = util::read_json(&util::config_path()) else {
        return false;
    };
    let version = cfg.get("installedVersion").and_then(Value::as_u64);
    let exe = cfg.get("overlayExe").and_then(Value::as_str).unwrap_or("");
    if version != Some(INSTALL_VERSION) || exe != current_exe().to_string_lossy() {
        return false;
    }
    let legacy_hooks = util::read_json(&settings_path()).map_or(false, |s| has_legacy_wiring(&s));
    let legacy_shortcut = legacy_shortcut_path().map_or(false, |p| p.exists());
    // Only a config.toml that reads and parses: the install could not fix any other, and
    // would then run (with its banner) on every start.
    let codex_stale = matches!(
        read_codex_config(&codex_config_path()),
        Ok(Some((_, text))) if codex_needs_repoint(&text, &codex_notify_line())
    );
    !(legacy_hooks || legacy_shortcut || codex_stale)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn notify_texts(text: &str) -> Vec<String> {
        codex_notify_stmts(text).unwrap().iter().map(|s| text[s.start..s.end].to_string()).collect()
    }

    #[test]
    fn toml_notify_single_and_multi_line() {
        let t = "model = \"o3\"\nnotify = [ \"a.exe\", \"turn-ended\" ]\n[tui]\nnotify = true\n";
        assert_eq!(notify_texts(t), vec!["notify = [ \"a.exe\", \"turn-ended\" ]\n"]);
        let t = "notify = [\n  \"python3\", # comment ]\n  '/x/[y].py',\n]\nmodel = \"o3\"\n";
        assert_eq!(notify_texts(t), vec!["notify = [\n  \"python3\", # comment ]\n  '/x/[y].py',\n]\n"]);
        // A `[` at the start of a line inside a multi-line string is not a table header.
        let t = "x = \"\"\"\n[not a table]\n\"\"\"\nnotify = [\"a\"]\n";
        assert_eq!(notify_texts(t), vec!["notify = [\"a\"]\n"]);
        assert!(codex_notify_stmts("notify = [\n\"a\",\n").is_err());
        assert!(codex_notify_stmts("notify = [\"a\"]\n  \"b\",\n]\n").is_err());
        assert!(codex_notify_stmts("x = \"open\n").is_err());
    }

    #[test]
    fn toml_strings_decoded() {
        assert_eq!(
            toml_string_array("notify = [\"C:\\\\a b\\\\x.exe\", 'C:\\lit', \"q\\\"\\u00e7\"] # c").unwrap(),
            vec!["C:\\a b\\x.exe", "C:\\lit", "q\"ç"]
        );
        assert_eq!(toml_string_array("notify = [\n  \"\"\"\nml\"\"\",\n  '''x''',\n]").unwrap(), vec!["ml", "x"]);
        assert_eq!(toml_string_array("notify = []").unwrap(), Vec::<String>::new());
        assert!(toml_string_array("notify = [1, 2]").is_none());
        assert!(toml_string_array("notify = [").is_none());
        assert!(toml_string_array("notify = \"x\"").is_none());
    }

    #[test]
    fn codex_enable_disable_round_trip() {
        // A multi-line notify with CRLF line breaks: enabled, then disabled, gives the file back.
        let text = "model = \"o3\"\r\nnotify = [\r\n  \"python3\",\r\n  \"n.py\",\r\n]\r\n[tui]\r\nx = 1\r\n";
        let stmts = codex_notify_stmts(text).unwrap();
        let orig = text[stmts[0].start..stmts[0].end].trim().to_string();
        assert_eq!(toml_string_array(&orig).unwrap(), vec!["python3", "n.py"]);
        let on = codex_enabled_text(true, text, &stmts);
        assert!(on.starts_with("\u{feff}notify = [\"powershell\""));
        assert!(on.ends_with("\r\nmodel = \"o3\"\r\n[tui]\r\nx = 1\r\n"), "{on}");
        let body = on.trim_start_matches('\u{feff}');
        let now = codex_notify_stmts(body).unwrap();
        assert_eq!(now.len(), 1);
        let ours: Vec<&TomlStmt> = now.iter().collect();
        assert!(is_complete_notify(&orig));
        let off = codex_disabled_text(false, body, &ours, Some(&orig));
        assert_eq!(off, "notify = [\r\n  \"python3\",\r\n  \"n.py\",\r\n]\r\nmodel = \"o3\"\r\n[tui]\r\nx = 1\r\n");
        assert!(codex_notify_stmts(&off).is_ok());
    }

    #[test]
    fn complete_notify_check() {
        assert!(is_complete_notify("notify = [ \"a.exe\", \"turn-ended\" ]"));
        assert!(is_complete_notify("notify = [\n  \"a\",\n]"));
        assert!(!is_complete_notify("notify = ["));
        assert!(!is_complete_notify("model = \"x\""));
    }

    #[test]
    fn strip_keeps_user_hooks_in_shared_entry() {
        // Claude Overlay 2.x's hook and ours are both stripped; the user's own stay.
        let old = "powershell -NoProfile -ExecutionPolicy Bypass -File \"C:/Users/u/AppData/Roaming/claude-notification/hooks/claude-overlay-send.ps1\" stop";
        let ours = "powershell -NoProfile -ExecutionPolicy Bypass -File \"C:/Users/u/AppData/Roaming/HeadsUp/hooks/headsup-send.ps1\" permission";
        let failure = ps_command(Path::new("C:\\Users\\u\\.headsup\\hooks\\headsup-send.ps1"), Some("stopfailure"));
        let mut s = json!({"hooks": {
            "Stop": [{"hooks": [{"type": "command", "command": old}, {"type": "command", "command": "my-stop.sh"}]}],
            "PermissionRequest": [{"hooks": [{"type": "command", "command": ours}]}],
            "StopFailure": [{"matcher": "rate_limit", "hooks": [{"type": "command", "command": failure}]}],
            "PreToolUse": [{"matcher": "Bash", "hooks": [
                {"type": "command", "command": "bash ~/hooks/claude-overlay-mine.sh"},
                {"type": "command", "command": "bash ~/hooks/headsup-mine.sh"}
            ]}]
        }});
        strip_our_hooks(&mut s);
        assert_eq!(s["hooks"]["Stop"], json!([{"hooks": [{"type": "command", "command": "my-stop.sh"}]}]));
        assert!(s["hooks"].get("PermissionRequest").is_none());
        assert!(s["hooks"].get("StopFailure").is_none());
        assert_eq!(s["hooks"]["PreToolUse"][0]["hooks"][0]["command"], "bash ~/hooks/claude-overlay-mine.sh");
        assert_eq!(s["hooks"]["PreToolUse"][0]["hooks"][1]["command"], "bash ~/hooks/headsup-mine.sh");
        // The 2.x status line wrapper is ours, never the user's original.
        assert!(is_our_str("powershell -NoProfile -ExecutionPolicy Bypass -File \"C:/Users/u/AppData/Roaming/claude-notification/hooks/claude-overlay-statusline.ps1\""));
        assert!(is_our_str("powershell -NoProfile -ExecutionPolicy Bypass -File \"C:/Users/u/AppData/Roaming/HeadsUp/hooks/headsup-statusline.ps1\""));
        assert!(!is_our_str("bash ~/.claude/statusline.sh"));
    }

    #[test]
    fn legacy_wiring_found() {
        let old = "powershell -NoProfile -ExecutionPolicy Bypass -File \"C:/Users/u/AppData/Roaming/claude-notification/hooks/claude-overlay-send.ps1\" stop";
        let ours = "powershell -NoProfile -ExecutionPolicy Bypass -File \"C:/Users/u/AppData/Roaming/HeadsUp/hooks/headsup-send.ps1\" stop";
        let ours_status = "powershell -NoProfile -ExecutionPolicy Bypass -File \"C:/Users/u/AppData/Roaming/HeadsUp/hooks/headsup-statusline.ps1\"";
        let old_status = "powershell -NoProfile -ExecutionPolicy Bypass -File \"C:/Users/u/AppData/Roaming/claude-notification/hooks/claude-overlay-statusline.ps1\"";
        let clean = json!({
            "hooks": {"Stop": [{"hooks": [{"type": "command", "command": ours}, {"type": "command", "command": "bash ~/hooks/claude-overlay-mine.sh"}]}]},
            "statusLine": {"type": "command", "command": ours_status}
        });
        assert!(!has_legacy_wiring(&clean));
        let mut hooks_back = clean.clone();
        hooks_back["hooks"]["Stop"].as_array_mut().unwrap().push(json!({"hooks": [{"type": "command", "command": old}]}));
        assert!(has_legacy_wiring(&hooks_back));
        let mut status_back = clean.clone();
        status_back["statusLine"]["command"] = json!(old_status);
        assert!(has_legacy_wiring(&status_back));
        assert!(!has_legacy_wiring(&json!({"hooks": "x", "statusLine": 3})));
    }

    #[test]
    fn codex_old_notify_is_repointed() {
        let old = "notify = [\"powershell\", \"-NoProfile\", \"-ExecutionPolicy\", \"Bypass\", \"-File\", \"C:/Users/u/AppData/Roaming/claude-notification/hooks/claude-overlay-codex.ps1\"]\n[tui]\n";
        let want = "notify = [\"powershell\", \"-NoProfile\", \"-ExecutionPolicy\", \"Bypass\", \"-File\", \"C:/Users/u/AppData/Roaming/HeadsUp/hooks/headsup-codex.ps1\"]";
        assert!(codex_needs_repoint(old, want));
        assert!(!codex_needs_repoint(&format!("{want}\n[tui]\n"), want));
        // Somebody else's notify is not ours to touch.
        assert!(!codex_needs_repoint("notify = [\"python3\", \"n.py\"]\n", want));
        // The rewrite leaves one notify, ours, with the new script name.
        let now = codex_enabled_text(false, old, &codex_notify_stmts(old).unwrap());
        assert_eq!(codex_notify_stmts(&now).unwrap().len(), 1);
        assert!(now.contains("headsup-codex.ps1") && !now.contains("claude-overlay-codex.ps1"));
    }

    #[test]
    fn quoting() {
        assert_eq!(psq("a'b\u{2019}c"), "'a''b\u{2019}\u{2019}c'");
        assert_eq!(
            ps_command(Path::new("C:\\Users\\a$b\\h.ps1"), Some("stop")),
            "powershell -NoProfile -ExecutionPolicy Bypass -File 'C:/Users/a$b/h.ps1' stop"
        );
        assert_eq!(
            ps_command(Path::new("C:\\Users\\o'b\\h.ps1"), None),
            "powershell -NoProfile -ExecutionPolicy Bypass -File \"C:/Users/o'b/h.ps1\""
        );
    }
}
