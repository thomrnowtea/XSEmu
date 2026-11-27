//! Hosted mode: `XSEmu run --host-hwnd <hex>` (docs/launcher-protocol.md). The game window is a
//! child of a launcher's native widget instead of a top-level window. It covers the largest
//! centred 16:9 rectangle of the widget's client area (the swap chain stretches the frame over
//! the whole window, so a 16:9 window is the letterbox; the bars are the widget's own
//! background) and follows the widget's size, which a child window does not learn on its own,
//! by polling it. The launcher's shortcuts (F1, F3, F11, Escape) pressed in it go to the
//! launcher (`control::key`), not to the title. Without the option nothing here runs and the
//! window is the top-level one.

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

/// The host window, handed from `XSEmu run` to the host process it starts.
pub const HOST_HWND_ENV: &str = "XSEMU_HOST_HWND";

/// `run`'s command line after `run`: the launcher options, then the game and its arguments.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct RunArgs {
    pub host_hwnd: Option<isize>,
    pub control_pipe: Option<String>,
    pub target: String,
    pub game_args: Vec<String>,
}

/// The launcher options sit between `run` and the game folder; what follows the game is passed
/// to it as before (everything after `--` when there is one, else everything).
pub fn parse_run_args(args: &[String]) -> Result<RunArgs, String> {
    let mut run = RunArgs::default();
    let mut i = 0;
    while let Some(option) = args.get(i) {
        match option.as_str() {
            "--host-hwnd" => {
                let v = args.get(i + 1).ok_or("--host-hwnd needs a window handle (hex)")?;
                run.host_hwnd = Some(parse_hwnd(v).ok_or_else(|| format!("--host-hwnd: {v:?} is not a window handle (hex, with or without 0x)"))?);
                i += 2;
            }
            "--control-pipe" => {
                let v = args.get(i + 1).filter(|v| !v.trim().is_empty()).ok_or("--control-pipe needs a pipe name")?;
                run.control_pipe = Some(v.clone());
                i += 2;
            }
            _ => break,
        }
    }
    let target = args.get(i).ok_or("no game folder or exe given")?;
    let tail = &args[i + 1..];
    run.target = target.clone();
    run.game_args = match tail.iter().position(|a| a == "--") {
        Some(k) => tail[k + 1..].to_vec(),
        None => tail.to_vec(),
    };
    Ok(run)
}

impl RunArgs {
    /// The options for the host process `launcher::run` starts (it inherits the environment);
    /// an option not given is removed, so a stray variable never turns hosting on.
    pub fn forward(&self) {
        match self.host_hwnd {
            Some(h) => std::env::set_var(HOST_HWND_ENV, format!("{h:x}")),
            None => std::env::remove_var(HOST_HWND_ENV),
        }
        match &self.control_pipe {
            Some(name) => std::env::set_var(crate::control::CONTROL_PIPE_ENV, name),
            None => std::env::remove_var(crate::control::CONTROL_PIPE_ENV),
        }
    }
}

/// A window handle in hex, with or without `0x`.
pub fn parse_hwnd(s: &str) -> Option<isize> {
    let s = s.trim();
    let hex = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")).unwrap_or(s);
    u64::from_str_radix(hex, 16).ok().filter(|&h| h != 0).map(|h| h as isize)
}

/// The launcher's window the game window is a child of (None: a top-level window as always).
pub fn host_hwnd() -> Option<isize> {
    static HOST: OnceLock<Option<isize>> = OnceLock::new();
    *HOST.get_or_init(|| std::env::var(HOST_HWND_ENV).ok().and_then(|v| parse_hwnd(&v)))
}

/// The largest 16:9 rectangle centred in a `w`×`h` client area: [x, y, width, height].
pub fn fit_16_9(w: i32, h: i32) -> [i32; 4] {
    let (w, h) = (w.max(0) as i64, h.max(0) as i64);
    let (fw, fh) = if w * 9 > h * 16 { (h * 16 / 9, h) } else { (w, w * 9 / 16) };
    [((w - fw) / 2) as i32, ((h - fh) / 2) as i32, fw as i32, fh as i32]
}

#[link(name = "user32")]
extern "system" {
    fn SetWindowPos(hwnd: *mut c_void, after: *mut c_void, x: i32, y: i32, cx: i32, cy: i32, flags: u32) -> i32;
    fn IsWindow(hwnd: *mut c_void) -> i32;
    fn SetFocus(hwnd: *mut c_void) -> *mut c_void;
    fn SetTimer(hwnd: *mut c_void, id: usize, ms: u32, f: *const c_void) -> usize;
    fn SetProcessDpiAwarenessContext(context: isize) -> i32;
    fn PostMessageW(hwnd: *mut c_void, msg: u32, wp: usize, lp: isize) -> i32;
}

const WM_DESTROY: u32 = 0x0002;
const WM_TIMER: u32 = 0x0113;
const WM_LBUTTONDOWN: u32 = 0x0201;
const WM_KEYDOWN: u32 = 0x0100;
const WM_KEYUP: u32 = 0x0101;
const WM_SYSKEYDOWN: u32 = 0x0104;
const WM_SYSKEYUP: u32 = 0x0105;
/// WM_APP + 0x5C1: take the keyboard focus (`focus`, from the control pipe's thread)
const WM_TAKE_FOCUS: u32 = 0x8000 + 0x5C1;
const SWP_NOZORDER: u32 = 0x0004;
const SWP_NOACTIVATE: u32 = 0x0010;
const SWP_ASYNCWINDOWPOS: u32 = 0x4000;
/// DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2
const PER_MONITOR_AWARE_V2: isize = -4;
/// the game window's timer that follows the host's size (`follow`)
const FOLLOW_TIMER: usize = 0x5C1;
/// how often the host's size is looked at (ms)
const FOLLOW_MS: u32 = 33;

/// where the game window was last put in the host's client area
static PLACED: Mutex<[i32; 4]> = Mutex::new([0; 4]);
static STOPPING: AtomicBool = AtomicBool::new(false);

/// The host process starts hosted: per-monitor DPI aware (v2) like the launcher, before any
/// window exists (Windows scales or refuses a child window of another process whose DPI
/// awareness differs), and the game window made at once, so the launcher sees it while the
/// title boots. Set here, not in a manifest: a top-level run keeps the awareness it has today.
pub fn start() {
    let Some(parent) = host_hwnd() else { return };
    let dpi = unsafe { SetProcessDpiAwarenessContext(PER_MONITOR_AWARE_V2) } != 0;
    crate::xlog_info!(
        "hosted in the launcher's window {parent:#x}: the game window is its child{}",
        if dpi { ", per-monitor DPI aware (v2)" } else { " (per-monitor DPI awareness could not be set)" }
    );
    if crate::hle::d3d12x::present::window().is_none() {
        let msg = format!("could not make the game window in the launcher's window {parent:#x}");
        crate::xlog_error!("{msg}");
        crate::control::exit(0x7C1_0003, "error", &msg);
        crate::report::write_final(Some(msg));
        crate::log::flush();
        std::process::exit(0x7C1_0003);
    }
}

/// The launcher's shortcut a virtual key is (`key` in docs/launcher-protocol.md).
pub fn launcher_key(vk: usize) -> Option<&'static str> {
    match vk {
        0x70 => Some("F1"),
        0x72 => Some("F3"),
        0x7A => Some("F11"),
        0x1B => Some("Escape"),
        _ => None,
    }
}

/// The launcher asked for the keyboard focus: the window thread takes it.
pub fn focus() {
    if host_hwnd().is_none() {
        return;
    }
    if let Some(hwnd) = crate::hle::d3d12x::present::window() {
        unsafe { PostMessageW(hwnd as *mut c_void, WM_TAKE_FOCUS, 0, 0) };
    }
}

/// Where the game window goes in `parent`'s client area now.
pub fn child_rect(parent: isize) -> [i32; 4] {
    use windows::Win32::{Foundation::{HWND, RECT}, UI::WindowsAndMessaging::GetClientRect};
    let mut r = RECT::default();
    let _ = unsafe { GetClientRect(HWND(parent as *mut c_void), &mut r) };
    let rect = fit_16_9(r.right - r.left, r.bottom - r.top);
    *PLACED.lock().unwrap_or_else(|e| e.into_inner()) = rect;
    rect
}

/// The game window was made as a top-level window: in hosted mode it becomes a child of the host,
/// covering `child_rect` over a black class background, and follows the host's size from then on.
pub fn adopt(hwnd: *mut c_void) {
    let Some(parent) = host_hwnd() else { return };
    use windows::Win32::Foundation::HWND;
    use windows::Win32::Graphics::Gdi::{GetStockObject, BLACK_BRUSH};
    use windows::Win32::UI::WindowsAndMessaging::{
        SetClassLongPtrW, SetParent, SetWindowLongPtrW, SetWindowPos, GCLP_HBRBACKGROUND, GWL_STYLE, SWP_FRAMECHANGED, SWP_NOZORDER, SWP_SHOWWINDOW,
        WS_CHILD, WS_CLIPCHILDREN, WS_CLIPSIBLINGS, WS_VISIBLE,
    };
    let [x, y, w, h] = child_rect(parent);
    let me = HWND(hwnd);
    unsafe {
        SetWindowLongPtrW(me, GWL_STYLE, (WS_CHILD | WS_CLIPSIBLINGS | WS_CLIPCHILDREN | WS_VISIBLE).0 as isize);
        let _ = SetParent(me, Some(HWND(parent as *mut c_void)));
        SetClassLongPtrW(me, GCLP_HBRBACKGROUND, GetStockObject(BLACK_BRUSH).0 as isize);
        let _ = SetWindowPos(me, None, x, y, w, h, SWP_NOZORDER | SWP_FRAMECHANGED | SWP_SHOWWINDOW);
    }
    crate::xlog_info!("XCL window moved into the launcher's window ({w}x{h} at {x},{y})");
    attach(hwnd);
}

/// The game window is up (a child of the host): it follows the host's size from now on.
pub fn attach(hwnd: *mut c_void) {
    unsafe { SetTimer(hwnd, FOLLOW_TIMER, FOLLOW_MS, std::ptr::null()) };
}

/// The game window's messages in hosted mode, before anything else sees them: Some(result)
/// when handled here.
pub unsafe fn on_message(hwnd: *mut c_void, msg: u32, wp: usize, lp: isize) -> Option<isize> {
    let parent = host_hwnd()?;
    match msg {
        WM_TIMER if wp == FOLLOW_TIMER => {
            follow(hwnd, parent);
            Some(0)
        }
        WM_LBUTTONDOWN => {
            // a child window is not given the keyboard focus by a click
            SetFocus(hwnd);
            None
        }
        WM_TAKE_FOCUS => {
            SetFocus(hwnd);
            Some(0)
        }
        WM_KEYDOWN | WM_SYSKEYDOWN | WM_KEYUP | WM_SYSKEYUP => {
            let key = launcher_key(wp)?;
            // (bit 30: the key was down already, a repeat)
            if matches!(msg, WM_KEYDOWN | WM_SYSKEYDOWN) && lp & (1 << 30) == 0 {
                crate::control::key(key);
            }
            Some(0)
        }
        WM_DESTROY => {
            stop_soon("the launcher's window closed");
            None
        }
        _ => None,
    }
}

/// The host's client area changed: the game window moves to its new 16:9 rectangle. The host
/// gone (the launcher ended): the title ends.
unsafe fn follow(hwnd: *mut c_void, parent: isize) {
    if IsWindow(parent as *mut c_void) == 0 {
        stop_soon("the launcher's window is gone");
        return;
    }
    let before = *PLACED.lock().unwrap_or_else(|e| e.into_inner());
    let [x, y, w, h] = child_rect(parent);
    if [x, y, w, h] != before {
        SetWindowPos(hwnd, std::ptr::null_mut(), x, y, w, h, SWP_ASYNCWINDOWPOS | SWP_NOACTIVATE | SWP_NOZORDER);
    }
}

/// End the title on purpose, once (a later call returns at once): the frame dumps, the report
/// and the log written, the GPU drained (`real::end_title`).
pub fn stop_title(why: &str) {
    if STOPPING.swap(true, Ordering::AcqRel) {
        return;
    }
    crate::xlog_info!("{why} – ending the title");
    crate::control::exit(0, "stopped", "");
    crate::hle::d3d12x::present::flush_frame_dumps();
    crate::report::write_final(None);
    crate::log::flush();
    crate::hle::d3d12x::real::end_title(0);
}

/// `stop_title` on a thread of its own: the window thread goes on answering meanwhile.
fn stop_soon(why: &'static str) {
    if !STOPPING.load(Ordering::Acquire) {
        let _ = std::thread::Builder::new().name("xsemu-stop".into()).spawn(move || stop_title(why));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn run_without_launcher_options_is_unchanged() {
        let run = parse_run_args(&args(&["D:\\Game", "a", "b"])).unwrap();
        assert_eq!(run, RunArgs { host_hwnd: None, control_pipe: None, target: "D:\\Game".into(), game_args: args(&["a", "b"]) });
        let run = parse_run_args(&args(&["D:\\Game", "x", "--", "a"])).unwrap();
        assert_eq!(run.game_args, args(&["a"]));
    }

    #[test]
    fn launcher_options_come_before_the_game() {
        let run = parse_run_args(&args(&["--host-hwnd", "0x1A2B3C", "--control-pipe", "xsemu-42-1", "D:\\Game", "--", "--hello"])).unwrap();
        assert_eq!(run.host_hwnd, Some(0x1A2B3C));
        assert_eq!(run.control_pipe.as_deref(), Some("xsemu-42-1"));
        assert_eq!(run.target, "D:\\Game");
        assert_eq!(run.game_args, args(&["--hello"]));
        let run = parse_run_args(&args(&["--control-pipe", "p", "--host-hwnd", "10", "D:\\Game"])).unwrap();
        assert_eq!((run.host_hwnd, run.control_pipe.as_deref(), run.target.as_str()), (Some(0x10), Some("p"), "D:\\Game"));
        // after the game an option is the game's
        let run = parse_run_args(&args(&["D:\\Game", "--host-hwnd", "10"])).unwrap();
        assert_eq!(run.host_hwnd, None);
        assert_eq!(run.game_args, args(&["--host-hwnd", "10"]));
    }

    #[test]
    fn bad_launcher_options_are_refused() {
        assert!(parse_run_args(&args(&["--host-hwnd"])).is_err());
        assert!(parse_run_args(&args(&["--host-hwnd", "zz", "D:\\Game"])).is_err());
        assert!(parse_run_args(&args(&["--host-hwnd", "0", "D:\\Game"])).is_err());
        assert!(parse_run_args(&args(&["--host-hwnd", "1a2b"])).is_err());
        assert!(parse_run_args(&args(&["--control-pipe"])).is_err());
        assert!(parse_run_args(&args(&["--control-pipe", "", "D:\\Game"])).is_err());
    }

    #[test]
    fn window_handles_are_hex_with_or_without_0x() {
        assert_eq!(parse_hwnd("1a2b3c"), Some(0x1a2b3c));
        assert_eq!(parse_hwnd("0x1A2B3C"), Some(0x1a2b3c));
        assert_eq!(parse_hwnd(" 0X10 "), Some(0x10));
        assert_eq!(parse_hwnd(""), None);
        assert_eq!(parse_hwnd("0x"), None);
        assert_eq!(parse_hwnd("12g"), None);
    }

    #[test]
    fn launcher_shortcuts_are_f1_f3_f11_and_escape() {
        assert_eq!([0x70, 0x72, 0x7A, 0x1B].map(launcher_key), [Some("F1"), Some("F3"), Some("F11"), Some("Escape")]);
        assert_eq!(launcher_key(0x71), None);
        assert_eq!(launcher_key(0x0D), None);
    }

    #[test]
    fn the_game_window_is_the_largest_centred_16_9_rectangle() {
        assert_eq!(fit_16_9(1920, 1080), [0, 0, 1920, 1080]);
        // wider: bars left and right
        assert_eq!(fit_16_9(2560, 1080), [320, 0, 1920, 1080]);
        // taller (16:10): bars above and below
        assert_eq!(fit_16_9(1920, 1200), [0, 60, 1920, 1080]);
        assert_eq!(fit_16_9(1281, 720), [0, 0, 1280, 720]);
        assert_eq!(fit_16_9(0, 720), [0, 360, 0, 0]);
        assert_eq!(fit_16_9(-5, -5), [0, 0, 0, 0]);
    }
}
