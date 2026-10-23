//! Remote sessions from the phone. Over the paired Telegram chat the user can start a new
//! Claude Code session in a project the PC already knows, or send a message to an existing
//! session. The overlay runs `claude -p` headless (stream-json), relays progress and the
//! result to Telegram, and the existing hooks send permission requests to the phone.
//!
//! Safety: off by default (`remote.enabled`) and only usable once a chat is paired; only the
//! paired chat is obeyed; only projects that already have Claude Code transcripts (or are
//! listed in `remote.projects`) can be used and the working directory always comes from that
//! list, never from message text; the prompt is passed on stdin, never through a shell; the
//! permission mode is default, plan or acceptEdits (acceptEdits applies file edits without
//! asking; auto and bypassPermissions are refused); a run gets the user's persistent
//! environment, not that of whichever session started the overlay, and lives in a Job Object
//! so /stop and the overlay's exit end its whole process tree; nothing runs while the overlay
//! itself is elevated; one run at a time with a watchdog. Everything sent to the chat is
//! masked for secrets.
//!
//! While a usage limit is used up, a message is not run (it would only fail): it waits for the
//! reset (schedule.rs), and so does the continue of a run that ended on the limit. Those runs,
//! and the continues of sessions that stopped on the PC, go through `start` like any other.
use crate::{config, events, mask, telegram, util, Shared};
use serde_json::{json, Map, Value};
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::windows::io::AsRawHandle;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::Duration;
use tauri::AppHandle;
use windows_sys::Win32::Foundation::{CloseHandle, ERROR_MORE_DATA, HANDLE};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation, SetInformationJobObject,
    TerminateJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
};
use windows_sys::Win32::System::Registry::{
    RegCloseKey, RegEnumValueW, RegOpenKeyExW, HKEY, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, KEY_READ, REG_EXPAND_SZ,
    REG_SZ,
};

/// Modes in which Claude still asks before running commands. auto and bypassPermissions (and
/// dontAsk) approve on their own while nobody is at the PC.
const ALLOWED_MODES: [&str; 3] = ["default", "plan", "acceptEdits"];
/// Telegram allows 4096 characters; leave room for markup added around a chunk.
const CHUNK_MAX: usize = 3300;

#[derive(Clone, Debug)]
pub struct Project {
    pub name: String,
    pub cwd: String,
}

#[derive(Clone, Debug)]
pub struct SessionInfo {
    pub id: String,
    pub cwd: String,
    pub project: String,
    pub title: String,
}

#[derive(Clone, Debug)]
pub enum Target {
    New { cwd: String, project: String },
    Resume { id: String, cwd: String, project: String, title: String },
}

/// Who asked for a run.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Origin {
    /// A message from the phone, sent now.
    Phone,
    /// A message from the phone that waited for a usage limit to reset.
    Queued,
    /// schedule::CONTINUE_PROMPT at a usage-limit reset: a session of a remote run (`pc` false)
    /// or one that stopped on the PC (`pc` true; it reports to the phone only when the phone is
    /// set up to answer). Such a run is never continued again when it hits the limit itself.
    Continue { pc: bool },
}

/// What `start` did.
pub enum Start {
    Started,
    /// Another run is going on (only the phone is told, for a message from the phone).
    Busy,
    /// Not run, and why (already said on the phone where it reports).
    Refused(String),
}

pub struct Running {
    pub pid: u32,
    pub seq: u64,
    pub session_id: Option<String>,
    pub project: String,
    pub started_ms: i64,
    pub stop_requested: bool,
    /// The result has been sent; the process may still be finishing background work.
    pub result_sent: bool,
    pub activity: String,
    pub status_mid: Option<i64>,
    /// The claude process has exited (its pid may be reused: never kill it again).
    pub exited: bool,
    /// The run's Job Object (None if it could not be created: taskkill is used then).
    pub job: Option<Arc<Job>>,
    /// It continues a session of the PC at a usage-limit reset (needs no remote sessions).
    pub pc_continue: bool,
}

/// A Job Object holding one run's process tree. With KILL_ON_JOB_CLOSE every process still
/// in it ends when the last handle closes: when the run is over, or when the overlay exits or
/// crashes. No headless claude, or anything it started, outlives its run.
pub struct Job(HANDLE);

// The handle is only passed to Win32 calls, which may come from any thread.
unsafe impl Send for Job {}
unsafe impl Sync for Job {}

impl Job {
    fn for_child(child: &std::process::Child) -> Option<Job> {
        unsafe {
            let h = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            if h.is_null() {
                return None;
            }
            let job = Job(h);
            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            let ok = SetInformationJobObject(
                h,
                JobObjectExtendedLimitInformation,
                &info as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION as *const core::ffi::c_void,
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            ) != 0
                && AssignProcessToJobObject(h, child.as_raw_handle() as HANDLE) != 0;
            ok.then_some(job)
        }
    }

    /// End every process of the run, including children whose parent has already exited.
    fn kill(&self) {
        unsafe {
            TerminateJobObject(self.0, 1);
        }
    }
}

impl Drop for Job {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.0);
        }
    }
}

#[derive(Default)]
pub struct RemoteState {
    pub target: Option<Target>,
    /// Bumped whenever the user changes the target (/new pick, /sessions pick, /exit).
    pub target_gen: u64,
    pub projects_menu: Vec<Project>,
    pub sessions_menu: Vec<SessionInfo>,
    /// Bumped per menu sent; buttons carry it so taps on an old menu are refused.
    pub menu_gen: u64,
    pub running: Option<Running>,
    /// Prompt held back by the busy guard until the user confirms or cancels: (gen, target, text,
    /// sent now: Send anyway does not wait for a usage limit either).
    pub held: Option<(u64, Target, String, bool)>,
    pub held_gen: u64,
    /// session id -> unix ms when our own run on it ended (so the busy guard ignores it).
    pub own_runs: HashMap<String, i64>,
    /// session id -> seq of the newest run on it (so an old cleanup never removes a newer run).
    pub session_runs: HashMap<String, u64>,
    pub run_seq: u64,
    pub commands_registered: bool,
    /// This process set the bot's command menu (so it may delete it again).
    pub commands_set: bool,
    /// Turned off from the tray while config.json could not be written: stay off.
    pub force_off: bool,
    /// Last remote interaction (target picked, run started or ended), unix ms.
    pub last_touch_ms: i64,
    /// Telegram message ids sent by remote sessions (a reply to one of them is a prompt).
    pub msg_ids: HashSet<i64>,
}

impl RemoteState {
    /// Generations start from the clock so buttons from before a restart never match.
    pub fn new() -> Self {
        let seed = util::now_ms().max(0) as u64;
        RemoteState { menu_gen: seed, held_gen: seed, ..Default::default() }
    }
}

pub enum Toggle {
    On,
    Off,
    /// Off for this session, but config.json could not be saved.
    OffNotSaved,
    NeedPairing,
    ConfigUnreadable,
}

// ---------------------------------------------------------------- strings

pub struct S {
    pub disabled: &'static str,
    pub elevated: &'static str,
    pub pair_first: &'static str,
    pub help: &'static str,
    pub pick_project: &'static str,
    pub no_projects: &'static str,
    pub project_chosen: &'static str,
    pub pick_session: &'static str,
    pub no_sessions: &'static str,
    pub session_chosen: &'static str,
    pub no_target: &'static str,
    pub busy: &'static str,
    pub finishing: &'static str,
    pub working: &'static str,
    pub done: &'static str,
    pub failed: &'static str,
    pub stopped: &'static str,
    pub nothing_running: &'static str,
    pub active_warn: &'static str,
    pub send_anyway: &'static str,
    pub cancel: &'static str,
    pub cancelled: &'static str,
    pub expired: &'static str,
    pub claude_missing: &'static str,
    pub cleared: &'static str,
    pub model_now: &'static str,
    pub model_default: &'static str,
    pub model_invalid: &'static str,
    pub config_unreadable: &'static str,
    pub off_not_saved: &'static str,
    pub reply_hint: &'static str,
    pub status_running: &'static str,
    pub status_target_new: &'static str,
    pub status_target_resume: &'static str,
    pub status_idle: &'static str,
    pub timeout: &'static str,
    pub truncated: &'static str,
    pub turns: &'static str,
    pub tray_enable: &'static str,
    pub tray_disable: &'static str,
    pub enabled_title: &'static str,
    pub enabled_body: &'static str,
    pub disabled_title: &'static str,
    pub cmd_new: &'static str,
    pub cmd_sessions: &'static str,
    pub cmd_stop: &'static str,
    pub cmd_status: &'static str,
    pub cmd_exit: &'static str,
    pub cmd_model: &'static str,
    pub cmd_help: &'static str,
    /// First line of the status message of a run that waited for a usage limit to reset.
    pub due_queued: &'static str,
    pub due_continue: &'static str,
}

static EN: S = S {
    disabled: "Remote sessions are off. Turn them on from the tray menu: Phone: remote sessions.",
    elevated: "Remote sessions are refused while HeadsUp runs as Administrator. Quit it from the tray and start it again normally.",
    pair_first: "Pair the Telegram chat first: send the pairing code (tray: Phone: show Telegram pairing code) to your bot, then turn remote sessions on.",
    help: "<b>Remote sessions</b>\n/new: start a new session in a project\n/sessions: pick a recent session; your next message goes there\n/status: what is running\n/stop: stop the running task\n/exit: clear the selected session\n/model [name]: model for remote runs (/model default = your default)\n\nAfter a run, just write to continue the same session. A question or permission card is answered with its buttons, or by replying to that card.\n\nWhile a usage limit is used up, a message waits and is sent when the limit resets (/status lists what waits, /stop cancels it).",
    pick_project: "Which project? A new session starts there.",
    no_projects: "No known projects yet: open a project in Claude Code on the PC once.",
    project_chosen: "📁 <b>{p}</b>: new session. Send the task.",
    pick_session: "Recent sessions. Your next message goes to the one you pick.",
    no_sessions: "No sessions found.",
    session_chosen: "💬 <b>{p}</b> · {t}\nSend your message.",
    no_target: "No session selected. /new starts one, /sessions picks an existing one.",
    busy: "A remote task is still running. /status shows it, /stop stops it.",
    finishing: "The last task answered but is still finishing background work. Wait a moment or /stop it.",
    working: "⏳ <b>{p}</b> · working…",
    done: "✅ <b>{p}</b> · done",
    failed: "❌ <b>{p}</b> · failed",
    stopped: "⏹ Stopped.",
    nothing_running: "Nothing is running.",
    active_warn: "⚠️ This session changed {s} s ago, so it may be open on the PC. Sending now would mix the two conversations.",
    send_anyway: "Send anyway",
    cancel: "Cancel",
    cancelled: "Cancelled.",
    expired: "That menu is out of date. Send the command again.",
    claude_missing: "Claude Code CLI not found. Set remote.claudePath in config.json.",
    cleared: "Selection cleared.",
    model_now: "Model for remote runs: <b>{m}</b>",
    model_default: "Model for remote runs: your Claude Code default.",
    model_invalid: "Not a valid model name.",
    config_unreadable: "config.json could not be read, so it was not changed. Fix the file and try again.",
    off_not_saved: "Remote sessions are off until HeadsUp restarts. config.json could not be saved, so fix it to keep them off.",
    reply_hint: "Write to continue this session · /new for a new one",
    status_running: "▶️ <b>{p}</b> · running for {s} s",
    status_target_new: "Next message starts a new session in <b>{p}</b>.",
    status_target_resume: "Next message goes to <b>{p}</b> · {t}",
    status_idle: "Idle. /new or /sessions.",
    timeout: "⏱ Stopped after {m} minutes.",
    truncated: "… (truncated)",
    turns: "turns",
    tray_enable: "Phone: remote sessions (off)",
    tray_disable: "Phone: remote sessions (on)",
    enabled_title: "Remote sessions on",
    enabled_body: "Your paired Telegram chat can now start and continue Claude Code sessions on this PC.",
    disabled_title: "Remote sessions off",
    cmd_new: "Start a new session in a project",
    cmd_sessions: "Continue a recent session",
    cmd_stop: "Stop the running task",
    cmd_status: "What is running",
    cmd_exit: "Clear the selected session",
    cmd_model: "Model for remote runs",
    cmd_help: "Help",
    due_queued: "⏰ The limit reset: sending your waiting message.",
    due_continue: "⏰ The limit reset: continuing this session.",
};

static TR: S = S {
    disabled: "Uzaktan oturum kapalı. Tepsi menüsünden aç: Telefon: uzaktan oturum.",
    elevated: "HeadsUp yönetici olarak çalışırken uzaktan oturum reddedilir. Tepsiden çık ve normal şekilde yeniden başlat.",
    pair_first: "Önce Telegram sohbetini eşleştir: eşleştirme kodunu (tepsi: Telefon: Telegram eşleştirme kodunu göster) botuna gönder, sonra uzaktan oturumu aç.",
    help: "<b>Uzaktan oturum</b>\n/new: bir projede yeni oturum başlat\n/sessions: son oturumlardan birini seç; sonraki mesajın oraya gider\n/status: ne çalışıyor\n/stop: çalışan görevi durdur\n/exit: seçili oturumu bırak\n/model [ad]: uzaktan çalıştırma modeli (/model default = varsayılanın)\n\nBir çalıştırma bitince yazman yeterli, aynı oturum devam eder. Soru ve izin kartları kendi butonlarıyla ya da o karta yanıt vererek cevaplanır.\n\nKullanım limiti doluyken mesajın bekler ve limit yenilenince gönderilir (/status bekleyenleri gösterir, /stop iptal eder).",
    pick_project: "Hangi proje? Yeni oturum orada başlar.",
    no_projects: "Henüz bilinen proje yok: bir projeyi PC'de Claude Code ile bir kez aç.",
    project_chosen: "📁 <b>{p}</b>: yeni oturum. Görevi yaz.",
    pick_session: "Son oturumlar. Sonraki mesajın seçtiğine gider.",
    no_sessions: "Oturum bulunamadı.",
    session_chosen: "💬 <b>{p}</b> · {t}\nMesajını yaz.",
    no_target: "Seçili oturum yok. /new yeni başlatır, /sessions mevcut birini seçer.",
    busy: "Bir uzaktan görev hâlâ çalışıyor. /status gösterir, /stop durdurur.",
    finishing: "Son görev cevap verdi ama arka plan işlerini bitiriyor. Biraz bekle ya da /stop ile durdur.",
    working: "⏳ <b>{p}</b> · çalışıyor…",
    done: "✅ <b>{p}</b> · bitti",
    failed: "❌ <b>{p}</b> · hata",
    stopped: "⏹ Durduruldu.",
    nothing_running: "Çalışan bir şey yok.",
    active_warn: "⚠️ Bu oturum {s} sn önce değişti, PC'de açık olabilir. Şimdi göndermek iki konuşmayı birbirine karıştırır.",
    send_anyway: "Yine de gönder",
    cancel: "İptal",
    cancelled: "İptal edildi.",
    expired: "Bu menü eskidi. Komutu yeniden gönder.",
    claude_missing: "Claude Code CLI bulunamadı. config.json'da remote.claudePath ayarla.",
    cleared: "Seçim temizlendi.",
    model_now: "Uzaktan çalıştırma modeli: <b>{m}</b>",
    model_default: "Uzaktan çalıştırma modeli: Claude Code varsayılanın.",
    model_invalid: "Geçerli bir model adı değil.",
    config_unreadable: "config.json okunamadı, bu yüzden değiştirilmedi. Dosyayı düzeltip tekrar dene.",
    off_not_saved: "Uzaktan oturum HeadsUp yeniden başlayana kadar kapalı. config.json kaydedilemedi; kapalı kalması için dosyayı düzelt.",
    reply_hint: "Devam etmek için yaz · yeni oturum: /new",
    status_running: "▶️ <b>{p}</b> · {s} sn'dir çalışıyor",
    status_target_new: "Sonraki mesaj <b>{p}</b> içinde yeni oturum başlatır.",
    status_target_resume: "Sonraki mesaj <b>{p}</b> · {t} oturumuna gider",
    status_idle: "Boşta. /new ya da /sessions.",
    timeout: "⏱ {m} dakika sonra durduruldu.",
    truncated: "… (kısaltıldı)",
    turns: "tur",
    tray_enable: "Telefon: uzaktan oturum (kapalı)",
    tray_disable: "Telefon: uzaktan oturum (açık)",
    enabled_title: "Uzaktan oturum açık",
    enabled_body: "Eşleşmiş Telegram sohbetin artık bu PC'de Claude Code oturumu başlatıp sürdürebilir.",
    disabled_title: "Uzaktan oturum kapalı",
    cmd_new: "Bir projede yeni oturum başlat",
    cmd_sessions: "Son bir oturuma devam et",
    cmd_stop: "Çalışan görevi durdur",
    cmd_status: "Ne çalışıyor",
    cmd_exit: "Seçili oturumu bırak",
    cmd_model: "Uzaktan çalıştırma modeli",
    cmd_help: "Yardım",
    due_queued: "⏰ Limit yenilendi: bekleyen mesajın gönderiliyor.",
    due_continue: "⏰ Limit yenilendi: bu oturuma devam ediliyor.",
};

pub fn s(shared: &Shared) -> &'static S {
    if *shared.lang_code.lock().unwrap() == "tr" {
        &TR
    } else {
        &EN
    }
}

fn esc(x: &str) -> String {
    crate::phone::esc(x)
}

fn fill(template: &str, pairs: &[(&str, &str)]) -> String {
    let mut out = template.to_string();
    for (k, v) in pairs {
        out = out.replace(&format!("{{{k}}}"), v);
    }
    out
}

fn paired(cfg: &Value) -> bool {
    telegram::creds(cfg).map(|c| !c.chat_id.is_empty()).unwrap_or(false)
}

/// Remote sessions are usable: switched on and a Telegram chat is paired.
pub fn enabled(shared: &Shared) -> bool {
    let cfg = shared.cfg();
    config::get_bool(&cfg, "remote.enabled", false) && paired(&cfg) && !shared.remote.lock().unwrap().force_off
}

/// Was this Telegram message sent by a remote session (status, result, menus)?
pub fn is_remote_msg(shared: &Shared, mid: i64) -> bool {
    shared.remote.lock().unwrap().msg_ids.contains(&mid)
}

/// Forget the selected session, any held prompt and the messages waiting for a usage-limit
/// reset (remote switched off).
pub fn clear_selection(shared: &Arc<Shared>) {
    {
        let mut st = shared.remote.lock().unwrap();
        st.target = None;
        st.target_gen += 1;
        st.held = None;
    }
    crate::schedule::drop_remote(shared);
}

/// A run is going on (or finishing).
pub fn busy(shared: &Shared) -> bool {
    shared.remote.lock().unwrap().running.is_some()
}

/// The generation of the phone's target (bumped whenever the user picks another one).
pub fn target_gen(shared: &Shared) -> u64 {
    shared.remote.lock().unwrap().target_gen
}

/// Send to the paired chat as a remote-session message (a reply to it is a prompt).
pub fn send_msg(shared: &Shared, html: &str, buttons: &[Vec<(String, String)>]) -> Option<i64> {
    send(shared, html, buttons)
}

pub fn last_touch_ms(shared: &Shared) -> i64 {
    shared.remote.lock().unwrap().last_touch_ms
}

/// Is this session id one of our remote runs (running now or finished in the last minutes)?
pub fn is_remote_session(shared: &Shared, session_id: Option<&str>) -> bool {
    let Some(id) = session_id.filter(|s| !s.is_empty()) else { return false };
    shared.remote_sessions.lock().unwrap().contains(id)
}

/// Does a run currently target this session (used to hold its question/permission cards)?
pub fn has_target_or_run(shared: &Shared) -> bool {
    let st = shared.remote.lock().unwrap();
    st.target.is_some() || st.running.is_some()
}

// ---------------------------------------------------------------- discovery

pub fn claude_config_dir() -> PathBuf {
    if let Some(d) = std::env::var_os("CLAUDE_CONFIG_DIR") {
        return PathBuf::from(d);
    }
    std::env::var_os("USERPROFILE")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".claude")
}

fn projects_root() -> PathBuf {
    claude_config_dir().join("projects")
}

fn usable_exe(p: &Path) -> bool {
    // During an npm update the binary is briefly a tiny placeholder.
    std::fs::metadata(p).map(|m| m.is_file() && m.len() > 1_000_000).unwrap_or(false)
}

/// The Claude Code CLI binary: remote.claudePath, the npm install, the native installer,
/// PATH, then the desktop app's bundled copy.
pub fn claude_path(cfg: &Value) -> Option<PathBuf> {
    if let Some(p) = config::get_str(cfg, "remote.claudePath").map(|s| s.trim().to_string()).filter(|s| !s.is_empty()) {
        let p = PathBuf::from(p);
        if usable_exe(&p) {
            return Some(p);
        }
    }
    let env = |k: &str| std::env::var_os(k).map(PathBuf::from);
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(a) = env("APPDATA") {
        candidates.push(a.join("npm").join("node_modules").join("@anthropic-ai").join("claude-code").join("bin").join("claude.exe"));
    }
    if let Some(u) = env("USERPROFILE") {
        candidates.push(u.join(".local").join("bin").join("claude.exe"));
    }
    // The persistent PATH first (what the run itself gets), then the one we were started with.
    let persistent_path = persistent_env().and_then(|e| env_get(&e, "Path").map(std::ffi::OsString::from));
    for path in [persistent_path, std::env::var_os("PATH")].into_iter().flatten() {
        for dir in std::env::split_paths(&path) {
            // A relative entry would resolve against the overlay's own folder.
            if dir.is_absolute() {
                candidates.push(dir.join("claude.exe"));
            }
        }
    }
    if let Some(found) = candidates.into_iter().find(|p| usable_exe(p)) {
        return Some(found);
    }
    // Desktop app bundle: %APPDATA%\Claude\claude-code\<version>\...\claude.exe (newest).
    let base = env("APPDATA")?.join("Claude").join("claude-code");
    let mut best: Option<(std::time::SystemTime, PathBuf)> = None;
    let mut stack = vec![(base, 0usize)];
    while let Some((dir, depth)) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() && depth < 4 {
                stack.push((p, depth + 1));
            } else if p.file_name().map(|n| n.eq_ignore_ascii_case("claude.exe")).unwrap_or(false) && usable_exe(&p) {
                let t = e.metadata().and_then(|m| m.modified()).unwrap_or(std::time::UNIX_EPOCH);
                if best.as_ref().map(|(bt, _)| t > *bt).unwrap_or(true) {
                    best = Some((t, p));
                }
            }
        }
    }
    best.map(|(_, p)| p)
}

fn temp_dir_lower() -> String {
    std::env::temp_dir().to_string_lossy().to_lowercase()
}

fn path_key(p: &str) -> String {
    p.trim_end_matches(['\\', '/']).replace('/', "\\").to_lowercase()
}

/// (cwd, title) from the head of a transcript: the first real user prompt.
pub fn read_head(path: &Path) -> Option<(String, String)> {
    let f = File::open(path).ok()?;
    let reader = BufReader::new(f.take(3_000_000));
    let mut cwd = String::new();
    for line in reader.lines().take(600) {
        let Ok(line) = line else { break };
        if !line.contains("\"cwd\"") {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(&line) else { continue };
        if cwd.is_empty() {
            if let Some(c) = v.get("cwd").and_then(Value::as_str) {
                cwd = c.to_string();
            }
        }
        if v.get("type").and_then(Value::as_str) != Some("user")
            || v.get("isSidechain").and_then(Value::as_bool) == Some(true)
        {
            continue;
        }
        let text = match v.pointer("/message/content") {
            Some(Value::String(s)) => s.clone(),
            Some(Value::Array(a)) => a
                .iter()
                .find_map(|b| {
                    (b.get("type").and_then(Value::as_str) == Some("text"))
                        .then(|| b.get("text").and_then(Value::as_str).unwrap_or("").to_string())
                })
                .unwrap_or_default(),
            _ => String::new(),
        };
        let text = text.trim();
        if text.is_empty() || text.starts_with('<') {
            continue;
        }
        if cwd.is_empty() {
            return None;
        }
        let title = util::truncate(&text.split_whitespace().collect::<Vec<_>>().join(" "), 48);
        return Some((cwd, title));
    }
    if cwd.is_empty() {
        None
    } else {
        Some((cwd, String::new()))
    }
}

fn project_label(cwd: &str) -> String {
    util::project_name(Some(&Value::String(cwd.to_string())))
}

/// Top-level transcripts, newest first.
fn transcripts() -> Vec<(PathBuf, std::time::SystemTime)> {
    let mut out = Vec::new();
    let Ok(dirs) = std::fs::read_dir(projects_root()) else { return out };
    for d in dirs.flatten() {
        let Ok(files) = std::fs::read_dir(d.path()) else { continue };
        for f in files.flatten() {
            let p = f.path();
            if p.extension().map(|e| e == "jsonl").unwrap_or(false) && p.is_file() {
                if let Ok(t) = f.metadata().and_then(|m| m.modified()) {
                    out.push((p, t));
                }
            }
        }
    }
    out.sort_by(|a, b| b.1.cmp(&a.1));
    out
}

fn usable_cwd(cwd: &str) -> bool {
    !cwd.is_empty() && Path::new(cwd).is_dir() && !cwd.to_lowercase().starts_with(&temp_dir_lower())
}

pub fn list_projects(cfg: &Value, limit: usize) -> Vec<Project> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for extra in config::get_str_list(cfg, "remote.projects") {
        if Path::new(&extra).is_dir() && seen.insert(path_key(&extra)) {
            out.push(Project { name: project_label(&extra), cwd: extra });
        }
    }
    // One head read per project folder (its newest transcript), newest folders first.
    let mut seen_dirs = HashSet::new();
    for (path, _) in transcripts() {
        if out.len() >= limit {
            break;
        }
        let dir = path.parent().map(|p| p.to_path_buf()).unwrap_or_default();
        if seen_dirs.contains(&dir) {
            continue;
        }
        let Some((cwd, _)) = read_head(&path) else { continue };
        seen_dirs.insert(dir);
        if usable_cwd(&cwd) && seen.insert(path_key(&cwd)) {
            out.push(Project { name: project_label(&cwd), cwd });
        }
    }
    out.truncate(limit);
    out
}

/// Only directories from this list may be used as a working directory.
pub fn is_known_project(cfg: &Value, cwd: &str) -> bool {
    let key = path_key(cwd);
    list_projects(cfg, 500).iter().any(|p| path_key(&p.cwd) == key)
}

pub fn list_sessions(limit: usize) -> Vec<SessionInfo> {
    let mut out = Vec::new();
    for (path, _) in transcripts().into_iter().take(60) {
        if out.len() >= limit {
            break;
        }
        let Some(id) = path.file_stem().map(|s| s.to_string_lossy().to_string()) else { continue };
        let Some((cwd, title)) = read_head(&path) else { continue };
        if !usable_cwd(&cwd) || title.is_empty() {
            continue;
        }
        out.push(SessionInfo { project: project_label(&cwd), id, cwd, title });
    }
    out
}

/// The transcript of session `id` (a file name never comes from anywhere but a session id
/// made of letters, digits and dashes).
pub fn transcript_path(id: &str) -> Option<PathBuf> {
    if id.is_empty() || !id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
        return None;
    }
    let dirs = std::fs::read_dir(projects_root()).ok()?;
    dirs.flatten().map(|d| d.path().join(format!("{id}.jsonl"))).find(|p| p.is_file())
}

fn transcript_mtime_ms(id: &str) -> Option<i64> {
    let m = std::fs::metadata(transcript_path(id)?).ok()?;
    let t = m.modified().ok()?;
    Some(t.duration_since(std::time::UNIX_EPOCH).ok()?.as_millis() as i64)
}

/// How long ago (ms) session `id` changed, when that was after `since_ms`, less than
/// remote.busyGuardSeconds ago and not through one of our own runs: then it is probably open on
/// the PC, and a message to it is held for confirmation.
pub fn changed_since(shared: &Shared, id: &str, since_ms: i64) -> Option<i64> {
    let guard_ms = config::get_u64(&shared.cfg(), "remote.busyGuardSeconds", 90) as i64 * 1000;
    let own_end = shared.remote.lock().unwrap().own_runs.get(id).copied();
    let mtime = transcript_mtime_ms(id)?;
    let age = util::now_ms() - mtime;
    let ours = own_end.map(|end| mtime <= end + 15_000).unwrap_or(false);
    (guard_ms > 0 && age < guard_ms && !ours && mtime > since_ms).then_some(age)
}

/// Hold `text` for `target` until the user confirms (Send anyway / Cancel): the session changed
/// `age_ms` ago, so sending now could mix two conversations. `now`: it was sent with Send now, so
/// Send anyway sends it even while a usage limit is used up.
pub fn hold_prompt(shared: &Shared, target: Target, text: String, age_ms: i64, now: bool) {
    let x = s(shared);
    let gen = {
        let mut st = shared.remote.lock().unwrap();
        st.held_gen += 1;
        let g = st.held_gen;
        st.held = Some((g, target, text, now));
        g
    };
    let secs = (age_ms / 1000).max(1).to_string();
    send(
        shared,
        &fill(x.active_warn, &[("s", &secs)]),
        &[vec![(x.send_anyway.to_string(), format!("rf:{gen}")), (x.cancel.to_string(), format!("rx:{gen}"))]],
    );
}

// ---------------------------------------------------------------- environment

/// The string values (REG_SZ / REG_EXPAND_SZ) of one registry key: (name, value, expandable).
fn reg_values(root: HKEY, path: &str) -> Option<Vec<(String, String, bool)>> {
    let wide: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
    let mut key: HKEY = std::ptr::null_mut();
    if unsafe { RegOpenKeyExW(root, wide.as_ptr(), 0, KEY_READ, &mut key) } != 0 {
        return None;
    }
    let mut out = Vec::new();
    let mut name = vec![0u16; 512];
    let mut data = vec![0u8; 8192];
    let mut index = 0u32;
    loop {
        let mut name_len = name.len() as u32;
        let mut data_len = data.len() as u32;
        let mut kind = 0u32;
        let rc = unsafe {
            RegEnumValueW(key, index, name.as_mut_ptr(), &mut name_len, std::ptr::null(), &mut kind, data.as_mut_ptr(), &mut data_len)
        };
        if rc == ERROR_MORE_DATA {
            if data.len() < 1 << 20 {
                // Grow and read the same value again.
                name.resize(16_384, 0);
                data.resize((data_len as usize).max(data.len() * 2).min(1 << 20), 0);
            } else {
                index += 1; // an absurdly large value: skip it
            }
            continue;
        }
        if rc != 0 {
            break; // ERROR_NO_MORE_ITEMS, or a failure
        }
        index += 1;
        if kind != REG_SZ && kind != REG_EXPAND_SZ {
            continue;
        }
        let units: Vec<u16> = data[..(data_len as usize).min(data.len())]
            .chunks_exact(2)
            .map(|b| u16::from_le_bytes([b[0], b[1]]))
            .collect();
        let value = String::from_utf16_lossy(&units).trim_end_matches('\0').to_string();
        let value_name = String::from_utf16_lossy(&name[..(name_len as usize).min(name.len())]);
        out.push((value_name, value, kind == REG_EXPAND_SZ));
    }
    unsafe {
        RegCloseKey(key);
    }
    Some(out)
}

fn env_get<'a>(env: &'a [(String, String)], k: &str) -> Option<&'a str> {
    env.iter().find(|(n, _)| n.eq_ignore_ascii_case(k)).map(|(_, v)| v.as_str())
}

fn env_set(env: &mut Vec<(String, String)>, k: &str, v: String) {
    match env.iter().position(|(n, _)| n.eq_ignore_ascii_case(k)) {
        Some(i) => env[i].1 = v,
        None => env.push((k.to_string(), v)),
    }
}

/// Expand %NAME% references from `env`; unknown ones stay as they are, as in Windows.
fn expand(value: &str, env: &[(String, String)]) -> String {
    let mut out = String::with_capacity(value.len());
    let mut rest = value;
    while let Some(start) = rest.find('%') {
        let after = &rest[start + 1..];
        let Some(end) = after.find('%') else { break };
        match env_get(env, &after[..end]).filter(|_| end > 0) {
            Some(v) => {
                out.push_str(&rest[..start]);
                out.push_str(v);
                rest = &after[end + 1..];
            }
            None => {
                out.push_str(&rest[..=start]);
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

/// Variables Windows gives every process of the logon session. They are not in the
/// registry's Environment keys, so they come from our own environment.
const SESSION_BASE: &[&str] = &[
    "SystemRoot", "SystemDrive", "windir", "ComSpec", "PATHEXT", "TEMP", "TMP", "OS", "USERPROFILE", "USERNAME",
    "USERDOMAIN", "USERDOMAIN_ROAMINGPROFILE", "LOGONSERVER", "COMPUTERNAME", "SESSIONNAME", "HOMEDRIVE", "HOMEPATH",
    "HOMESHARE", "APPDATA", "LOCALAPPDATA", "ProgramData", "ALLUSERSPROFILE", "PUBLIC", "ProgramFiles",
    "ProgramFiles(x86)", "ProgramW6432", "CommonProgramFiles", "CommonProgramFiles(x86)", "CommonProgramW6432",
    "PROCESSOR_ARCHITECTURE", "PROCESSOR_IDENTIFIER", "PROCESSOR_LEVEL", "PROCESSOR_REVISION", "NUMBER_OF_PROCESSORS",
];

/// The environment a terminal opened now would get: the session's own variables plus the
/// persistent machine and user variables (user wins; Path is machine + user), %VAR%
/// expanded. Not the environment we were started with: that belongs to whichever Claude
/// session's hook started the overlay (its project's PATH, its CLAUDE_* variables).
fn persistent_env() -> Option<Vec<(String, String)>> {
    let machine = reg_values(HKEY_LOCAL_MACHINE, r"SYSTEM\CurrentControlSet\Control\Session Manager\Environment");
    let user = reg_values(HKEY_CURRENT_USER, "Environment");
    if machine.is_none() && user.is_none() {
        util::log_line(&["remote-error", "could not read the persistent environment"]);
        return None;
    }
    let mut base: Vec<(String, String)> = Vec::new();
    for k in SESSION_BASE {
        if let Ok(v) = std::env::var(k) {
            env_set(&mut base, k, v);
        }
    }
    let mut env = build_env(base, machine.unwrap_or_default(), user.unwrap_or_default());
    // Listing and running must use the same Claude directory.
    if let Ok(v) = std::env::var("CLAUDE_CONFIG_DIR") {
        env_set(&mut env, "CLAUDE_CONFIG_DIR", v);
    }
    Some(env)
}

/// Merge the registry's (name, value, expandable) lists over the session's own variables the
/// way Windows builds a logon environment, so a %VAR% resolves whatever order the values
/// were created in: plain values first, then expandable ones, Path (machine + user) last.
fn build_env(
    base: Vec<(String, String)>,
    machine: Vec<(String, String, bool)>,
    user: Vec<(String, String, bool)>,
) -> Vec<(String, String)> {
    let mut env = base;
    let scopes = [(false, &machine), (true, &user)];
    for pass_expandable in [false, true] {
        for (is_user, vars) in scopes {
            for (name, value, expandable) in vars.iter() {
                if *expandable != pass_expandable || name.eq_ignore_ascii_case("Path") {
                    continue;
                }
                if is_user || !SESSION_BASE.iter().any(|b| b.eq_ignore_ascii_case(name)) {
                    // The session's own values win over the machine defaults (USERNAME=SYSTEM,
                    // TEMP=C:\Windows\Temp); the user's own variables win over both.
                    let value = if *expandable { expand(value, &env) } else { value.clone() };
                    env_set(&mut env, name, value);
                }
            }
        }
    }
    let mut path: Vec<String> = Vec::new();
    for (_, vars) in scopes {
        for (name, value, expandable) in vars.iter() {
            if name.eq_ignore_ascii_case("Path") {
                let value = if *expandable { expand(value, &env) } else { value.clone() };
                path.extend(value.split(';').filter(|p| !p.trim().is_empty()).map(String::from));
            }
        }
    }
    env_set(&mut env, "Path", path.join(";"));
    env
}

/// Variables a parent Claude Code session injects into its children (removed when the
/// persistent environment cannot be read).
const SESSION_VARS: [&str; 6] = [
    "CLAUDECODE",
    "CLAUDE_CODE_ENTRYPOINT",
    "CLAUDE_CODE_CHILD_SESSION",
    "CLAUDE_CODE_SSE_PORT",
    "CLAUDE_AGENT_SDK_VERSION",
    "CLAUDE_CODE_SESSION_ID",
];

// ---------------------------------------------------------------- telegram glue

fn send(shared: &Shared, html: &str, buttons: &[Vec<(String, String)>]) -> Option<i64> {
    match telegram::send(shared, html, buttons) {
        Ok(id) => {
            let mut st = shared.remote.lock().unwrap();
            if st.msg_ids.len() > 2000 {
                st.msg_ids.clear();
            }
            st.msg_ids.insert(id);
            Some(id)
        }
        Err(e) => {
            util::log_line(&["telegram-error", &e]);
            None
        }
    }
}

/// Bot command list shown in Telegram's menu.
pub fn commands(shared: &Shared) -> Vec<(&'static str, &'static str)> {
    let x = s(shared);
    vec![
        ("new", x.cmd_new),
        ("sessions", x.cmd_sessions),
        ("status", x.cmd_status),
        ("stop", x.cmd_stop),
        ("exit", x.cmd_exit),
        ("model", x.cmd_model),
        ("help", x.cmd_help),
    ]
}

fn valid_model(m: &str) -> bool {
    !m.is_empty()
        && m.len() <= 64
        && !m.starts_with('-')
        && m.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '[' | ']' | ':'))
}

/// A slash command from the paired chat. Returns true when it was a remote command.
pub fn on_command(app: &AppHandle, shared: &Arc<Shared>, text: &str) -> bool {
    let mut parts = text.splitn(2, char::is_whitespace);
    let head = parts.next().unwrap_or("");
    let cmd = head.split('@').next().unwrap_or("").to_lowercase();
    let arg = parts.next().unwrap_or("").trim().to_string();
    let known = ["/new", "/projects", "/sessions", "/stop", "/status", "/exit", "/model", "/help"];
    if !known.contains(&cmd.as_str()) {
        return false;
    }
    util::log_line(&["remote-cmd", &cmd]);
    let x = s(shared);
    // /stop must always work, even after remote sessions were switched off mid-run. It also
    // cancels everything that waits for a usage-limit reset.
    if cmd == "/stop" {
        let mut lines: Vec<String> = Vec::new();
        if stop_running(app, shared, false) {
            lines.push(x.stopped.to_string());
        }
        let cancelled = crate::schedule::cancel_all(app, shared);
        if cancelled > 0 {
            lines.push(crate::schedule::s(shared).stop_cleared.replace("{n}", &cancelled.to_string()));
        }
        if lines.is_empty() {
            lines.push(x.nothing_running.to_string());
        }
        send(shared, &lines.join("\n"), &[]);
        return true;
    }
    // /status too: what waits for a limit reset includes sessions of the PC.
    if cmd == "/status" {
        let mut lines = Vec::new();
        if enabled(shared) {
            let st = shared.remote.lock().unwrap();
            if let Some(r) = &st.running {
                let secs = ((util::now_ms() - r.started_ms) / 1000).to_string();
                lines.push(fill(x.status_running, &[("p", &esc(&r.project)), ("s", &secs)]));
            }
            match &st.target {
                Some(Target::New { project, .. }) => lines.push(fill(x.status_target_new, &[("p", &esc(project))])),
                Some(Target::Resume { project, title, .. }) => {
                    lines.push(fill(x.status_target_resume, &[("p", &esc(project)), ("t", &esc(title))]))
                }
                None => {}
            }
            if lines.is_empty() {
                lines.push(x.status_idle.to_string());
            }
        } else {
            lines.push(x.disabled.to_string());
        }
        lines.extend(crate::limits::status_lines(shared));
        let (waiting, buttons) = crate::schedule::status(shared);
        lines.extend(waiting);
        send(shared, &lines.join("\n"), &buttons);
        return true;
    }
    if !enabled(shared) {
        send(shared, x.disabled, &[]);
        return true;
    }
    // An elevated overlay would hand its administrator token to every run.
    if crate::win::is_elevated() && matches!(cmd.as_str(), "/new" | "/projects" | "/sessions") {
        util::log_line(&["remote-refused", "elevated"]);
        send(shared, x.elevated, &[]);
        return true;
    }
    let cfg = shared.cfg();
    match cmd.as_str() {
        "/help" => {
            send(shared, x.help, &[]);
        }
        "/new" | "/projects" => {
            let projects = list_projects(&cfg, 10);
            if projects.is_empty() {
                send(shared, x.no_projects, &[]);
            } else {
                let gen = {
                    let mut st = shared.remote.lock().unwrap();
                    st.menu_gen += 1;
                    st.projects_menu = projects.clone();
                    st.menu_gen
                };
                let buttons: Vec<Vec<(String, String)>> = projects
                    .iter()
                    .enumerate()
                    .map(|(i, p)| vec![(util::truncate(&mask::secrets(&format!("📁 {}", p.name)), 60), format!("rp:{gen}:{i}"))])
                    .collect();
                send(shared, x.pick_project, &buttons);
            }
        }
        "/sessions" => {
            let sessions = list_sessions(8);
            if sessions.is_empty() {
                send(shared, x.no_sessions, &[]);
            } else {
                let gen = {
                    let mut st = shared.remote.lock().unwrap();
                    st.menu_gen += 1;
                    st.sessions_menu = sessions.clone();
                    st.menu_gen
                };
                let buttons: Vec<Vec<(String, String)>> = sessions
                    .iter()
                    .enumerate()
                    .map(|(i, si)| vec![(util::truncate(&mask::secrets(&format!("{} · {}", si.project, si.title)), 60), format!("rs:{gen}:{i}"))])
                    .collect();
                send(shared, x.pick_session, &buttons);
            }
        }
        "/exit" => {
            {
                let mut st = shared.remote.lock().unwrap();
                st.target = None;
                st.target_gen += 1;
                st.held = None;
            }
            send(shared, x.cleared, &[]);
        }
        "/model" => {
            if !arg.is_empty() {
                let value = if arg.eq_ignore_ascii_case("default") || arg == "-" {
                    String::new()
                } else if valid_model(&arg) {
                    arg.clone()
                } else {
                    send(shared, x.model_invalid, &[]);
                    return true;
                };
                if !set_user_config(shared, "model", json!(value)) {
                    send(shared, x.config_unreadable, &[]);
                    return true;
                }
            }
            let m = config::get_str(&shared.cfg(), "remote.model").unwrap_or_default();
            if m.trim().is_empty() {
                send(shared, x.model_default, &[]);
            } else {
                send(shared, &fill(x.model_now, &[("m", &esc(&m))]), &[]);
            }
        }
        _ => {}
    }
    true
}

/// A button from a remote menu. Returns the short text for Telegram's callback toast.
pub fn on_callback(app: &AppHandle, shared: &Arc<Shared>, data: &str) -> Option<String> {
    util::log_line(&["remote-cb", data]);
    let x = s(shared);
    if !enabled(shared) {
        return Some(x.disabled.to_string());
    }
    let mut it = data.split(':');
    let kind = it.next().unwrap_or("");
    let gen: u64 = it.next().and_then(|g| g.parse().ok()).unwrap_or(u64::MAX);
    let idx: usize = it.next().and_then(|i| i.parse().ok()).unwrap_or(usize::MAX);
    match kind {
        "rp" | "rs" => {
            let picked = {
                let mut st = shared.remote.lock().unwrap();
                if gen != st.menu_gen {
                    None
                } else if kind == "rp" {
                    st.projects_menu.get(idx).cloned().map(|p| {
                        let t = Target::New { cwd: p.cwd.clone(), project: p.name.clone() };
                        st.target = Some(t);
                        st.target_gen += 1;
                        st.held = None;
                        st.last_touch_ms = util::now_ms();
                        fill(x.project_chosen, &[("p", &esc(&p.name))])
                    })
                } else {
                    st.sessions_menu.get(idx).cloned().map(|si| {
                        st.target = Some(Target::Resume { id: si.id.clone(), cwd: si.cwd.clone(), project: si.project.clone(), title: si.title.clone() });
                        st.target_gen += 1;
                        st.held = None;
                        st.last_touch_ms = util::now_ms();
                        fill(x.session_chosen, &[("p", &esc(&si.project)), ("t", &esc(&si.title))])
                    })
                }
            };
            match picked {
                Some(msg) => {
                    send(shared, &msg, &[]);
                    Some("✓".to_string())
                }
                None => {
                    send(shared, x.expired, &[]);
                    Some(x.expired.to_string())
                }
            }
        }
        "rf" | "rx" => {
            let held = {
                let mut st = shared.remote.lock().unwrap();
                match &st.held {
                    Some((g, ..)) if *g == gen => st.held.take(),
                    _ => None,
                }
            };
            match (kind, held) {
                ("rf", Some((_, target, prompt, now))) => {
                    // A usage limit used up meanwhile: it waits for the reset instead (unless it
                    // was sent with Send now).
                    if now || !crate::schedule::queue_if_limited(app, shared, &target, &prompt) {
                        start_run(app, shared, target, prompt);
                    }
                    Some(x.send_anyway.to_string())
                }
                ("rx", Some(_)) => {
                    send(shared, x.cancelled, &[]);
                    Some(x.cancelled.to_string())
                }
                _ => Some(x.expired.to_string()),
            }
        }
        _ => None,
    }
}

/// Plain text that is not an answer to a card: send it to the selected session. Returns false
/// when remote sessions are off (the caller then falls back to its own handling).
pub fn on_text(app: &AppHandle, shared: &Arc<Shared>, text: &str) -> bool {
    if !enabled(shared) {
        return false;
    }
    let x = s(shared);
    let (target, running) = {
        let st = shared.remote.lock().unwrap();
        (st.target.clone(), st.running.as_ref().map(|r| r.result_sent))
    };
    util::log_line(&[
        "remote-text",
        match (&target, running) {
            (_, Some(_)) => "busy",
            (Some(Target::New { .. }), _) => "new",
            (Some(Target::Resume { .. }), _) => "resume",
            (None, _) => "no-target",
        },
    ]);
    if let Some(result_sent) = running {
        send(shared, if result_sent { x.finishing } else { x.busy }, &[]);
        return true;
    }
    let Some(target) = target else {
        send(shared, x.no_target, &[]);
        return true;
    };
    // A usage limit used up: the message waits for the reset (the busy guard is checked then).
    if crate::schedule::queue_if_limited(app, shared, &target, text) {
        return true;
    }
    // Busy guard: a session someone is using on the PC right now.
    if let Target::Resume { id, .. } = &target {
        if let Some(age) = changed_since(shared, id, 0) {
            hold_prompt(shared, target.clone(), text.to_string(), age, false);
            return true;
        }
    }
    start_run(app, shared, target, text.to_string());
    true
}

/// Write one key of the `remote` section into the user's config.json. Refuses to touch a
/// file that exists but does not parse, so a typo never wipes the user's settings.
fn set_user_config(shared: &Shared, key: &str, value: Value) -> bool {
    let path = util::config_path();
    let lock = util::lock_config();
    let mut cfg = if path.exists() {
        match util::read_json(&path) {
            Some(v) if v.is_object() => v,
            _ => {
                util::log_line(&["remote-error", "config.json unreadable, not written"]);
                return false;
            }
        }
    } else {
        json!({})
    };
    if !cfg.get("remote").map(Value::is_object).unwrap_or(false) {
        cfg["remote"] = json!({});
    }
    cfg["remote"][key] = value;
    if util::write_json(&path, &cfg).is_err() {
        return false;
    }
    drop(lock);
    shared.reload_config();
    true
}

/// Tray toggle. Turning it off always stops a running task (not a session of the PC continued
/// at a limit reset) and clears the selection and the waiting messages, even when config.json
/// cannot be written (then it stays off for this session in memory).
pub fn toggle(app: &AppHandle, shared: &Arc<Shared>) -> Toggle {
    let cfg = shared.cfg();
    let currently_on = enabled(shared);
    if !currently_on {
        if !paired(&cfg) {
            return Toggle::NeedPairing;
        }
        if !set_user_config(shared, "enabled", json!(true)) {
            return Toggle::ConfigUnreadable;
        }
        {
            let mut st = shared.remote.lock().unwrap();
            st.force_off = false;
            st.commands_registered = false;
        }
        util::log_line(&["remote", "enabled"]);
        return Toggle::On;
    }
    stop_phone_run(app, shared);
    let written = set_user_config(shared, "enabled", json!(false));
    clear_selection(shared);
    {
        let mut st = shared.remote.lock().unwrap();
        st.commands_registered = false;
        st.force_off = !written;
    }
    util::log_line(&["remote", "disabled"]);
    if written {
        Toggle::Off
    } else {
        Toggle::OffNotSaved
    }
}

// ---------------------------------------------------------------- running claude

/// Fallback when a run has no Job Object.
pub fn kill_tree(pid: u32) {
    let _ = Command::new(crate::win::system_dir().join("taskkill.exe"))
        .args(["/T", "/F", "/PID", &pid.to_string()])
        .creation_flags(util::CREATE_NO_WINDOW)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

/// Stop the running task (process tree) and close its open cards. Returns false when
/// nothing was running.
pub fn stop_running(app: &AppHandle, shared: &Arc<Shared>, quitting: bool) -> bool {
    let info = {
        let mut st = shared.remote.lock().unwrap();
        st.running.as_mut().map(|r| {
            r.stop_requested = true;
            (r.pid, r.exited, r.session_id.clone(), r.job.clone())
        })
    };
    let Some((pid, exited, session_id, job)) = info else { return false };
    util::log_line(&["remote-stop", &pid.to_string()]);
    match job {
        Some(job) => job.kill(),
        // pid 0: not spawned yet (start_run checks stop_requested right after spawning).
        None if pid != 0 && !exited => kill_tree(pid),
        None => {}
    }
    if let Some(id) = &session_id {
        events::cancel_pending_for_session(app, shared, id);
    }
    let _ = quitting; // on quit the process exits right away: no slow network calls here
    true
}

/// Remote sessions went off: stop the run, unless it continues a session of the PC at a
/// usage-limit reset (that one does not need them; /stop still stops it).
pub fn stop_phone_run(app: &AppHandle, shared: &Arc<Shared>) -> bool {
    let pc = shared.remote.lock().unwrap().running.as_ref().is_some_and(|r| r.pc_continue);
    !pc && stop_running(app, shared, false)
}

/// Called when the overlay quits.
pub fn shutdown(app: &AppHandle, shared: &Arc<Shared>) {
    stop_running(app, shared, true);
}

/// HTML-escaped chunks of `text`, each at most CHUNK_MAX characters after escaping. Secrets
/// are masked over the whole text first, so none is split across chunks or lines unmasked.
fn escaped_chunks(text: &str) -> Vec<String> {
    let text = mask::secrets(text);
    let esc = crate::phone::esc_raw;
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut cur_len = 0usize;
    let push_piece = |piece: String, out: &mut Vec<String>, cur: &mut String, cur_len: &mut usize| {
        let len = piece.chars().count();
        if *cur_len > 0 && *cur_len + 1 + len > CHUNK_MAX {
            out.push(std::mem::take(cur));
            *cur_len = 0;
        }
        if *cur_len > 0 {
            cur.push('\n');
            *cur_len += 1;
        }
        cur.push_str(&piece);
        *cur_len += len;
    };
    for line in text.split('\n') {
        let escaped = esc(line);
        if escaped.chars().count() <= CHUNK_MAX {
            push_piece(escaped, &mut out, &mut cur, &mut cur_len);
            continue;
        }
        // A very long line: split the raw text so no escape sequence is cut in half.
        let mut piece = String::new();
        let mut piece_len = 0usize;
        for ch in line.chars() {
            let e = esc(&ch.to_string());
            let l = e.chars().count();
            if piece_len + l > CHUNK_MAX {
                push_piece(std::mem::take(&mut piece), &mut out, &mut cur, &mut cur_len);
                piece_len = 0;
            }
            piece.push_str(&e);
            piece_len += l;
        }
        if !piece.is_empty() {
            push_piece(piece, &mut out, &mut cur, &mut cur_len);
        }
    }
    if !cur.trim().is_empty() {
        out.push(cur);
    }
    out
}

fn activity_line(block: &Value) -> Option<String> {
    match block.get("type").and_then(Value::as_str) {
        Some("tool_use") => {
            let name = block.get("name").and_then(Value::as_str).unwrap_or("tool");
            // Masked before cutting, so a cut never leaves part of a secret unrecognised.
            Some(format!("🔧 {}", util::truncate(&mask::secrets(&events::describe_tool(name, block.get("input"))), 120)))
        }
        Some("text") => {
            let t = block.get("text").and_then(Value::as_str).unwrap_or("").trim();
            if t.is_empty() {
                None
            } else {
                Some(format!("✍️ {}", util::truncate(&mask::secrets(&t.split_whitespace().collect::<Vec<_>>().join(" ")), 120)))
            }
        }
        _ => None,
    }
}

/// Feed the rate-limit windows that headless runs report into the limit-reset timers
/// (only the windows the status line reports too).
fn feed_rate_limits(app: &AppHandle, shared: &Arc<Shared>, v: &Value) {
    let Some(windows) = v.pointer("/rate_limit_info/unifiedWindows").and_then(Value::as_object) else { return };
    let mut rl = Map::new();
    for (name, info) in windows {
        if !events::LIMIT_WINDOWS.contains(&name.as_str()) {
            continue;
        }
        let Some(at) = info.get("resetsAt").and_then(|x| x.as_i64().or_else(|| x.as_f64().map(|f| f as i64))) else { continue };
        let used = info.get("utilization").and_then(Value::as_f64).map(|u| u * 100.0).unwrap_or(0.0);
        rl.insert(name.clone(), json!({"resets_at": at, "used_percentage": used}));
    }
    if !rl.is_empty() {
        events::handle_status(app, shared, &json!({"rate_limits": Value::Object(rl)}));
    }
}

/// A message from the phone, sent now.
pub fn start_run(app: &AppHandle, shared: &Arc<Shared>, target: Target, prompt: String) {
    let gen = target_gen(shared);
    start(app, shared, target, prompt, Origin::Phone, Some(gen), None);
}

/// A prompt is held for the user's confirmation (hold_prompt).
pub fn has_held(shared: &Shared) -> bool {
    shared.remote.lock().unwrap().held.is_some()
}

/// remote.model, when it is a valid model name.
fn configured_model(cfg: &Value) -> Option<String> {
    config::get_str(cfg, "remote.model").map(|m| m.trim().to_string()).filter(|m| valid_model(m))
}

/// The model a run uses: remote.model, else the `model` of Claude Code's user settings (what
/// `claude -p` picks without --model). None when neither names one.
pub fn run_model(cfg: &Value) -> Option<String> {
    configured_model(cfg).or_else(|| {
        util::read_json(&claude_config_dir().join("settings.json"))
            .and_then(|v| v.get("model").and_then(Value::as_str).map(|m| m.trim().to_string()))
            .filter(|m| !m.is_empty())
    })
}

/// The stricter of two permission modes (plan < default < acceptEdits); one not allowed, or
/// empty, counts as default.
fn stricter_mode(a: &str, b: &str) -> String {
    let rank = |m: &str| match m {
        "plan" => 0,
        "acceptEdits" => 2,
        _ => 1,
    };
    match rank(a).min(rank(b)) {
        0 => "plan",
        2 => "acceptEdits",
        _ => "default",
    }
    .to_string()
}

/// A session id that can only be read as one: letters, digits and dashes, not starting with one.
fn valid_session_id(id: &str) -> bool {
    !id.is_empty() && !id.starts_with('-') && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}

/// Run `prompt` for `target`. `target_gen`: the phone's next message goes to this session after
/// the run, if the user has not picked another target by then (None: the target stays). `cap`:
/// the permission mode the session stopped in on the PC; the run uses no looser one.
pub fn start(
    app: &AppHandle,
    shared: &Arc<Shared>,
    target: Target,
    prompt: String,
    origin: Origin,
    target_gen: Option<u64>,
    cap: Option<&str>,
) -> Start {
    let x = s(shared);
    // Where the run reports: the paired chat, except for a session continued for the PC while
    // the phone is not set up to answer (the PC shows banners for it).
    let phone = match origin {
        Origin::Continue { pc: true } => crate::phone::answers_cards(shared),
        _ => true,
    };
    let say = |html: &str| if phone { send(shared, html, &[]) } else { None };
    // An elevated overlay would hand its administrator token to the run.
    if crate::win::is_elevated() {
        util::log_line(&["remote-refused", "elevated"]);
        say(x.elevated);
        return Start::Refused(x.elevated.to_string());
    }
    if shared.remote.lock().unwrap().running.is_some() {
        if origin == Origin::Phone {
            send(shared, x.busy, &[]);
        }
        return Start::Busy;
    }
    let cfg = shared.cfg();
    let (cwd, project, resume_id) = match &target {
        Target::New { cwd, project } => (cwd.clone(), project.clone(), None),
        Target::Resume { id, cwd, project, .. } => (cwd.clone(), project.clone(), Some(id.clone())),
    };
    // The working directory must be a known project, whatever the menus said earlier.
    if !is_known_project(&cfg, &cwd) {
        util::log_line(&["remote-refused", "unknown project", &project]);
        say(x.no_projects);
        return Start::Refused(x.no_projects.to_string());
    }
    // Never read as an option (a session id comes from hooks and state.json).
    if resume_id.as_deref().is_some_and(|id| !valid_session_id(id)) {
        util::log_line(&["remote-refused", "bad session id", &project]);
        say(x.no_sessions);
        return Start::Refused(x.no_sessions.to_string());
    }
    let Some(exe) = claude_path(&cfg) else {
        say(x.claude_missing);
        return Start::Refused(x.claude_missing.to_string());
    };
    let wanted = config::get_str(&cfg, "remote.permissionMode").unwrap_or_else(|| "default".to_string());
    let mut mode = if ALLOWED_MODES.contains(&wanted.as_str()) {
        wanted
    } else {
        util::log_line(&["remote-mode-refused", &wanted, "using default"]);
        "default".to_string()
    };
    if let Some(c) = cap {
        let capped = stricter_mode(&mode, c);
        if capped != mode {
            util::log_line(&["remote-mode-capped", &mode, &capped]);
            mode = capped;
        }
    }
    // Shown next to "working…": acceptEdits changes files without asking.
    let mode_note = if mode == "default" { String::new() } else { format!(" · <i>{}</i>", esc(&mode)) };
    let model = configured_model(&cfg);

    let mut cmd = Command::new(&exe);
    cmd.current_dir(&cwd)
        .args(["-p", "--output-format", "stream-json", "--verbose", "--permission-mode", &mode]);
    if let Some(m) = &model {
        cmd.args(["--model", m]);
    }
    if let Some(id) = &resume_id {
        cmd.arg(format!("--resume={id}"));
    }
    match persistent_env() {
        // What a freshly opened terminal would get, not the environment of whichever Claude
        // session happened to start the overlay.
        Some(env) => {
            cmd.env_clear().envs(env);
        }
        None => {
            for k in SESSION_VARS {
                cmd.env_remove(k);
            }
        }
    }
    cmd.env("MCP_CONNECTION_NONBLOCKING", "true")
        // Do not let leftover background work keep the run open for the default 10 minutes.
        .env("CLAUDE_CODE_PRINT_BG_WAIT_CEILING_MS", "120000")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .creation_flags(util::CREATE_NO_WINDOW);

    // Reserve the slot before spawning so two quick messages cannot both start a run.
    let seq = {
        let mut st = shared.remote.lock().unwrap();
        if st.running.is_some() {
            drop(st);
            if origin == Origin::Phone {
                send(shared, x.busy, &[]);
            }
            return Start::Busy;
        }
        st.run_seq += 1;
        let seq = st.run_seq;
        st.running = Some(Running {
            pid: 0,
            seq,
            session_id: resume_id.clone(),
            project: project.clone(),
            started_ms: util::now_ms(),
            stop_requested: false,
            result_sent: false,
            activity: String::new(),
            status_mid: None,
            exited: false,
            job: None,
            pc_continue: origin == (Origin::Continue { pc: true }),
        });
        if let Some(id) = &resume_id {
            st.session_runs.insert(id.clone(), seq);
        }
        seq
    };

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            {
                let mut st = shared.remote.lock().unwrap();
                st.running = None;
                if let Some(id) = &resume_id {
                    if st.session_runs.get(id) == Some(&seq) {
                        st.session_runs.remove(id);
                    }
                }
            }
            util::log_line(&["remote-error", &e.to_string()]);
            say(&format!("{}\n<code>{}</code>", fill(x.failed, &[("p", &esc(&project))]), esc(&e.to_string())));
            return Start::Refused(e.to_string());
        }
    };
    // Into its Job Object before it has the prompt, so it cannot start anything outside it.
    let job = Job::for_child(&child).map(Arc::new);
    if job.is_none() {
        util::log_line(&["remote-error", "no job object for the run, stop uses taskkill"]);
    }
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(prompt.as_bytes());
        // stdin is dropped here, which closes it so claude starts.
    }
    let pid = child.id();
    let started = util::now_ms();
    let stopped_meanwhile = {
        let mut st = shared.remote.lock().unwrap();
        st.last_touch_ms = started;
        match st.running.as_mut() {
            Some(r) if r.seq == seq => {
                r.pid = pid;
                r.started_ms = started;
                r.job = job.clone();
                r.stop_requested
            }
            _ => true,
        }
    };
    if stopped_meanwhile {
        match &job {
            Some(job) => job.kill(),
            None => kill_tree(pid),
        }
    }
    if let Some(id) = &resume_id {
        shared.remote_sessions.lock().unwrap().insert(id.clone());
    }
    let origin_name = match origin {
        Origin::Phone => "phone",
        Origin::Queued => "queued",
        Origin::Continue { pc: false } => "continue",
        Origin::Continue { pc: true } => "continue-pc",
    };
    util::log_line(&["remote-start", &project, resume_id.as_deref().unwrap_or("new"), origin_name]);
    // A run that waited for a limit reset says so first.
    let lead = match origin {
        Origin::Phone => String::new(),
        Origin::Queued => format!("{}\n", x.due_queued),
        Origin::Continue { .. } => format!("{}\n", x.due_continue),
    };
    let status_mid = say(&format!("{lead}{}{mode_note}", fill(x.working, &[("p", &esc(&project))])));
    if let Some(r) = shared.remote.lock().unwrap().running.as_mut() {
        if r.seq == seq {
            r.status_mid = status_mid;
        }
    }

    // stderr: keep the tail for error reports.
    let stderr_tail = Arc::new(std::sync::Mutex::new(String::new()));
    if let Some(err) = child.stderr.take() {
        let tail = stderr_tail.clone();
        std::thread::spawn(move || {
            for line in BufReader::new(err).lines().map_while(Result::ok) {
                let mut t = tail.lock().unwrap();
                t.push_str(&line);
                t.push('\n');
                if t.len() > 4000 {
                    let cut = t.len() - 2000;
                    let idx = t.char_indices().find(|(i, _)| *i >= cut).map(|(i, _)| i).unwrap_or(0);
                    *t = t[idx..].to_string();
                }
            }
        });
    }

    let stdout = child.stdout.take();
    let title_for_new = match &target {
        Target::Resume { title, .. } if !title.is_empty() && origin != Origin::Phone => title.clone(),
        _ => util::truncate(&prompt.split_whitespace().collect::<Vec<_>>().join(" "), 48),
    };
    let ctx = Arc::new(RunCtx {
        seq,
        project: project.clone(),
        cwd: cwd.clone(),
        started,
        status_mid,
        target_gen,
        origin,
        phone,
        lead,
        mode_note,
        title_for_new,
        stderr_tail,
        out: std::sync::Mutex::new(RunOut { session_id: resume_id.clone(), ..RunOut::default() }),
        edit_guard: std::sync::Mutex::new(()),
    });

    // Waiter: owns the process. Normally stdout reaches EOF the moment it exits; if a leftover
    // grandchild keeps the pipe open, the run is finished anyway shortly after the exit.
    {
        let app = app.clone();
        let shared = shared.clone();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let mut child = child;
            let _ = child.wait();
            if let Some(r) = shared.remote.lock().unwrap().running.as_mut() {
                if r.seq == ctx.seq {
                    r.exited = true;
                }
            }
            std::thread::sleep(Duration::from_secs(10));
            // If the reader is still sending the answer, let it finish first (bounded).
            for _ in 0..90 {
                let busy = {
                    let o = ctx.out.lock().unwrap();
                    o.reported && !o.report_done && !o.finalized
                };
                if !busy {
                    break;
                }
                std::thread::sleep(Duration::from_secs(1));
            }
            finalize(&app, &shared, &ctx);
        });
    }

    // Watchdog + progress ticker: edits the status message with the latest step every few
    // seconds, and stops the run after remote.maxRunMinutes.
    {
        let app = app.clone();
        let shared = shared.clone();
        let ctx = ctx.clone();
        let max_ms = config::get_u64(&cfg, "remote.maxRunMinutes", 30).max(1) as i64 * 60_000;
        std::thread::spawn(move || {
            let mut shown = String::new();
            loop {
                std::thread::sleep(Duration::from_secs(4));
                let snapshot = {
                    let st = shared.remote.lock().unwrap();
                    match st.running.as_ref() {
                        Some(r) if r.seq == ctx.seq => Some((r.activity.clone(), r.result_sent, r.stop_requested)),
                        _ => None,
                    }
                };
                let Some((activity, result_sent, stop_requested)) = snapshot else { return };
                if util::now_ms() - ctx.started > max_ms && !stop_requested {
                    util::log_line(&["remote-timeout", &ctx.seq.to_string()]);
                    stop_running(&app, &shared, false);
                    let minutes = (max_ms / 60_000).to_string();
                    let note = fill(s(&shared).timeout, &[("m", &minutes)]);
                    if ctx.phone {
                        send(&shared, &note, &[]);
                    }
                    // finalize() says nothing of a stopped run: the PC is told here.
                    if ctx.origin == (Origin::Continue { pc: true }) {
                        crate::schedule::pc_not_continued(&app, &shared, &ctx.project, &note);
                    }
                    continue;
                }
                if result_sent || stop_requested || activity == shown || activity.is_empty() {
                    continue;
                }
                if let Some(mid) = ctx.status_mid {
                    // Re-check under the edit guard so a progress edit can never land after the
                    // final "done" edit.
                    let _g = ctx.edit_guard.lock().unwrap();
                    let finalized = ctx.out.lock().unwrap().finalized;
                    let still = !finalized && shared
                        .remote
                        .lock()
                        .unwrap()
                        .running
                        .as_ref()
                        .map(|r| r.seq == ctx.seq && !r.result_sent && !r.stop_requested)
                        .unwrap_or(false);
                    if still {
                        let secs = (util::now_ms() - ctx.started) / 1000;
                        let html = format!(
                            "{}{}{} ({secs} s)\n<i>{}</i>",
                            ctx.lead,
                            fill(s(&shared).working, &[("p", &esc(&ctx.project))]),
                            ctx.mode_note,
                            esc(&activity)
                        );
                        telegram::edit(&shared, mid, &html);
                    }
                }
                shown = activity;
            }
        });
    }

    // Reader: parses the stream until EOF (the process may keep writing after the result line
    // while it finishes background work; stopping early would leave its pipe full).
    let app = app.clone();
    let shared = shared.clone();
    std::thread::spawn(move || {
        if let Some(out) = stdout {
            for line in BufReader::new(out).lines().map_while(Result::ok) {
                let Ok(v) = serde_json::from_str::<Value>(line.trim()) else { continue };
                // A used-up usage limit (its StopFailure hook is left to this reader).
                let limit = crate::limits::from_stream(&v);
                if let Some(hit) = &limit {
                    let waiter = crate::limits::Waiter {
                        session_id: ctx.out.lock().unwrap().session_id.clone().unwrap_or_default(),
                        cwd: ctx.cwd.clone(),
                        entrypoint: "remote".to_string(),
                        at_ms: util::now_ms(),
                        ..crate::limits::Waiter::default()
                    };
                    crate::limits::record_hit(&app, &shared, hit.clone(), Some(waiter), "remote");
                }
                match v.get("type").and_then(Value::as_str).unwrap_or("") {
                    "system" if v.get("subtype").and_then(Value::as_str) == Some("init") => {
                        if let Some(id) = v.get("session_id").and_then(Value::as_str) {
                            ctx.out.lock().unwrap().session_id = Some(id.to_string());
                            shared.remote_sessions.lock().unwrap().insert(id.to_string());
                            let mut st = shared.remote.lock().unwrap();
                            st.session_runs.insert(id.to_string(), ctx.seq);
                            if let Some(r) = st.running.as_mut() {
                                if r.seq == ctx.seq {
                                    r.session_id = Some(id.to_string());
                                }
                            }
                        }
                    }
                    "assistant" if v.get("parent_tool_use_id").map(Value::is_null).unwrap_or(true) => {
                        // Is the newest message the limit's error (or was it followed by more)?
                        ctx.out.lock().unwrap().limit_msg = limit.clone();
                        if let Some(blocks) = v.pointer("/message/content").and_then(Value::as_array) {
                            for b in blocks {
                                if let Some(a) = activity_line(b) {
                                    if let Some(r) = shared.remote.lock().unwrap().running.as_mut() {
                                        if r.seq == ctx.seq {
                                            r.activity = a;
                                        }
                                    }
                                }
                                if b.get("type").and_then(Value::as_str) == Some("text") {
                                    let t = b.get("text").and_then(Value::as_str).unwrap_or("");
                                    if !t.trim().is_empty() {
                                        ctx.out.lock().unwrap().last_text = t.to_string();
                                    }
                                }
                            }
                        }
                    }
                    "rate_limit_event" => {
                        feed_rate_limits(&app, &shared, &v);
                        if limit.is_some() {
                            ctx.out.lock().unwrap().limit_info = limit.clone();
                        }
                    }
                    "result" => {
                        // The run ended because the usage limit ran out: its continue at the
                        // reset is planned in finalize.
                        {
                            let mut o = ctx.out.lock().unwrap();
                            let is_error = v.get("is_error").and_then(Value::as_bool).unwrap_or(false);
                            let ended = limit.clone().or_else(|| if is_error { o.limit_msg.clone() } else { None });
                            o.limit = ended.map(|mut h| {
                                if let (None, Some(info)) = (h.resets_at, &o.limit_info) {
                                    h.resets_at = info.resets_at;
                                    h.reset_exact = info.reset_exact;
                                }
                                h
                            });
                        }
                        // Mark it before waiting for the guard, so a waiter-side finalize waits
                        // for this report instead of declaring the run failed.
                        let first = {
                            let mut o = ctx.out.lock().unwrap();
                            let first = !o.reported && !o.finalized;
                            if first {
                                o.reported = true;
                            }
                            first
                        };
                        let _g = ctx.edit_guard.lock().unwrap();
                        let first = first && !ctx.out.lock().unwrap().finalized;
                        if first {
                            // Not finalized yet, so the run is still registered.
                            let stopped = {
                                let mut st = shared.remote.lock().unwrap();
                                match st.running.as_mut() {
                                    Some(r) if r.seq == ctx.seq => {
                                        r.result_sent = true;
                                        r.stop_requested
                                    }
                                    _ => true,
                                }
                            };
                            if !stopped {
                                report(&shared, &ctx, &v);
                                ctx.out.lock().unwrap().report_ran = true;
                            }
                            ctx.out.lock().unwrap().report_done = true;
                        }
                    }
                    _ => {}
                }
            }
        }
        finalize(&app, &shared, &ctx);
    });
    Start::Started
}

/// Fixed facts about one run, shared by its reader, waiter and ticker threads.
struct RunCtx {
    seq: u64,
    project: String,
    cwd: String,
    started: i64,
    status_mid: Option<i64>,
    /// The phone's target generation when the run started (None: the run never moves the target).
    target_gen: Option<u64>,
    origin: Origin,
    /// It reports to the paired chat.
    phone: bool,
    /// The first line of its status message ("The limit reset: …"), or empty.
    lead: String,
    /// " · <i>plan</i>" etc. after "working…" when the mode is not the default.
    mode_note: String,
    title_for_new: String,
    stderr_tail: Arc<std::sync::Mutex<String>>,
    out: std::sync::Mutex<RunOut>,
    /// Serializes status-message edits between the ticker and the final report.
    edit_guard: std::sync::Mutex<()>,
}

#[derive(Default)]
struct RunOut {
    session_id: Option<String>,
    /// The result line was seen.
    reported: bool,
    /// The reader has finished handling the result line.
    report_done: bool,
    /// report() actually ran (the answer was sent).
    report_ran: bool,
    ok: bool,
    last_text: String,
    finalized: bool,
    /// The newest top-level message was the usage limit's error.
    limit_msg: Option<crate::limits::Hit>,
    /// The last rejected rate-limit report (its reset).
    limit_info: Option<crate::limits::Hit>,
    /// The run ended because the usage limit ran out.
    limit: Option<crate::limits::Hit>,
}

/// The result line arrived: final status edit and the answer itself.
fn report(shared: &Arc<Shared>, ctx: &RunCtx, r: &Value) {
    let x = s(shared);
    let elapsed = (util::now_ms() - ctx.started) / 1000;
    let is_error = r.get("is_error").and_then(Value::as_bool).unwrap_or(false);
    let turns = r.get("num_turns").and_then(Value::as_i64).unwrap_or(0);
    let last_text = {
        let mut o = ctx.out.lock().unwrap();
        if let Some(id) = r.get("session_id").and_then(Value::as_str) {
            o.session_id = Some(id.to_string());
        }
        o.ok = !is_error;
        o.last_text.clone()
    };
    let head = if is_error { fill(x.failed, &[("p", &esc(&ctx.project))]) } else { fill(x.done, &[("p", &esc(&ctx.project))]) };
    if let Some(mid) = ctx.status_mid {
        telegram::edit(shared, mid, &format!("{head} · {turns} {} · {elapsed} s", x.turns));
    }
    let mut body = r.get("result").and_then(Value::as_str).unwrap_or("").to_string();
    if body.trim().is_empty() {
        body = last_text;
    }
    if !ctx.phone {
        return;
    }
    let chunks = escaped_chunks(&body);
    let total = chunks.len();
    for (i, c) in chunks.into_iter().take(4).enumerate() {
        let mut html = c;
        if i == 3 && total > 4 {
            html.push('\n');
            html.push_str(x.truncated);
        }
        send(shared, &html, &[]);
    }
}

/// End of a run (stdout EOF, or shortly after the process exited). Runs once.
fn finalize(app: &AppHandle, shared: &Arc<Shared>, ctx: &RunCtx) {
    let guard = ctx.edit_guard.lock().unwrap();
    let (session_id, reported, ok, last_text, report_ran) = {
        let mut o = ctx.out.lock().unwrap();
        if o.finalized {
            return;
        }
        o.finalized = true;
        (o.session_id.clone(), o.reported, o.ok, o.last_text.clone(), o.report_ran)
    };
    let x = s(shared);
    let elapsed = (util::now_ms() - ctx.started) / 1000;
    let stop_requested = shared
        .remote
        .lock()
        .unwrap()
        .running
        .as_ref()
        .map(|r| r.seq == ctx.seq && r.stop_requested)
        .unwrap_or(false);
    if stop_requested && !report_ran {
        if let Some(mid) = ctx.status_mid {
            telegram::edit(shared, mid, &format!("{} · {elapsed} s", x.stopped));
        }
    } else if !reported {
        if stop_requested {
            if let Some(mid) = ctx.status_mid {
                telegram::edit(shared, mid, &format!("{} · {elapsed} s", x.stopped));
            }
        } else {
            let tail = ctx.stderr_tail.lock().unwrap().clone();
            let detail = if tail.trim().is_empty() { last_text } else { tail };
            // Escape first, then size: the end of the output is the useful part.
            let body = escaped_chunks(detail.trim()).pop().unwrap_or_default();
            let html = format!("{}\n<code>{}</code>", fill(x.failed, &[("p", &esc(&ctx.project))]), body);
            match ctx.status_mid {
                Some(mid) => telegram::edit(shared, mid, &html),
                None if ctx.phone => {
                    send(shared, &html, &[]);
                }
                None => {}
            }
        }
    } else if ok && !stop_requested && ctx.phone && ctx.target_gen.is_some() {
        send(shared, &format!("<i>{}</i>", x.reply_hint), &[]);
    }

    // Cards of this run that are still open would otherwise swallow later messages.
    if let Some(id) = &session_id {
        events::cancel_pending_for_session(app, shared, id);
    }

    // The next message continues this session, unless the user picked something else while
    // the run was going.
    let mut title = ctx.title_for_new.clone();
    {
        let mut st = shared.remote.lock().unwrap();
        if let Some(id) = &session_id {
            st.own_runs.insert(id.clone(), util::now_ms());
            if let Some(Target::Resume { id: tid, title: t, .. }) = &st.target {
                if tid == id {
                    title = t.clone();
                }
            }
            if ctx.target_gen == Some(st.target_gen) {
                st.target = Some(Target::Resume { id: id.clone(), cwd: ctx.cwd.clone(), project: ctx.project.clone(), title: title.clone() });
            }
        }
        if st.running.as_ref().map(|r| r.seq == ctx.seq).unwrap_or(false) {
            st.running = None;
        }
        st.last_touch_ms = util::now_ms();
    }
    util::log_line(&["remote-end", &ctx.project, session_id.as_deref().unwrap_or("-")]);
    drop(guard);

    // Ended because the usage limit ran out: continued at the reset, but a continue never
    // continues itself again (no chain of resets without a new message from the user).
    let limit = ctx.out.lock().unwrap().limit.clone();
    match (limit, &session_id) {
        (Some(_), _) if stop_requested => {}
        (Some(_), _) if matches!(ctx.origin, Origin::Continue { .. }) => {
            util::log_line(&["schedule", "limit-again", &ctx.project]);
            if ctx.phone {
                send(shared, crate::schedule::s(shared).again, &[]);
            }
        }
        (Some(hit), Some(id)) => crate::schedule::after_remote_limit(shared, id, &ctx.cwd, &ctx.project, &title, &hit),
        _ => {}
    }
    // A session continued for the PC that did not get an answer: the PC is told.
    if ctx.origin == (Origin::Continue { pc: true }) && !stop_requested && (!reported || !ok) {
        let detail = if reported { String::new() } else { util::truncate(ctx.stderr_tail.lock().unwrap().trim(), 160) };
        crate::schedule::pc_not_continued(app, shared, &ctx.project, &detail);
    }

    // Hooks of this session keep being treated as remote for a little while, unless a newer
    // run on the same session has started meanwhile (that run owns the entry).
    if let Some(id) = session_id {
        let shared2 = shared.clone();
        let seq = ctx.seq;
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_secs(120));
            let newer = {
                let st = shared2.remote.lock().unwrap();
                st.session_runs.get(&id).map(|s| *s != seq).unwrap_or(false) || is_current_session(&st, &id)
            };
            if !newer {
                shared2.remote_sessions.lock().unwrap().remove(&id);
            }
        });
    }
}

fn is_current_session(st: &RemoteState, id: &str) -> bool {
    st.running.as_ref().and_then(|r| r.session_id.as_deref()).map(|s| s == id).unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expands_like_windows() {
        let env = vec![("SystemRoot".to_string(), r"C:\Windows".to_string())];
        assert_eq!(expand(r"%SYSTEMROOT%\x;%NOPE%;%%;50%", &env), r"C:\Windows\x;%NOPE%;%%;50%");
        assert_eq!(expand("%NOPE%%SystemRoot%", &env), r"%NOPE%C:\Windows");
    }

    #[test]
    fn path_expands_variables_defined_after_it() {
        let s = |v: &str| v.to_string();
        let base = vec![(s("SystemRoot"), s(r"C:\Windows"))];
        let machine = vec![
            (s("Path"), s(r"%SystemRoot%\system32;%JAVA_HOME%\bin"), true),
            (s("JAVA_HOME"), s(r"%TOOLS%\jdk"), true),
            (s("TOOLS"), s(r"C:\tools"), false),
        ];
        let user = vec![(s("Path"), s(r"%LATER%\bin"), true), (s("LATER"), s(r"C:\later"), false)];
        let env = build_env(base, machine, user);
        assert_eq!(env_get(&env, "Path"), Some(r"C:\Windows\system32;C:\tools\jdk\bin;C:\later\bin"));
        assert_eq!(env_get(&env, "JAVA_HOME"), Some(r"C:\tools\jdk"));
    }

    #[test]
    fn persistent_env_is_a_fresh_terminal_env() {
        let env = persistent_env().expect("registry readable");
        let path = env_get(&env, "Path").unwrap_or("");
        assert!(path.to_lowercase().contains("system32"), "{path}");
        assert!(!path.contains('%'), "unexpanded: {path}");
        assert_ne!(env_get(&env, "USERNAME").map(str::to_uppercase).as_deref(), Some("SYSTEM"));
        assert!(env_get(&env, "SystemRoot").is_some());
    }

    #[test]
    fn continue_mode_is_capped_and_ids_are_never_options() {
        assert_eq!(stricter_mode("acceptEdits", "plan"), "plan");
        assert_eq!(stricter_mode("acceptEdits", "default"), "default");
        assert_eq!(stricter_mode("acceptEdits", "acceptEdits"), "acceptEdits");
        assert_eq!(stricter_mode("plan", "acceptEdits"), "plan");
        // Not known, or a mode a run never gets: default.
        assert_eq!(stricter_mode("acceptEdits", ""), "default");
        assert_eq!(stricter_mode("acceptEdits", "bypassPermissions"), "default");
        assert_eq!(stricter_mode("acceptEdits", "auto"), "default");
        assert!(valid_session_id("0b7d2f3e-1c2a-4f5e-9a8b-123456789abc"));
        for bad in ["", "-x", "--permission-mode=bypassPermissions", "a b", "a/b", "a=b"] {
            assert!(!valid_session_id(bad), "{bad}");
        }
    }

    #[test]
    fn chunks_are_masked_as_a_whole() {
        let text = format!("{}token=hunter2abc\n-----BEGIN PRIVATE KEY-----\nMIIEsecret\n-----END PRIVATE KEY-----", "x".repeat(5000));
        let all = escaped_chunks(&text).join("");
        assert!(!all.contains("hunter2abc") && !all.contains("MIIEsecret"), "{all}");
        assert!(escaped_chunks(&"é<".repeat(4000)).iter().all(|c| c.chars().count() <= CHUNK_MAX));
    }

    #[test]
    fn job_ends_the_whole_tree() {
        // cmd starts ping as its child, and ping holds the stdout pipe too: the pipe only
        // reaches EOF once both have ended.
        for by_drop in [false, true] {
            let mut child = Command::new(crate::win::system_dir().join("cmd.exe"))
                .args(["/d", "/c", "ping -n 30 127.0.0.1"])
                .creation_flags(util::CREATE_NO_WINDOW)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
                .unwrap();
            let job = Job::for_child(&child).expect("job object");
            std::thread::sleep(Duration::from_millis(500));
            let started = std::time::Instant::now();
            if by_drop {
                drop(job);
            } else {
                job.kill();
            }
            let mut out = Vec::new();
            let _ = child.stdout.take().unwrap().read_to_end(&mut out);
            let _ = child.wait();
            assert!(started.elapsed() < Duration::from_secs(5), "tree still running after kill (drop: {by_drop})");
        }
    }
}
