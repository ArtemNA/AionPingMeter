// The overlay window (layered, click-through, always on top) and the tray icon with its menu.

use std::cell::RefCell;
use std::collections::HashMap;
use std::ptr::{null, null_mut};
use std::sync::Arc;
use std::sync::atomic::Ordering::Relaxed;

use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, SIZE, WPARAM};
use windows_sys::Win32::Graphics::Gdi::{
    AC_SRC_ALPHA, AC_SRC_OVER, BLENDFUNCTION, CreateCompatibleDC, DeleteDC, DeleteObject, GetDC, GetMonitorInfoW,
    MONITOR_DEFAULTTONULL, MONITOR_DEFAULTTOPRIMARY, MONITORINFO, MonitorFromPoint, ReleaseDC, SelectObject,
};
use windows_sys::Win32::System::DataExchange::{CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock};
use windows_sys::Win32::UI::HiDpi::GetDpiForWindow;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::ReleaseCapture;
use windows_sys::Win32::UI::Shell::{
    NIF_ICON, NIF_INFO, NIF_MESSAGE, NIF_TIP, NIIF_INFO, NIM_ADD, NIM_DELETE, NIM_MODIFY, NOTIFYICONDATAW,
    Shell_NotifyIconW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::*;

use crate::render::{Color, OverlayModel, Renderer, tray_icon};
use crate::settings;
use crate::stats::{PingStats, SPIKE_FLASH_MS};
use crate::util::{copy_wide, f0, wide};

const GOOD: Color = Color::rgb(110, 220, 130);
const FAIR: Color = Color::rgb(240, 200, 90);
const POOR: Color = Color::rgb(255, 110, 100);
const NO_DATA: Color = Color::rgb(140, 146, 156);
const ALARM_RED: Color = Color::rgb(255, 90, 80);

const THRESHOLDS: [i32; 5] = [80, 100, 150, 200, 300];

const WM_TRAY: u32 = WM_APP + 1;
const TIMER_ID: usize = 1;
const CF_UNICODETEXT: u32 = 13;

fn taskbar_created() -> u32 {
    static MSG: std::sync::OnceLock<u32> = std::sync::OnceLock::new();
    *MSG.get_or_init(|| unsafe { RegisterWindowMessageW(wide("TaskbarCreated").as_ptr()) })
}

// Menu command ids.
const CMD_SERVER: usize = 10;
const CMD_SHOW_SERVER: usize = 11;
const CMD_MOVE: usize = 12;
const CMD_RESET: usize = 13;
const CMD_LOG: usize = 14;
const CMD_EXIT: usize = 15;
const CMD_OPEN_LOG: usize = 16;
const CMD_THRESHOLD: usize = 100; // + index in THRESHOLDS

struct App {
    stats: Arc<PingStats>,
    renderer: Renderer,
    overlay: HWND,
    tray_owner: HWND,
    locked: bool,
    show_server: bool,
    model: Option<OverlayModel>,
    tray_color: Option<Color>,
    icons: HashMap<Color, HICON>,
    tip: String,
    tick: u32,
}

thread_local! {
    static APP: RefCell<Option<App>> = const { RefCell::new(None) };
}

fn with_app<R>(f: impl FnOnce(&mut App) -> R) -> Option<R> {
    APP.with(|a| a.try_borrow_mut().ok().and_then(|mut a| a.as_mut().map(f)))
}

/// Creates the overlay and the tray icon and runs the message loop until Exit.
pub fn run(stats: Arc<PingStats>) {
    stats.threshold_ms.store(settings::load_threshold(), Relaxed);
    unsafe {
        let hinstance = GetModuleHandleW(null());
        let class = wide("AionPingMeter.Overlay");
        let wc = WNDCLASSEXW {
            cbSize: size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(wndproc),
            hInstance: hinstance,
            hCursor: LoadCursorW(null_mut(), IDC_ARROW),
            lpszClassName: class.as_ptr(),
            hIcon: LoadIconW(hinstance, 1 as _),
            ..std::mem::zeroed()
        };
        RegisterClassExW(&wc);

        let title = wide("AionPingMeter");
        let (x, y) = initial_position();
        let overlay = CreateWindowExW(
            WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_TOPMOST | WS_EX_LAYERED | WS_EX_TRANSPARENT,
            class.as_ptr(),
            title.as_ptr(),
            WS_POPUP,
            x,
            y,
            1,
            1,
            null_mut(),
            null_mut(),
            hinstance,
            null(),
        );
        // A hidden window owns the tray icon and its menu (the overlay itself never takes focus).
        let tray_owner =
            CreateWindowExW(WS_EX_TOOLWINDOW, class.as_ptr(), title.as_ptr(), WS_POPUP, 0, 0, 0, 0, null_mut(), null_mut(), hinstance, null());
        if overlay.is_null() || tray_owner.is_null() {
            return;
        }

        APP.with(|a| {
            *a.borrow_mut() = Some(App {
                stats,
                renderer: Renderer::new(),
                overlay,
                tray_owner,
                locked: true,
                show_server: settings::load_show_server(),
                model: None,
                tray_color: None,
                icons: HashMap::new(),
                tip: String::new(),
                tick: 0,
            })
        });

        with_app(|app| {
            app.add_tray();
            app.update_view();
        });
        ShowWindow(overlay, SW_SHOWNOACTIVATE);
        SetTimer(overlay, TIMER_ID, 250, None);

        let mut msg: MSG = std::mem::zeroed();
        while GetMessageW(&mut msg, null_mut(), 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }

        if let Some(app) = APP.with(|a| a.borrow_mut().take()) {
            app.stats.set_logging(false);
            for (_, icon) in app.icons {
                DestroyIcon(icon);
            }
        }
    }
}

/// The saved position if it is still on some monitor's work area, else the top-left corner of the primary monitor.
fn initial_position() -> (i32, i32) {
    if let Some((x, y)) = settings::load_position() {
        unsafe {
            let m = MonitorFromPoint(POINT { x, y }, MONITOR_DEFAULTTONULL);
            if !m.is_null() {
                let mut mi = MONITORINFO { cbSize: size_of::<MONITORINFO>() as u32, ..std::mem::zeroed() };
                if GetMonitorInfoW(m, &mut mi) != 0 && contains(&mi.rcWork, x, y) {
                    return (x, y);
                }
            }
        }
    }
    default_location()
}

fn contains(r: &RECT, x: i32, y: i32) -> bool {
    x >= r.left && x < r.right && y >= r.top && y < r.bottom
}

fn default_location() -> (i32, i32) {
    unsafe {
        let m = MonitorFromPoint(POINT { x: 0, y: 0 }, MONITOR_DEFAULTTOPRIMARY);
        let mut mi = MONITORINFO { cbSize: size_of::<MONITORINFO>() as u32, ..std::mem::zeroed() };
        GetMonitorInfoW(m, &mut mi);
        (mi.rcWork.left + 24, mi.rcWork.top + 24)
    }
}

impl App {
    fn notify_data(&self) -> NOTIFYICONDATAW {
        let mut nid: NOTIFYICONDATAW = unsafe { std::mem::zeroed() };
        nid.cbSize = size_of::<NOTIFYICONDATAW>() as u32;
        nid.hWnd = self.tray_owner;
        nid.uID = 1;
        nid
    }

    fn icon(&mut self, c: Color) -> HICON {
        let size = unsafe { GetSystemMetrics(SM_CXSMICON) };
        *self.icons.entry(c).or_insert_with(|| tray_icon(c, size))
    }

    fn add_tray(&mut self) {
        let icon = self.icon(NO_DATA);
        let mut nid = self.notify_data();
        nid.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP;
        nid.uCallbackMessage = WM_TRAY;
        nid.hIcon = icon;
        copy_wide(&mut nid.szTip, "AionPingMeter");
        unsafe { Shell_NotifyIconW(NIM_ADD, &nid) };
        self.tray_color = Some(NO_DATA);
    }

    fn remove_tray(&self) {
        let nid = self.notify_data();
        unsafe { Shell_NotifyIconW(NIM_DELETE, &nid) };
    }

    fn balloon(&self, text: &str) {
        let mut nid = self.notify_data();
        nid.uFlags = NIF_INFO;
        nid.dwInfoFlags = NIIF_INFO;
        copy_wide(&mut nid.szInfoTitle, "AionPingMeter");
        copy_wide(&mut nid.szInfo, text);
        unsafe { Shell_NotifyIconW(NIM_MODIFY, &nid) };
    }

    fn set_tray_icon(&mut self, c: Color) {
        if self.tray_color == Some(c) {
            return;
        }
        let icon = self.icon(c);
        let mut nid = self.notify_data();
        nid.uFlags = NIF_ICON;
        nid.hIcon = icon;
        unsafe { Shell_NotifyIconW(NIM_MODIFY, &nid) };
        self.tray_color = Some(c);
    }

    fn set_tip(&mut self, tip: String) {
        if tip == self.tip {
            return;
        }
        let mut nid = self.notify_data();
        nid.uFlags = NIF_TIP;
        copy_wide(&mut nid.szTip, &tip); // Windows tray tooltip limit: 127 characters
        unsafe { Shell_NotifyIconW(NIM_MODIFY, &nid) };
        self.tip = tip;
    }

    fn set_locked(&mut self, locked: bool) {
        self.locked = locked;
        unsafe {
            let ex = GetWindowLongPtrW(self.overlay, GWL_EXSTYLE);
            let t = WS_EX_TRANSPARENT as isize;
            SetWindowLongPtrW(self.overlay, GWL_EXSTYLE, if locked { ex | t } else { ex & !t });
        }
        if locked {
            self.save_position();
        }
        self.update_view();
    }

    fn save_position(&self) {
        let mut r: RECT = unsafe { std::mem::zeroed() };
        unsafe { GetWindowRect(self.overlay, &mut r) };
        settings::save_position(r.left, r.top);
    }

    fn reset_position(&self) {
        let (x, y) = default_location();
        unsafe { SetWindowPos(self.overlay, null_mut(), x, y, 0, 0, SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE) };
        settings::save_position(x, y);
    }

    fn toggle_log(&self) {
        let enable = !self.stats.is_logging();
        if let Some(path) = self.stats.set_logging(enable) {
            self.balloon(&format!("Log: {}", path.display()));
        }
    }

    /// Opens Explorer with the log file selected. The balloon with the path is hidden while
    /// Windows is in "do not disturb" (e.g. during a game), so the menu offers this instead.
    fn open_log(&self) {
        use std::os::windows::process::CommandExt;
        let path = settings::dir().join("ping-log.csv");
        let _ = std::process::Command::new("explorer.exe").raw_arg(format!("/select,\"{}\"", path.display())).spawn();
    }

    fn copy_server(&self) {
        if let Some(t) = self.stats.game_endpoint_text()
            && set_clipboard(self.tray_owner, &t) {
                self.balloon(&format!("Copied: {t}"));
            }
    }

    fn update_view(&mut self) {
        let stats = &self.stats;
        let (cur, _, max) = stats.snapshot();
        let (net, _) = stats.tcp_snapshot();

        let quality = match cur {
            Some(c) if c < 60.0 => GOOD,
            Some(c) if c < 120.0 => FAIR,
            Some(_) => POOR,
            None => NO_DATA,
        };

        // Alert pill — problems in the last minute: red for the first 10 s (plus a red outline), then orange.
        let (mut head, mut tail, mut flash) = (None, None, false);
        if let Some(sp) = stats.spike_snapshot() {
            flash = sp.since_last_ms < SPIKE_FLASH_MS;
            let mut parts: Vec<String> = Vec::with_capacity(5);
            if let Some(gm) = sp.game_max {
                head = Some(format!("ping {}", f0(gm)));
                if let Some(gc) = sp.game_cause {
                    parts.push(gc.into());
                }
            }
            if let Some(nm) = sp.network_max {
                parts.push(format!("network {}", f0(nm)));
            }
            if let Some(fm) = sp.freeze_max {
                parts.push(format!("freeze {}", f0(fm)));
            }
            if sp.losses > 0 {
                parts.push(format!("loss ×{}", sp.losses));
            }
            if head.is_none() && !parts.is_empty() {
                head = Some(parts.remove(0));
            }
            parts.push(if sp.since_last_ms < 1000 { "just now".into() } else { format!("{}s ago", sp.since_last_ms / 1000) });
            tail = Some(parts.join(" · "));
        }

        let threshold = stats.threshold_ms.load(Relaxed) as f64;
        let model = OverlayModel {
            quality,
            ping: cur.map_or("—".into(), f0),
            network: cur.map(|_| net.map_or("—".into(), f0)),
            max: cur.map(|_| f0(max)),
            max_hot: flash && max >= threshold,
            status: if cur.is_none() { Some(stats.status()) } else { None },
            alert_head: head,
            alert_tail: tail,
            flash,
            server: self.show_server.then(|| stats.game_endpoint_text().unwrap_or_else(|| "server —".into())),
            unlocked: !self.locked,
        };

        if self.model.as_ref() != Some(&model) {
            self.set_tray_icon(if model.flash { ALARM_RED } else { model.quality });
            let dpi = unsafe { GetDpiForWindow(self.overlay) }.max(96);
            if let Some((hbmp, w, h)) = self.renderer.render_hbitmap(&model, dpi as f32 / 96.0) {
                update_layered(self.overlay, hbmp, w, h);
                unsafe { DeleteObject(hbmp) };
            }
            self.model = Some(model);
        }

        self.tick += 1;
        if self.tick.is_multiple_of(4) {
            let server = self.stats.game_endpoint_text();
            let tip = format!(
                "{}\npings {} · loss {} · freeze {}\ngateway changes {}",
                server.as_ref().map_or("AionPingMeter".into(), |s| format!("AionPingMeter · {s}")),
                self.stats.pings.load(Relaxed),
                self.stats.losses.load(Relaxed),
                self.stats.freezes.load(Relaxed),
                self.stats.gateway_changes.load(Relaxed)
            );
            self.set_tip(tip);
        }
    }

    /// Builds the tray menu with the current state.
    fn build_menu(&self) -> HMENU {
        unsafe {
            let menu = CreatePopupMenu();
            let add = |m: HMENU, flags: u32, id: usize, text: &str| {
                let w = wide(text);
                AppendMenuW(m, flags, id, w.as_ptr());
            };
            let checked = |b: bool| if b { MF_CHECKED } else { MF_UNCHECKED };

            add(menu, MF_STRING | MF_GRAYED, 0, &format!("AionPingMeter v{}", env!("CARGO_PKG_VERSION")));
            AppendMenuW(menu, MF_SEPARATOR, 0, null());
            match self.stats.game_endpoint_text() {
                Some(s) => add(menu, MF_STRING, CMD_SERVER, &format!("Server: {s}  (click to copy)")),
                None => add(menu, MF_STRING | MF_GRAYED, CMD_SERVER, "Server: —"),
            }
            add(menu, MF_STRING | checked(self.show_server), CMD_SHOW_SERVER, "Show server in overlay");
            AppendMenuW(menu, MF_SEPARATOR, 0, null());
            add(menu, MF_STRING, CMD_MOVE, if self.locked { "Move" } else { "Lock position" });
            add(menu, MF_STRING, CMD_RESET, "Reset position");
            let logging = self.stats.is_logging();
            add(menu, MF_STRING | checked(logging), CMD_LOG, "Write log");
            if logging {
                add(menu, MF_STRING, CMD_OPEN_LOG, "Open log");
            }

            let sub = CreatePopupMenu();
            let current = self.stats.threshold_ms.load(Relaxed);
            for (i, ms) in THRESHOLDS.iter().enumerate() {
                add(sub, MF_STRING | checked(*ms == current), CMD_THRESHOLD + i, &format!("{ms} ms"));
            }
            add(menu, MF_STRING | MF_POPUP, sub as usize, "Alert threshold");
            AppendMenuW(menu, MF_SEPARATOR, 0, null());
            add(menu, MF_STRING, CMD_EXIT, "Exit");
            menu
        }
    }

    fn command(&mut self, cmd: usize) {
        match cmd {
            CMD_SERVER => self.copy_server(),
            CMD_SHOW_SERVER => {
                self.show_server = !self.show_server;
                settings::save_show_server(self.show_server);
                self.update_view();
            }
            CMD_MOVE => self.set_locked(!self.locked),
            CMD_RESET => self.reset_position(),
            CMD_LOG => self.toggle_log(),
            CMD_OPEN_LOG => self.open_log(),
            // Closed from the message loop, outside this borrow of the app (WM_DESTROY needs it).
            CMD_EXIT => unsafe {
                PostMessageW(self.overlay, WM_CLOSE, 0, 0);
            },
            c if (CMD_THRESHOLD..CMD_THRESHOLD + THRESHOLDS.len()).contains(&c) => {
                let ms = THRESHOLDS[c - CMD_THRESHOLD];
                self.stats.threshold_ms.store(ms, Relaxed);
                settings::save_threshold(ms);
            }
            _ => {}
        }
    }
}

/// Shows the tray menu at the cursor and returns the chosen command. The app is not borrowed while
/// the menu is open, so the overlay keeps updating from its timer.
fn show_menu(owner: HWND) -> Option<usize> {
    let menu = with_app(|app| app.build_menu())?;
    unsafe {
        // The owner must be the foreground window, or the menu will not close when clicking elsewhere.
        let mut pt = POINT { x: 0, y: 0 };
        GetCursorPos(&mut pt);
        SetForegroundWindow(owner);
        let cmd = TrackPopupMenu(
            menu,
            TPM_RETURNCMD | TPM_NONOTIFY | TPM_RIGHTBUTTON | TPM_BOTTOMALIGN | TPM_RIGHTALIGN,
            pt.x,
            pt.y,
            0,
            owner,
            null(),
        );
        PostMessageW(owner, WM_NULL, 0, 0);
        DestroyMenu(menu); // also destroys the submenu
        (cmd > 0).then_some(cmd as usize)
    }
}

/// Puts a premultiplied 32-bit bitmap on the layered window (and resizes it to the bitmap).
fn update_layered(hwnd: HWND, hbmp: windows_sys::Win32::Graphics::Gdi::HBITMAP, w: i32, h: i32) {
    unsafe {
        let screen = GetDC(null_mut());
        let mem = CreateCompatibleDC(screen);
        let old = SelectObject(mem, hbmp);
        let size = SIZE { cx: w, cy: h };
        let src = POINT { x: 0, y: 0 };
        let blend = BLENDFUNCTION {
            BlendOp: AC_SRC_OVER as u8,
            BlendFlags: 0,
            SourceConstantAlpha: 255,
            AlphaFormat: AC_SRC_ALPHA as u8,
        };
        UpdateLayeredWindow(hwnd, screen, null(), &size, mem, &src, 0, &blend, ULW_ALPHA);
        SelectObject(mem, old);
        DeleteDC(mem);
        ReleaseDC(null_mut(), screen);
    }
}

fn set_clipboard(owner: HWND, text: &str) -> bool {
    let w = wide(text);
    unsafe {
        if OpenClipboard(owner) == 0 {
            return false;
        }
        EmptyClipboard();
        let mem = GlobalAlloc(GMEM_MOVEABLE, w.len() * 2);
        let mut ok = false;
        if !mem.is_null() {
            let p = GlobalLock(mem) as *mut u16;
            if !p.is_null() {
                std::ptr::copy_nonoverlapping(w.as_ptr(), p, w.len());
                GlobalUnlock(mem);
                ok = !SetClipboardData(CF_UNICODETEXT, mem).is_null();
            }
        }
        CloseClipboard();
        ok
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_TIMER if wparam == TIMER_ID => {
                with_app(|app| app.update_view());
                0
            }
            WM_TRAY => {
                let event = (lparam & 0xFFFF) as u32;
                if (event == WM_RBUTTONUP || event == WM_CONTEXTMENU)
                    && let Some(cmd) = show_menu(hwnd) {
                        with_app(|app| app.command(cmd));
                    }
                0
            }
            WM_LBUTTONDOWN => {
                if with_app(|app| !app.locked && app.overlay == hwnd) == Some(true) {
                    ReleaseCapture();
                    SendMessageW(hwnd, WM_NCLBUTTONDOWN, HTCAPTION as usize, 0);
                }
                0
            }
            WM_DPICHANGED => {
                with_app(|app| {
                    app.model = None; // re-render at the new scale
                    app.update_view();
                });
                0
            }
            WM_MOUSEACTIVATE => MA_NOACTIVATE as LRESULT,
            WM_DESTROY => {
                if with_app(|app| app.overlay == hwnd) == Some(true) {
                    KillTimer(hwnd, TIMER_ID);
                    with_app(|app| app.remove_tray());
                    PostQuitMessage(0);
                }
                0
            }
            // Explorer restarted: the tray icon is gone and has to be added again.
            _ if msg == taskbar_created() => {
                with_app(|app| {
                    app.add_tray();
                    app.model = None;
                    app.update_view();
                });
                0
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}
