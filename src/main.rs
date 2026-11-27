#![windows_subsystem = "windows"]
//! magiCCam — show any Windows video source fullscreen, as a live feed.

mod capture;

use std::cell::RefCell;
use std::time::{Duration, Instant};

use capture::{Device, Graph, Options};
use windows::core::{w, BOOL, PCWSTR};
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_APARTMENTTHREADED};
use windows::Win32::System::Console::{AttachConsole, ATTACH_PARENT_PROCESS};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Power::{SetThreadExecutionState, ES_CONTINUOUS, ES_DISPLAY_REQUIRED, ES_SYSTEM_REQUIRED};
use windows::Win32::UI::HiDpi::{SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2};
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::WindowsAndMessaging::*;

const WM_GRAPH: u32 = WM_APP + 1;
const TIMER_CURSOR: usize = 1;
const TIMER_RECONNECT: usize = 2;
const ID_DEVICE: usize = 1000;
const ID_FULLSCREEN: usize = 1;
const ID_REFRESH: usize = 2;
const ID_MONITOR: usize = 3;
const ID_EXIT: usize = 4;

struct App {
    hwnd: HWND,
    devices: Vec<Device>,
    current: Option<usize>,
    /// Key of the device we want to show (survives unplug / re-enumeration).
    wanted: Option<String>,
    graph: Option<Graph>,
    opts: Options,
    fullscreen: bool,
    windowed_rect: RECT,
    monitor: usize,
    last_mouse: Instant,
    cursor_hidden: bool,
    status: String,
}

thread_local! {
    static APP: RefCell<Option<App>> = const { RefCell::new(None) };
}

/// Runs `f` on the app state; silently skips re-entrant calls (DirectShow can
/// pump messages while a graph is being built).
fn with_app<R>(f: impl FnOnce(&mut App) -> R) -> Option<R> {
    APP.with(|a| a.try_borrow_mut().ok().and_then(|mut a| a.as_mut().map(f)))
}

struct Args {
    list: bool,
    device: Option<String>,
    monitor: Option<usize>,
    windowed: bool,
    opts: Options,
}

const HELP: &str = "magiCCam - fullscreen live view of any camera / capture device

USAGE: magiccam [options]
  -l, --list            list video devices and exit
  -d, --device <x>      device number (1-based) or part of its name
  -m, --monitor <n>     monitor number (1-based), default: the one under the mouse
  -w, --windowed        start in a window instead of fullscreen
  -k, --keep-format     do not change the device's output format
      --max-height <n>  highest auto-selected resolution (default 1080, 0 = no limit)

KEYS: Esc/Q quit | F/F11/Enter/double-click fullscreen | Tab/arrows/1-9 switch camera
      M next monitor | R rescan devices | right-click menu";

fn parse_args() -> Result<Args, String> {
    let mut a = Args {
        list: false,
        device: None,
        monitor: None,
        windowed: false,
        opts: Options { keep_format: false, max_height: 1080 },
    };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "-l" | "--list" => a.list = true,
            "-w" | "--windowed" => a.windowed = true,
            "-k" | "--keep-format" => a.opts.keep_format = true,
            "-d" | "--device" => a.device = Some(it.next().ok_or("--device needs a value")?),
            "-m" | "--monitor" => {
                let n: usize = it.next().and_then(|v| v.parse().ok()).ok_or("--monitor needs a number")?;
                a.monitor = Some(n.saturating_sub(1));
            }
            "--max-height" => {
                a.opts.max_height = it.next().and_then(|v| v.parse().ok()).ok_or("--max-height needs a number")?
            }
            "-h" | "--help" | "/?" => return Err(String::new()),
            other => return Err(format!("unknown option: {other}")),
        }
    }
    Ok(a)
}

fn console_print(s: &str) {
    unsafe {
        let _ = AttachConsole(ATTACH_PARENT_PROCESS);
    }
    println!("{s}");
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

fn state_file() -> Option<std::path::PathBuf> {
    let dir = std::path::PathBuf::from(std::env::var_os("APPDATA")?).join("magiCCam");
    let _ = std::fs::create_dir_all(&dir);
    Some(dir.join("last_device.txt"))
}

fn main() {
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            console_print(&if e.is_empty() { HELP.to_string() } else { format!("{e}\n\n{HELP}") });
            return;
        }
    };

    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
    }

    let devices = capture::enumerate();
    if args.list {
        let mut s = String::new();
        if devices.is_empty() {
            s.push_str("No video devices found.");
        }
        for (i, d) in devices.iter().enumerate() {
            s.push_str(&format!("{:>2}: {}\n", i + 1, d.name));
        }
        console_print(s.trim_end());
        unsafe { CoUninitialize() };
        return;
    }

    // Which device to start with: --device, else the last one used, else the first.
    let wanted = match &args.device {
        Some(sel) => {
            let found = match sel.parse::<usize>() {
                Ok(n) if n >= 1 && n <= devices.len() => Some(n - 1),
                _ => {
                    let l = sel.to_lowercase();
                    devices.iter().position(|d| d.name.to_lowercase().contains(&l))
                }
            };
            match found {
                Some(i) => Some(devices[i].key()),
                None => Some(sel.clone()), // not present yet: wait for it to appear
            }
        }
        None => state_file()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .map(|s| s.trim().to_string())
            .filter(|s| devices.iter().any(|d| d.key() == *s))
            .or_else(|| devices.first().map(|d| d.key())),
    };

    unsafe {
        // Live view: keep the display awake, no screensaver.
        SetThreadExecutionState(ES_CONTINUOUS | ES_DISPLAY_REQUIRED | ES_SYSTEM_REQUIRED);

        let hinst = GetModuleHandleW(None).unwrap_or_default();
        let class = w!("magiCCamWindow");
        let wc = WNDCLASSEXW {
            cbSize: size_of::<WNDCLASSEXW>() as u32,
            style: CS_DBLCLKS | CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(wndproc),
            hInstance: hinst.into(),
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            hIcon: LoadIconW(None, IDI_APPLICATION).unwrap_or_default(),
            hbrBackground: HBRUSH(GetStockObject(BLACK_BRUSH).0),
            lpszClassName: class,
            ..Default::default()
        };
        RegisterClassExW(&wc);

        let monitors = monitor_rects();
        let monitor = args.monitor.filter(|m| *m < monitors.len()).unwrap_or_else(|| {
            let mut pt = POINT::default();
            let _ = GetCursorPos(&mut pt);
            monitors.iter().position(|r| PtInRect(r, pt).as_bool()).unwrap_or(0)
        });
        let mr = monitors.get(monitor).copied().unwrap_or(RECT { left: 0, top: 0, right: 1280, bottom: 720 });
        let (mw, mh) = (mr.right - mr.left, mr.bottom - mr.top);
        let (ww, wh) = ((mw * 2 / 3).max(640), (mh * 2 / 3).max(360));
        let windowed_rect = RECT {
            left: mr.left + (mw - ww) / 2,
            top: mr.top + (mh - wh) / 2,
            right: mr.left + (mw + ww) / 2,
            bottom: mr.top + (mh + wh) / 2,
        };

        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            class,
            w!("magiCCam"),
            WS_OVERLAPPEDWINDOW | WS_CLIPCHILDREN,
            windowed_rect.left,
            windowed_rect.top,
            ww,
            wh,
            None,
            None,
            Some(hinst.into()),
            None,
        )
        .expect("CreateWindowExW failed");

        APP.with(|a| {
            *a.borrow_mut() = Some(App {
                hwnd,
                devices,
                current: None,
                wanted,
                graph: None,
                opts: args.opts,
                fullscreen: false,
                windowed_rect,
                monitor,
                last_mouse: Instant::now(),
                cursor_hidden: false,
                status: String::new(),
            })
        });

        if !args.windowed {
            with_app(|app| app.set_fullscreen(true));
        }
        let _ = ShowWindow(hwnd, SW_SHOW);
        let _ = SetForegroundWindow(hwnd);
        SetTimer(Some(hwnd), TIMER_CURSOR, 500, None);
        with_app(|app| app.connect());

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }

        APP.with(|a| a.borrow_mut().take()); // releases the graph before COM shuts down
        CoUninitialize();
    }
}

fn monitor_rects() -> Vec<RECT> {
    unsafe extern "system" fn cb(hm: HMONITOR, _: HDC, _: *mut RECT, data: LPARAM) -> BOOL {
        let v = &mut *(data.0 as *mut Vec<RECT>);
        let mut mi = MONITORINFO { cbSize: size_of::<MONITORINFO>() as u32, ..Default::default() };
        if GetMonitorInfoW(hm, &mut mi).as_bool() {
            // primary monitor first so "--monitor 1" is the main screen
            if mi.dwFlags & MONITORINFOF_PRIMARY != 0 {
                v.insert(0, mi.rcMonitor);
            } else {
                v.push(mi.rcMonitor);
            }
        }
        TRUE
    }
    let mut v: Vec<RECT> = Vec::new();
    unsafe {
        let _ = EnumDisplayMonitors(None, None, Some(cb), LPARAM(&mut v as *mut _ as isize));
    }
    v
}

impl App {
    fn client_rect(&self) -> RECT {
        let mut rc = RECT::default();
        unsafe {
            let _ = GetClientRect(self.hwnd, &mut rc);
        }
        rc
    }

    fn invalidate(&self) {
        unsafe {
            let _ = InvalidateRect(Some(self.hwnd), None, true);
        }
    }

    fn update_title(&self) {
        let title = match (self.current, &self.graph) {
            (Some(i), Some(g)) => format!("magiCCam - {} [{}]", self.devices[i].name, g.format),
            _ => format!("magiCCam - {}", self.status),
        };
        unsafe {
            let _ = SetWindowTextW(self.hwnd, PCWSTR(wide(&title).as_ptr()));
        }
    }

    /// (Re)opens the wanted device. Returns false if it is not available yet.
    fn connect(&mut self) -> bool {
        self.graph = None; // must release the device before reopening it
        self.current = None;
        let Some(key) = self.wanted.clone() else {
            self.status = "No video device found - plug one in or press R".into();
            self.update_title();
            self.invalidate();
            return false;
        };
        let lkey = key.to_lowercase();
        let idx = self
            .devices
            .iter()
            .position(|d| d.key() == key)
            .or_else(|| self.devices.iter().position(|d| d.name.to_lowercase().contains(&lkey)));
        let Some(idx) = idx else {
            self.status = format!("Waiting for \"{key}\"...");
            self.schedule_reconnect();
            return false;
        };

        self.status = format!("Opening {}...", self.devices[idx].name);
        self.invalidate();
        unsafe {
            let _ = UpdateWindow(self.hwnd);
        }
        match Graph::open(&self.devices[idx], self.hwnd, WM_GRAPH, self.opts) {
            Ok(g) => {
                g.resize(self.client_rect());
                self.graph = Some(g);
                self.current = Some(idx);
                self.status.clear();
                if let Some(p) = state_file() {
                    let _ = std::fs::write(p, self.devices[idx].key());
                }
                unsafe {
                    let _ = KillTimer(Some(self.hwnd), TIMER_RECONNECT);
                }
                self.update_title();
                self.invalidate();
                true
            }
            Err(e) => {
                self.status = format!("{}: cannot open ({}) - retrying", self.devices[idx].name, e.message());
                self.schedule_reconnect();
                false
            }
        }
    }

    fn schedule_reconnect(&mut self) {
        self.update_title();
        self.invalidate();
        unsafe {
            SetTimer(Some(self.hwnd), TIMER_RECONNECT, 2000, None);
        }
    }

    fn select(&mut self, idx: usize) {
        if idx < self.devices.len() {
            self.wanted = Some(self.devices[idx].key());
            self.connect();
        }
    }

    fn step(&mut self, delta: isize) {
        let n = self.devices.len() as isize;
        if n == 0 {
            return;
        }
        let cur = self.current.map(|c| c as isize).unwrap_or(-1);
        self.select((cur + delta).rem_euclid(n) as usize);
    }

    fn rescan(&mut self) {
        self.graph = None;
        self.devices = capture::enumerate();
        if self.wanted.is_none() {
            self.wanted = self.devices.first().map(|d| d.key());
        }
        self.connect();
    }

    fn set_fullscreen(&mut self, on: bool) {
        unsafe {
            if on {
                if !self.fullscreen {
                    let _ = GetWindowRect(self.hwnd, &mut self.windowed_rect);
                }
                let mons = monitor_rects();
                let r = mons.get(self.monitor).or(mons.first()).copied().unwrap_or_default();
                SetWindowLongPtrW(self.hwnd, GWL_STYLE, (WS_POPUP | WS_VISIBLE | WS_CLIPCHILDREN).0 as isize);
                let _ = SetWindowPos(
                    self.hwnd,
                    Some(HWND_TOP),
                    r.left,
                    r.top,
                    r.right - r.left,
                    r.bottom - r.top,
                    SWP_FRAMECHANGED | SWP_SHOWWINDOW,
                );
            } else {
                SetWindowLongPtrW(self.hwnd, GWL_STYLE, (WS_OVERLAPPEDWINDOW | WS_VISIBLE | WS_CLIPCHILDREN).0 as isize);
                let r = self.windowed_rect;
                let _ = SetWindowPos(
                    self.hwnd,
                    Some(HWND_NOTOPMOST),
                    r.left,
                    r.top,
                    r.right - r.left,
                    r.bottom - r.top,
                    SWP_FRAMECHANGED | SWP_SHOWWINDOW,
                );
                self.show_cursor();
            }
        }
        self.fullscreen = on;
        self.relayout();
    }

    /// WM_SIZE is swallowed while we hold the app state, so re-fit explicitly.
    fn relayout(&self) {
        if let Some(g) = &self.graph {
            g.resize(self.client_rect());
        }
        self.invalidate();
    }

    fn next_monitor(&mut self) {
        let n = monitor_rects().len().max(1);
        self.monitor = (self.monitor + 1) % n;
        if self.fullscreen {
            self.set_fullscreen(true);
        } else {
            let mons = monitor_rects();
            if let Some(m) = mons.get(self.monitor) {
                let (w, h) = (self.windowed_rect.right - self.windowed_rect.left, self.windowed_rect.bottom - self.windowed_rect.top);
                let (x, y) = (m.left + (m.right - m.left - w) / 2, m.top + (m.bottom - m.top - h) / 2);
                unsafe {
                    let _ = SetWindowPos(self.hwnd, None, x, y, w, h, SWP_NOZORDER);
                }
                self.relayout();
            }
        }
    }

    fn show_cursor(&mut self) {
        self.last_mouse = Instant::now();
        if self.cursor_hidden {
            self.cursor_hidden = false;
            unsafe {
                SetCursor(LoadCursorW(None, IDC_ARROW).ok());
            }
        }
    }

    fn show_menu(&mut self, x: i32, y: i32) {
        self.show_cursor();
        unsafe {
            let Ok(menu) = CreatePopupMenu() else { return };
            if self.devices.is_empty() {
                let _ = AppendMenuW(menu, MF_STRING | MF_GRAYED, 0, w!("(no video devices)"));
            }
            for (i, d) in self.devices.iter().enumerate() {
                let label = if i < 9 { format!("&{}  {}", i + 1, d.name) } else { d.name.clone() };
                let flags = if self.current == Some(i) { MF_STRING | MF_CHECKED } else { MF_STRING };
                let _ = AppendMenuW(menu, flags, ID_DEVICE + i, PCWSTR(wide(&label).as_ptr()));
            }
            let _ = AppendMenuW(menu, MF_SEPARATOR, 0, None);
            let fs = if self.fullscreen { MF_STRING | MF_CHECKED } else { MF_STRING };
            let _ = AppendMenuW(menu, fs, ID_FULLSCREEN, w!("&Fullscreen\tF"));
            let _ = AppendMenuW(menu, MF_STRING, ID_MONITOR, w!("Next &monitor\tM"));
            let _ = AppendMenuW(menu, MF_STRING, ID_REFRESH, w!("&Rescan devices\tR"));
            if let Some(g) = &self.graph {
                let info = format!("{} / {}", g.format, if g.is_evr() { "EVR" } else { "legacy renderer" });
                let _ = AppendMenuW(menu, MF_STRING | MF_GRAYED, 0, PCWSTR(wide(&info).as_ptr()));
            }
            let _ = AppendMenuW(menu, MF_SEPARATOR, 0, None);
            let _ = AppendMenuW(menu, MF_STRING, ID_EXIT, w!("E&xit\tEsc"));
            let _ = SetForegroundWindow(self.hwnd);
            let cmd = TrackPopupMenu(menu, TPM_RIGHTBUTTON | TPM_RETURNCMD, x, y, None, self.hwnd, None);
            let _ = DestroyMenu(menu);
            if cmd.0 > 0 {
                let _ = PostMessageW(Some(self.hwnd), WM_COMMAND, WPARAM(cmd.0 as usize), LPARAM(0));
            }
        }
    }

    fn paint(&self, hdc: HDC) {
        if let Some(g) = &self.graph {
            g.repaint();
            return;
        }
        unsafe {
            let mut rc = self.client_rect();
            FillRect(hdc, &rc, HBRUSH(GetStockObject(BLACK_BRUSH).0));
            SetBkMode(hdc, TRANSPARENT);
            SetTextColor(hdc, COLORREF(0x00A0A0A0));
            let mut text: Vec<u16> = format!("{}\n\nright-click for menu  -  Esc to quit", self.status).encode_utf16().collect();
            let mut calc = rc;
            DrawTextW(hdc, &mut text, &mut calc, DT_CENTER | DT_WORDBREAK | DT_CALCRECT);
            let h = calc.bottom - calc.top;
            rc.top = (rc.bottom - h) / 2;
            DrawTextW(hdc, &mut text, &mut rc, DT_CENTER | DT_WORDBREAK);
        }
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_SIZE => {
            with_app(|app| {
                if let Some(g) = &app.graph {
                    g.resize(app.client_rect());
                }
                app.invalidate();
            });
            LRESULT(0)
        }
        WM_PAINT => {
            let mut ps = PAINTSTRUCT::default();
            let hdc = BeginPaint(hwnd, &mut ps);
            if with_app(|app| app.paint(hdc)).is_none() {
                FillRect(hdc, &ps.rcPaint, HBRUSH(GetStockObject(BLACK_BRUSH).0));
            }
            let _ = EndPaint(hwnd, &ps);
            LRESULT(0)
        }
        WM_ERASEBKGND => {
            // EVR paints the whole client area (incl. black bars) itself.
            if with_app(|app| app.graph.as_ref().is_some_and(|g| g.is_evr())).unwrap_or(false) {
                LRESULT(1)
            } else {
                DefWindowProcW(hwnd, msg, wp, lp)
            }
        }
        WM_GRAPH => {
            with_app(|app| {
                if app.graph.as_ref().is_some_and(capture::device_failed) {
                    app.graph = None;
                    app.current = None;
                    app.status = "No signal (source stopped or unplugged) - waiting for it...".into();
                    app.schedule_reconnect();
                }
            });
            LRESULT(0)
        }
        WM_TIMER => {
            match wp.0 {
                TIMER_CURSOR => {
                    with_app(|app| {
                        if app.fullscreen && !app.cursor_hidden && app.last_mouse.elapsed() > Duration::from_secs(2) {
                            app.cursor_hidden = true;
                            SetCursor(None);
                        }
                    });
                }
                TIMER_RECONNECT => {
                    with_app(|app| {
                        app.devices = capture::enumerate();
                        app.connect();
                    });
                }
                _ => {}
            }
            LRESULT(0)
        }
        WM_SETCURSOR if (lp.0 & 0xFFFF) as u32 == HTCLIENT => {
            if with_app(|app| app.cursor_hidden).unwrap_or(false) {
                SetCursor(None);
                LRESULT(1)
            } else {
                DefWindowProcW(hwnd, msg, wp, lp)
            }
        }
        WM_MOUSEMOVE => {
            with_app(|app| app.show_cursor());
            LRESULT(0)
        }
        WM_LBUTTONDBLCLK => {
            with_app(|app| app.set_fullscreen(!app.fullscreen));
            LRESULT(0)
        }
        WM_CONTEXTMENU => {
            let (mut x, mut y) = ((lp.0 & 0xFFFF) as i16 as i32, ((lp.0 >> 16) & 0xFFFF) as i16 as i32);
            if x == -1 && y == -1 {
                let mut pt = POINT::default();
                let _ = GetCursorPos(&mut pt);
                (x, y) = (pt.x, pt.y);
            }
            with_app(|app| app.show_menu(x, y));
            LRESULT(0)
        }
        WM_COMMAND => {
            let id = wp.0 & 0xFFFF;
            match id {
                ID_FULLSCREEN => {
                    with_app(|app| app.set_fullscreen(!app.fullscreen));
                }
                ID_REFRESH => {
                    with_app(|app| app.rescan());
                }
                ID_MONITOR => {
                    with_app(|app| app.next_monitor());
                }
                ID_EXIT => {
                    let _ = DestroyWindow(hwnd);
                }
                id if id >= ID_DEVICE => {
                    with_app(|app| app.select(id - ID_DEVICE));
                }
                _ => {}
            }
            LRESULT(0)
        }
        WM_KEYDOWN | WM_SYSKEYDOWN => {
            let vk = VIRTUAL_KEY(wp.0 as u16);
            let shift = GetKeyState(VK_SHIFT.0 as i32) < 0;
            match vk {
                VK_ESCAPE | VK_Q => {
                    let _ = DestroyWindow(hwnd);
                }
                VK_F | VK_F11 | VK_RETURN => {
                    with_app(|app| app.set_fullscreen(!app.fullscreen));
                }
                VK_TAB => {
                    with_app(|app| app.step(if shift { -1 } else { 1 }));
                }
                VK_RIGHT | VK_DOWN | VK_NEXT | VK_SPACE => {
                    with_app(|app| app.step(1));
                }
                VK_LEFT | VK_UP | VK_PRIOR => {
                    with_app(|app| app.step(-1));
                }
                VK_M => {
                    with_app(|app| app.next_monitor());
                }
                VK_R => {
                    with_app(|app| app.rescan());
                }
                _ if (0x31..=0x39).contains(&vk.0) => {
                    with_app(|app| app.select((vk.0 - 0x31) as usize));
                }
                _ if (VK_NUMPAD1.0..=VK_NUMPAD9.0).contains(&vk.0) => {
                    with_app(|app| app.select((vk.0 - VK_NUMPAD1.0) as usize));
                }
                _ => return DefWindowProcW(hwnd, msg, wp, lp),
            }
            LRESULT(0)
        }
        WM_DISPLAYCHANGE => {
            with_app(|app| {
                if app.fullscreen {
                    app.set_fullscreen(true);
                }
            });
            LRESULT(0)
        }
        WM_DESTROY => {
            with_app(|app| app.graph = None);
            PostQuitMessage(0);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}
