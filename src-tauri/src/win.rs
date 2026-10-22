//! Small Win32 helpers: never-activating window style, a message box for CLI mode, process
//! liveness, token elevation, mouse clicks anywhere on the screen and the double-click time.
use std::os::windows::ffi::OsStringExt;
use std::path::PathBuf;
use std::sync::OnceLock;
use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, ERROR_ACCESS_DENIED, HANDLE, HWND, RECT, STILL_ACTIVE};
use windows_sys::Win32::Security::{GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY};
use windows_sys::Win32::System::SystemInformation::{GetSystemDirectoryW, GetTickCount};
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, GetExitCodeProcess, OpenProcess, OpenProcessToken, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, GetDoubleClickTime, GetLastInputInfo, LASTINPUTINFO, VK_LBUTTON, VK_MBUTTON, VK_RBUTTON,
    VK_XBUTTON1, VK_XBUTTON2,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    GetForegroundWindow, GetWindow, GetWindowLongPtrW, GetWindowRect, IsIconic, IsWindowVisible, MessageBoxW,
    SetForegroundWindow, SetWindowLongPtrW, SetWindowPos, GWL_EXSTYLE, GW_HWNDPREV, HWND_NOTOPMOST, HWND_TOPMOST,
    MB_ICONINFORMATION, MB_OK, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOOWNERZORDER, SWP_NOSIZE, WS_EX_NOACTIVATE,
    WS_EX_TOOLWINDOW, WS_EX_TOPMOST,
};

fn hwnd_of(window: &tauri::WebviewWindow) -> Option<HWND> {
    let h = window.hwnd().ok()?;
    Some(h.0 as isize as HWND)
}

/// WS_EX_NOACTIVATE: clicks on the overlay never take focus away from the game or app
/// underneath. WS_EX_TOOLWINDOW keeps it out of Alt+Tab. tao rewrites the extended style
/// from its own flags whenever something like `set_ignore_cursor_events` is called, so this
/// is re-applied after every such call (cheap: it only writes when the bits are missing).
pub fn ensure_noactivate(window: &tauri::WebviewWindow) {
    if let Some(h) = hwnd_of(window) {
        unsafe {
            let style = GetWindowLongPtrW(h, GWL_EXSTYLE);
            let wanted = style | WS_EX_NOACTIVATE as isize | WS_EX_TOOLWINDOW as isize;
            if wanted != style {
                SetWindowLongPtrW(h, GWL_EXSTYLE, wanted);
            }
        }
    }
}

/// Put the overlay at the top of the always-on-top band. tao's `set_always_on_top(true)`
/// does nothing while its flag is already set, but the window can still end up below other
/// windows with WS_EX_TOPMOST set (seen after hours next to a fullscreen video in a browser),
/// so the position itself is re-asserted here. `force` first drops and then restores the
/// flag, which re-inserts a window whose z-order is stuck.
pub fn raise_topmost(window: &tauri::WebviewWindow, force: bool) {
    if let Some(h) = hwnd_of(window) {
        let flags = SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_NOOWNERZORDER;
        unsafe {
            if force {
                SetWindowPos(h, HWND_NOTOPMOST, 0, 0, 0, 0, flags);
            }
            SetWindowPos(h, HWND_TOPMOST, 0, 0, 0, 0, flags);
        }
    }
}

/// True when a visible window without WS_EX_TOPMOST is above the overlay in the z-order and
/// overlaps it: the overlay has lost its always-on-top position.
pub fn is_buried(window: &tauri::WebviewWindow) -> bool {
    let Some(h) = hwnd_of(window) else { return false };
    unsafe {
        let mut mine: RECT = std::mem::zeroed();
        if GetWindowRect(h, &mut mine) == 0 {
            return false;
        }
        let mut above = GetWindow(h, GW_HWNDPREV);
        // Bounded walk: the z-order can change while it is read.
        for _ in 0..512 {
            if above.is_null() {
                return false;
            }
            if IsWindowVisible(above) != 0 && IsIconic(above) == 0 {
                let ex = GetWindowLongPtrW(above, GWL_EXSTYLE) as u32;
                let mut r: RECT = std::mem::zeroed();
                let overlaps = GetWindowRect(above, &mut r) != 0
                    && r.left < mine.right
                    && r.right > mine.left
                    && r.top < mine.bottom
                    && r.bottom > mine.top
                    && r.right > r.left
                    && r.bottom > r.top;
                if overlaps && ex & WS_EX_TOPMOST == 0 {
                    return true;
                }
            }
            above = GetWindow(above, GW_HWNDPREV);
        }
        false
    }
}

/// Temporarily let the overlay take keyboard focus (free-text answers).
pub fn allow_activate(window: &tauri::WebviewWindow) {
    if let Some(h) = hwnd_of(window) {
        unsafe {
            let style = GetWindowLongPtrW(h, GWL_EXSTYLE);
            SetWindowLongPtrW(h, GWL_EXSTYLE, style & !(WS_EX_NOACTIVATE as isize));
            SetForegroundWindow(h);
        }
    }
}

pub fn foreground_hwnd() -> isize {
    unsafe { GetForegroundWindow() as isize }
}

pub fn set_foreground_raw(hwnd: isize) {
    if hwnd != 0 {
        unsafe {
            SetForegroundWindow(hwnd as HWND);
        }
    }
}

/// Milliseconds since the last keyboard or mouse input on this PC.
pub fn idle_ms() -> u64 {
    unsafe {
        let mut info = LASTINPUTINFO { cbSize: std::mem::size_of::<LASTINPUTINFO>() as u32, dwTime: 0 };
        if GetLastInputInfo(&mut info) == 0 {
            return 0;
        }
        GetTickCount().wrapping_sub(info.dwTime) as u64
    }
}

/// Mouse clicks anywhere on the screen, seen by polling the physical button state (no hook):
/// `poll` is true when any button went down since the last poll. A press already held at the
/// first poll counts too, so the caller ignores what it reports for a moment after starting.
#[derive(Default)]
pub struct ClickWatch {
    down: bool,
}

impl ClickWatch {
    pub fn poll(&mut self) -> bool {
        let (mut down, mut tapped) = (false, false);
        for vk in [VK_LBUTTON, VK_RBUTTON, VK_MBUTTON, VK_XBUTTON1, VK_XBUTTON2] {
            let state = unsafe { GetAsyncKeyState(vk as i32) } as u16;
            down |= state & 0x8000 != 0;
            // Pressed since the last call: catches a click shorter than the poll interval.
            tapped |= state & 1 != 0;
        }
        let clicked = !self.down && (down || tapped);
        self.down = down;
        clicked
    }
}

/// The user's double-click time (Mouse settings), in ms, kept within sane bounds.
pub fn double_click_ms() -> i64 {
    (unsafe { GetDoubleClickTime() } as i64).clamp(200, 900)
}

pub fn message_box(title: &str, text: &str) {
    let t: Vec<u16> = title.encode_utf16().chain(std::iter::once(0)).collect();
    let x: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        MessageBoxW(std::ptr::null_mut(), x.as_ptr(), t.as_ptr(), MB_OK | MB_ICONINFORMATION);
    }
}

/// The Windows system directory (System32), from the OS rather than PATH or the exe folder.
pub fn system_dir() -> PathBuf {
    let mut buf = [0u16; 260];
    let n = unsafe { GetSystemDirectoryW(buf.as_mut_ptr(), buf.len() as u32) } as usize;
    if n > 0 && n < buf.len() {
        return PathBuf::from(std::ffi::OsString::from_wide(&buf[..n]));
    }
    let root = std::env::var_os("SystemRoot").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(r"C:\Windows"));
    root.join("System32")
}

/// True while process `pid` is running. A process we may not query (another account, a
/// protected process) counts as running; only a pid that no longer exists, or one that has
/// exited, counts as gone.
pub fn process_alive(pid: u32) -> bool {
    unsafe {
        let h: HANDLE = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if h.is_null() {
            return GetLastError() == ERROR_ACCESS_DENIED;
        }
        let mut code: u32 = 0;
        let ok = GetExitCodeProcess(h, &mut code) != 0;
        CloseHandle(h);
        !ok || code == STILL_ACTIVE as u32
    }
}

/// True when the overlay runs with an elevated (Administrator) token. Cached: elevation
/// cannot change during the life of a process.
pub fn is_elevated() -> bool {
    static ELEVATED: OnceLock<bool> = OnceLock::new();
    *ELEVATED.get_or_init(|| unsafe {
        let mut token: HANDLE = std::ptr::null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            return false;
        }
        let mut elevation = TOKEN_ELEVATION { TokenIsElevated: 0 };
        let mut len = 0u32;
        let ok = GetTokenInformation(
            token,
            TokenElevation,
            &mut elevation as *mut TOKEN_ELEVATION as *mut core::ffi::c_void,
            std::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut len,
        ) != 0;
        CloseHandle(token);
        ok && elevation.TokenIsElevated != 0
    })
}
