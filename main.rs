#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
//! # Auto Clicker
//!
//! An open-source take on OP Auto Clicker, sharing its layout and its defaults but
//! adding the parts that were always missing: themes, six languages, a tray icon,
//! rebindable hotkeys, a hold-to-click mode, multi-point clicking, a keystroke mode
//! and a pixel stop condition.
//!
//! Threads:
//!   * UI       - eframe/egui
//!   * hotkeys  - RegisterHotKey + tray window + Win32 message loop
//!   * clicker  - spin_sleep + timeBeginPeriod(1) + SendInput
//!
//! Deliberately hook-free: everything is driven by `RegisterHotKey` and
//! `GetAsyncKeyState`, so the app never installs a global keyboard hook.
//!
//! `panic = "abort"` in release, so the shared paths avoid unwrap/indexing entirely.

use anyhow::{Context as _, Result};
use eframe::egui;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};
use tracing::{info, warn};

#[cfg(windows)]
mod win32 {
    pub use windows::Win32::Foundation::*;
    pub use windows::Win32::Globalization::GetUserDefaultUILanguage;
    pub use windows::Win32::Graphics::Dwm::*;
    pub use windows::Win32::Graphics::Gdi::{GetDC, GetPixel, HRGN, ReleaseDC};
    pub use windows::Win32::Media::*;
    pub use windows::Win32::System::LibraryLoader::*;
    pub use windows::Win32::System::Registry::*;
    pub use windows::Win32::System::Threading::*;
    pub use windows::Win32::UI::HiDpi::*;
    pub use windows::Win32::UI::Input::KeyboardAndMouse::*;
    pub use windows::Win32::UI::Shell::*;
    pub use windows::Win32::UI::WindowsAndMessaging::*;
    pub use windows::core::{PCSTR, PCWSTR, w};
}

// ============================================================================
// Constants
// ============================================================================

const APP_TITLE: &str = "Auto Clicker";
const APP_VERSION: &str = env!("CARGO_PKG_VERSION");

const ICON_RGBA: &[u8] = include_bytes!("../assets/icon.rgba");
const ICON_SIZE: u32 = 128;

const HK_ID_TOGGLE: i32 = 1;
const HK_ID_STOP: i32 = 2;

const WM_HOTKEY_ID: u32 = 0x0312;
const WM_APP_REHOTKEY: u32 = 0x8001;
const WM_APP_TRAY: u32 = 0x8002;
const WM_APP_HK_OFF: u32 = 0x8003;

const TRAY_ID_SHOW: u32 = 101;
const TRAY_ID_START: u32 = 102;
const TRAY_ID_STOP: u32 = 103;
const TRAY_ID_EXIT: u32 = 104;

/// Longest single sleep inside the click loop: bounds Stop latency.
const SLEEP_CHUNK_US: u64 = 15_000;
const SPIN_THRESHOLD_US: u64 = 2_000;
const METRICS_TTL_US: u64 = 500_000;
const PIXEL_CHECK_TTL_US: u64 = 200_000;
/// Anything faster than this is almost certainly a mistake, so the UI says so.
const FAST_WARN_US: u64 = 10_000;

// ============================================================================
// Utilities
// ============================================================================

static EPOCH: OnceLock<Instant> = OnceLock::new();

fn init_epoch() {
    let _ = EPOCH.set(Instant::now());
}

fn now_us() -> u64 {
    EPOCH.get().map(|e| e.elapsed().as_micros() as u64).unwrap_or(0)
}

fn format_hms(us: u64) -> String {
    let secs = us / 1_000_000;
    format!("{:02}:{:02}:{:02}", secs / 3600, (secs / 60) % 60, secs % 60)
}

/// xorshift64* - the only randomness needed is interval jitter.
struct Rng(u64);

impl Rng {
    fn new() -> Self {
        let seed = now_us() ^ 0x9E37_79B9_7F4A_7C15 ^ ((std::process::id() as u64) << 32);
        Self(if seed == 0 { 0x2545_F491_4F6C_DD1D } else { seed })
    }
    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn below(&mut self, bound: u64) -> u64 {
        if bound == 0 { 0 } else { self.next_u64() % bound }
    }
    /// Uniform value in `-span..=span`.
    fn signed(&mut self, span: i64) -> i64 {
        if span <= 0 {
            0
        } else {
            (self.next_u64() % (span as u64 * 2 + 1)) as i64 - span
        }
    }
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

// ============================================================================
// Paths
// ============================================================================

mod paths {
    use std::path::{Path, PathBuf};
    use std::sync::OnceLock;

    static DATA_DIR: OnceLock<PathBuf> = OnceLock::new();

    fn is_writable(dir: &Path) -> bool {
        let probe = dir.join(".auto_clicker_write_test");
        match std::fs::File::create(&probe) {
            Ok(_) => {
                let _ = std::fs::remove_file(&probe);
                true
            }
            Err(_) => false,
        }
    }

    fn exe_dir() -> Option<PathBuf> {
        Some(std::env::current_exe().ok()?.parent()?.to_path_buf())
    }

    #[cfg(windows)]
    fn roaming_dir() -> Option<PathBuf> {
        known_folders::get_known_folder_path(known_folders::KnownFolder::RoamingAppData)
            .map(|p| p.join("AutoClicker"))
    }

    #[cfg(not(windows))]
    fn roaming_dir() -> Option<PathBuf> {
        std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config/auto-clicker"))
    }

    /// Portable (next to the exe) when possible, otherwise %APPDATA%.
    pub fn data_dir() -> &'static Path {
        DATA_DIR.get_or_init(|| {
            if let Some(dir) = exe_dir() {
                if is_writable(&dir) {
                    return dir;
                }
            }
            if let Some(dir) = roaming_dir() {
                if std::fs::create_dir_all(&dir).is_ok() && is_writable(&dir) {
                    return dir;
                }
            }
            PathBuf::from(".")
        })
    }

    pub fn config_path() -> PathBuf {
        data_dir().join("config.json")
    }
    pub fn sub_dir(name: &str) -> PathBuf {
        let dir = data_dir().join(name);
        let _ = std::fs::create_dir_all(&dir);
        dir
    }
    pub fn log_dir() -> PathBuf {
        sub_dir("logs")
    }
    pub fn profiles_dir() -> PathBuf {
        sub_dir("profiles")
    }
    pub fn lang_dir() -> PathBuf {
        sub_dir("lang")
    }
}

// ============================================================================
// Hotkeys
// ============================================================================

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub struct Hotkey {
    pub vk: u32,
    #[serde(default)]
    pub ctrl: bool,
    #[serde(default)]
    pub alt: bool,
    #[serde(default)]
    pub shift: bool,
}

impl Hotkey {
    const fn plain(vk: u32) -> Self {
        Self { vk, ctrl: false, alt: false, shift: false }
    }
    fn label(&self) -> String {
        if self.vk == 0 {
            return "—".into();
        }
        let mut s = String::new();
        if self.ctrl {
            s.push_str("Ctrl+");
        }
        if self.alt {
            s.push_str("Alt+");
        }
        if self.shift {
            s.push_str("Shift+");
        }
        s.push_str(&vk_name(self.vk));
        s
    }
}

fn vk_name(vk: u32) -> String {
    match vk {
        0x00 => "—".into(),
        0x01 => "LMB".into(),
        0x02 => "RMB".into(),
        0x04 => "MMB".into(),
        0x05 => "X1".into(),
        0x06 => "X2".into(),
        0x08 => "Backspace".into(),
        0x09 => "Tab".into(),
        0x0D => "Enter".into(),
        0x10 => "Shift".into(),
        0x11 => "Ctrl".into(),
        0x12 => "Alt".into(),
        0x13 => "Pause".into(),
        0x14 => "CapsLock".into(),
        0x1B => "Esc".into(),
        0x20 => "Space".into(),
        0x21 => "PageUp".into(),
        0x22 => "PageDown".into(),
        0x23 => "End".into(),
        0x24 => "Home".into(),
        0x25 => "Left".into(),
        0x26 => "Up".into(),
        0x27 => "Right".into(),
        0x28 => "Down".into(),
        0x2C => "PrintScreen".into(),
        0x2D => "Insert".into(),
        0x2E => "Delete".into(),
        0x30..=0x39 => char::from(b'0' + (vk - 0x30) as u8).to_string(),
        0x41..=0x5A => char::from(b'A' + (vk - 0x41) as u8).to_string(),
        0x5B => "LWin".into(),
        0x5C => "RWin".into(),
        0x60..=0x69 => format!("Num{}", vk - 0x60),
        0x6A => "Num*".into(),
        0x6B => "Num+".into(),
        0x6D => "Num-".into(),
        0x6E => "Num.".into(),
        0x6F => "Num/".into(),
        0x70..=0x87 => format!("F{}", vk - 0x6F),
        0x90 => "NumLock".into(),
        0x91 => "ScrollLock".into(),
        0xBA => ";".into(),
        0xBB => "=".into(),
        0xBC => ",".into(),
        0xBD => "-".into(),
        0xBE => ".".into(),
        0xBF => "/".into(),
        0xC0 => "`".into(),
        0xDB => "[".into(),
        0xDC => "\\".into(),
        0xDD => "]".into(),
        0xDE => "'".into(),
        _ => format!("VK 0x{vk:02X}"),
    }
}

/// Keys offered in the dropdown next to each binding.
///
/// The "press a key" capture covers everything; this list is the guaranteed path,
/// including keys a focused window never receives.
const HOTKEY_CHOICES: [(&str, u32); 26] = [
    ("—", 0x00),
    ("F1", 0x70), ("F2", 0x71), ("F3", 0x72), ("F4", 0x73),
    ("F5", 0x74), ("F6", 0x75), ("F7", 0x76), ("F8", 0x77),
    ("F9", 0x78), ("F10", 0x79), ("F11", 0x7A), ("F12", 0x7B),
    ("Pause", 0x13), ("ScrollLock", 0x91), ("Insert", 0x2D), ("Delete", 0x2E),
    ("Home", 0x24), ("End", 0x23), ("PageUp", 0x21), ("PageDown", 0x22),
    ("Num0", 0x60), ("Num1", 0x61), ("Num*", 0x6A), ("Num-", 0x6D), ("Num+", 0x6B),
];

static HOOK_THREAD_ID: AtomicU32 = AtomicU32::new(0);
static HK_FAILED: AtomicU32 = AtomicU32::new(0);
static PENDING_HOTKEYS: Mutex<[Hotkey; 2]> =
    Mutex::new([Hotkey::plain(0x75), Hotkey::plain(0x78)]);

/// 0 = idle, otherwise the slot currently waiting for a key press.
static CAPTURE_SLOT: AtomicU32 = AtomicU32::new(0);

fn publish_hotkeys(cfg: &AppConfig) {
    *PENDING_HOTKEYS.lock() = [cfg.hotkey_toggle, cfg.hotkey_stop];
}

fn request_hotkey_message(msg: u32) {
    #[cfg(windows)]
    {
        let tid = HOOK_THREAD_ID.load(Ordering::Relaxed);
        if tid != 0 {
            unsafe {
                let _ = win32::PostThreadMessageW(tid, msg, win32::WPARAM(0), win32::LPARAM(0));
            }
        }
    }
    #[cfg(not(windows))]
    let _ = msg;
}

fn begin_capture(slot: u32) {
    CAPTURE_SLOT.store(slot, Ordering::Relaxed);
    // Otherwise the currently bound key is swallowed by RegisterHotKey and could
    // never be re-assigned onto another slot.
    request_hotkey_message(WM_APP_HK_OFF);
}

fn end_capture() {
    CAPTURE_SLOT.store(0, Ordering::Relaxed);
    request_hotkey_message(WM_APP_REHOTKEY);
}

/// Scans the keyboard for a key that is physically down.
///
/// `GetAsyncKeyState` reads hardware state, so this works no matter which window has
/// focus and needs no hook - unlike window input, which never sees Pause or NumPad.
#[cfg(windows)]
fn scan_pressed_key() -> Option<Hotkey> {
    use win32::*;
    unsafe {
        let held = |vk: i32| (GetAsyncKeyState(vk) as u16 & 0x8000) != 0;
        if held(0x1B) {
            return Some(Hotkey::plain(0)); // Esc cancels
        }
        for vk in 0x01u32..=0xFEu32 {
            // Modifiers on their own are not a binding, and mouse buttons would make
            // it impossible to click anything in the UI.
            if matches!(vk, 0x01..=0x06 | 0x10..=0x12 | 0x5B | 0x5C | 0xA0..=0xA5) {
                continue;
            }
            if held(vk as i32) {
                return Some(Hotkey {
                    vk,
                    ctrl: held(0x11),
                    alt: held(0x12),
                    shift: held(0x10),
                });
            }
        }
        None
    }
}

#[cfg(not(windows))]
fn scan_pressed_key() -> Option<Hotkey> {
    None
}

// ============================================================================
// Configuration
// ============================================================================

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(default)]
pub struct AppConfig {
    // interval
    pub interval_h: u64,
    pub interval_m: u64,
    pub interval_s: u64,
    pub interval_ms: u64,
    pub random_offset_enabled: bool,
    pub random_offset_ms: u64,

    // action
    /// 0 = mouse button, 1 = keyboard key.
    pub action_mode: usize,
    /// 0 L, 1 R, 2 M, 3 X1, 4 X2.
    pub mouse_button: usize,
    /// 0 single, 1 double, 2 triple.
    pub click_type: usize,
    pub hold_ms: u64,
    pub key_vk: u32,

    // repeat
    pub repeat_infinite: bool,
    pub repeat_times: u64,

    // position: 0 = current, 1 = fixed point, 2 = cycle through the list
    pub position_mode: usize,
    pub pos_x: i32,
    pub pos_y: i32,
    pub jitter_px: i32,
    pub points: Vec<(i32, i32)>,
    pub return_cursor: bool,

    // time limit
    pub limit_enabled: bool,
    pub limit_h: u64,
    pub limit_m: u64,
    pub limit_s: u64,

    // pixel stop condition
    pub pixel_enabled: bool,
    pub pixel_x: i32,
    pub pixel_y: i32,
    pub pixel_r: u8,
    pub pixel_g: u8,
    pub pixel_b: u8,
    pub pixel_tolerance: u32,
    /// 0 = stop when it matches, 1 = stop when it differs.
    pub pixel_mode: usize,

    // hotkeys
    pub hotkey_toggle: Hotkey,
    pub hotkey_stop: Hotkey,
    /// Click only while the toggle key is physically held down.
    pub hold_mode: bool,

    // appearance
    pub default_lang: usize,
    pub default_theme: usize,
    pub transparent_ui: bool,
    pub always_on_top: bool,
    pub tray_enabled: bool,
    pub close_to_tray: bool,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            interval_h: 0,
            interval_m: 0,
            interval_s: 0,
            interval_ms: 100,
            random_offset_enabled: false,
            random_offset_ms: 40,

            action_mode: 0,
            mouse_button: 0,
            click_type: 0,
            hold_ms: 0,
            key_vk: 0x20, // Space

            repeat_infinite: true,
            repeat_times: 1,

            position_mode: 0,
            pos_x: 0,
            pos_y: 0,
            jitter_px: 0,
            points: Vec::new(),
            return_cursor: false,

            limit_enabled: false,
            limit_h: 0,
            limit_m: 0,
            limit_s: 0,

            pixel_enabled: false,
            pixel_x: 0,
            pixel_y: 0,
            pixel_r: 255,
            pixel_g: 0,
            pixel_b: 0,
            pixel_tolerance: 20,
            pixel_mode: 0,

            hotkey_toggle: Hotkey::plain(0x75), // F6, same as OP Auto Clicker
            hotkey_stop: Hotkey::plain(0x78),   // F9
            hold_mode: false,

            default_lang: 0,
            default_theme: 0,
            transparent_ui: true,
            always_on_top: true,
            tray_enabled: true,
            close_to_tray: true,
        }
    }
}

impl AppConfig {
    fn sanitize(&mut self) {
        self.interval_h = self.interval_h.min(240);
        self.interval_m = self.interval_m.min(59);
        self.interval_s = self.interval_s.min(59);
        self.interval_ms = self.interval_ms.min(999);
        self.random_offset_ms = self.random_offset_ms.min(600_000);
        self.action_mode = self.action_mode.min(1);
        self.mouse_button = self.mouse_button.min(4);
        self.click_type = self.click_type.min(2);
        self.hold_ms = self.hold_ms.min(5_000);
        self.repeat_times = self.repeat_times.clamp(1, 100_000_000);
        self.position_mode = self.position_mode.min(2);
        self.jitter_px = self.jitter_px.clamp(0, 500);
        self.points.truncate(64);
        self.limit_h = self.limit_h.min(240);
        self.limit_m = self.limit_m.min(59);
        self.limit_s = self.limit_s.min(59);
        self.pixel_tolerance = self.pixel_tolerance.min(255);
        self.pixel_mode = self.pixel_mode.min(1);
        self.default_lang = self.default_lang.min(6);
        self.default_theme = self.default_theme.min(THEME_NAMES.len() - 1);
        if self.key_vk > 0xFE {
            self.key_vk = 0x20;
        }
    }

    /// Interval between clicks, never below 1 ms.
    fn interval_us(&self) -> u64 {
        let us = (self.interval_h * 3_600_000
            + self.interval_m * 60_000
            + self.interval_s * 1_000
            + self.interval_ms)
            * 1_000;
        us.max(1_000)
    }

    fn limit_us(&self) -> u64 {
        (self.limit_h * 3600 + self.limit_m * 60 + self.limit_s) * 1_000_000
    }
}

fn load_config_from(path: &Path) -> AppConfig {
    let mut cfg = std::fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str::<AppConfig>(&s).ok())
        .unwrap_or_default();
    cfg.sanitize();
    cfg
}

fn save_config_to(path: &Path, cfg: &AppConfig) -> Result<()> {
    let json = serde_json::to_string_pretty(cfg)?;
    std::fs::write(path, json).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

fn list_profiles() -> Vec<String> {
    let mut out = Vec::new();
    if let Ok(entries) = std::fs::read_dir(paths::profiles_dir()) {
        for e in entries.flatten() {
            let p = e.path();
            if p.extension().and_then(|x| x.to_str()) == Some("json") {
                if let Some(stem) = p.file_stem().and_then(|x| x.to_str()) {
                    out.push(stem.to_string());
                }
            }
        }
    }
    out.sort();
    out
}

fn profile_path(name: &str) -> PathBuf {
    let safe: String = name
        .chars()
        .map(|c| if c.is_alphanumeric() || c == '-' || c == '_' || c == ' ' { c } else { '_' })
        .collect();
    paths::profiles_dir().join(format!("{}.json", safe.trim()))
}

// ============================================================================
// Platform layer
// ============================================================================

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MouseButton {
    Left,
    Right,
    Middle,
    X1,
    X2,
}

impl MouseButton {
    fn from_index(i: usize) -> Self {
        match i {
            1 => Self::Right,
            2 => Self::Middle,
            3 => Self::X1,
            4 => Self::X2,
            _ => Self::Left,
        }
    }
}

#[cfg(windows)]
mod platform {
    use super::win32::*;
    use super::{APP_TITLE, METRICS_TTL_US, MouseButton, now_us, wide};
    use std::ffi::c_void;
    use std::sync::atomic::{AtomicI32, AtomicIsize, AtomicU64, Ordering};

    static HWND_CACHE: AtomicIsize = AtomicIsize::new(0);
    static HWND_LAST_TRY: AtomicU64 = AtomicU64::new(0);
    static VS: [AtomicI32; 4] =
        [AtomicI32::new(0), AtomicI32::new(0), AtomicI32::new(1), AtomicI32::new(1)];
    static VS_LAST: AtomicU64 = AtomicU64::new(0);

    /// Our own top-level window, cached and validated against the process id.
    pub fn app_hwnd() -> HWND {
        let cached = HWND_CACHE.load(Ordering::Relaxed);
        if cached != 0 {
            let hwnd = HWND(cached as *mut c_void);
            if unsafe { IsWindow(Some(hwnd)) }.as_bool() {
                return hwnd;
            }
            HWND_CACHE.store(0, Ordering::Relaxed);
        }
        let now = now_us();
        let last = HWND_LAST_TRY.load(Ordering::Relaxed);
        if last != 0 && now.saturating_sub(last) < 1_000_000 {
            return HWND::default();
        }
        HWND_LAST_TRY.store(now, Ordering::Relaxed);

        unsafe {
            let title = wide(APP_TITLE);
            if let Ok(hwnd) = FindWindowW(None, PCWSTR(title.as_ptr())) {
                if !hwnd.0.is_null() {
                    let mut pid = 0u32;
                    let _ = GetWindowThreadProcessId(hwnd, Some(&mut pid));
                    if pid == GetCurrentProcessId() {
                        HWND_CACHE.store(hwnd.0 as isize, Ordering::Relaxed);
                        return hwnd;
                    }
                }
            }
        }
        HWND::default()
    }

    pub fn apply_system_backdrop(hwnd: HWND, backdrop: i32) {
        if hwnd.0.is_null() {
            return;
        }
        unsafe {
            let dark_mode: i32 = 1;
            let _ = DwmSetWindowAttribute(
                hwnd,
                DWMWA_USE_IMMERSIVE_DARK_MODE,
                &dark_mode as *const i32 as *const c_void,
                std::mem::size_of::<i32>() as u32,
            );
            let backdrop_type: i32 = backdrop;
            let result = DwmSetWindowAttribute(
                hwnd,
                DWMWINDOWATTRIBUTE(38),
                &backdrop_type as *const i32 as *const c_void,
                std::mem::size_of::<i32>() as u32,
            );
            if result.is_err() && backdrop > 1 {
                let bb = DWM_BLURBEHIND {
                    dwFlags: DWM_BB_ENABLE,
                    fEnable: true.into(),
                    hRgnBlur: HRGN::default(),
                    fTransitionOnMaximized: false.into(),
                };
                let _ = DwmEnableBlurBehindWindow(hwnd, &bb);
            }
        }
    }

    fn virtual_screen() -> (i32, i32, i32, i32) {
        let now = now_us();
        let last = VS_LAST.load(Ordering::Relaxed);
        if last == 0 || now.saturating_sub(last) >= METRICS_TTL_US {
            unsafe {
                VS[0].store(GetSystemMetrics(SM_XVIRTUALSCREEN), Ordering::Relaxed);
                VS[1].store(GetSystemMetrics(SM_YVIRTUALSCREEN), Ordering::Relaxed);
                VS[2].store(GetSystemMetrics(SM_CXVIRTUALSCREEN).max(1), Ordering::Relaxed);
                VS[3].store(GetSystemMetrics(SM_CYVIRTUALSCREEN).max(1), Ordering::Relaxed);
            }
            VS_LAST.store(now, Ordering::Relaxed);
        }
        (
            VS[0].load(Ordering::Relaxed),
            VS[1].load(Ordering::Relaxed),
            VS[2].load(Ordering::Relaxed),
            VS[3].load(Ordering::Relaxed),
        )
    }

    /// `w - 1` as the denominator so the right/bottom-most pixel stays reachable.
    pub fn normalize_abs(x: i32, y: i32, vx: i32, vy: i32, vw: i32, vh: i32) -> (i32, i32) {
        let dx = (vw - 1).max(1) as f64;
        let dy = (vh - 1).max(1) as f64;
        let nx = (((x - vx) as f64 / dx) * 65535.0).round().clamp(0.0, 65535.0) as i32;
        let ny = (((y - vy) as f64 / dy) * 65535.0).round().clamp(0.0, 65535.0) as i32;
        (nx, ny)
    }

    pub fn move_cursor(x: i32, y: i32) {
        unsafe {
            let (vx, vy, vw, vh) = virtual_screen();
            let (nx, ny) = normalize_abs(x, y, vx, vy, vw, vh);
            let input = INPUT {
                r#type: INPUT_MOUSE,
                Anonymous: INPUT_0 {
                    mi: MOUSEINPUT {
                        dx: nx,
                        dy: ny,
                        mouseData: 0,
                        dwFlags: MOUSEEVENTF_MOVE
                            | MOUSEEVENTF_ABSOLUTE
                            | MOUSEEVENTF_VIRTUALDESK,
                        time: 0,
                        dwExtraInfo: 0,
                    },
                },
            };
            SendInput(&[input], std::mem::size_of::<INPUT>() as i32);
        }
    }

    pub fn mouse_button_event(button: MouseButton, down: bool) {
        let (flags, data) = match (button, down) {
            (MouseButton::Left, true) => (MOUSEEVENTF_LEFTDOWN, 0),
            (MouseButton::Left, false) => (MOUSEEVENTF_LEFTUP, 0),
            (MouseButton::Right, true) => (MOUSEEVENTF_RIGHTDOWN, 0),
            (MouseButton::Right, false) => (MOUSEEVENTF_RIGHTUP, 0),
            (MouseButton::Middle, true) => (MOUSEEVENTF_MIDDLEDOWN, 0),
            (MouseButton::Middle, false) => (MOUSEEVENTF_MIDDLEUP, 0),
            (MouseButton::X1, true) => (MOUSEEVENTF_XDOWN, 1),
            (MouseButton::X1, false) => (MOUSEEVENTF_XUP, 1),
            (MouseButton::X2, true) => (MOUSEEVENTF_XDOWN, 2),
            (MouseButton::X2, false) => (MOUSEEVENTF_XUP, 2),
        };
        unsafe {
            let input = INPUT {
                r#type: INPUT_MOUSE,
                Anonymous: INPUT_0 {
                    mi: MOUSEINPUT {
                        dx: 0,
                        dy: 0,
                        mouseData: data,
                        dwFlags: flags,
                        time: 0,
                        dwExtraInfo: 0,
                    },
                },
            };
            SendInput(&[input], std::mem::size_of::<INPUT>() as i32);
        }
    }

    /// Sends a key using its scancode where possible: games that read raw input
    /// ignore virtual-key-only events.
    pub fn key_event(vk: u16, down: bool) {
        unsafe {
            let scan = MapVirtualKeyW(vk as u32, MAPVK_VK_TO_VSC) as u16;
            let mut flags = KEYBD_EVENT_FLAGS(0);
            if !down {
                flags |= KEYEVENTF_KEYUP;
            }
            let ki = if scan != 0 {
                flags |= KEYEVENTF_SCANCODE;
                KEYBDINPUT {
                    wVk: VIRTUAL_KEY(0),
                    wScan: scan,
                    dwFlags: flags,
                    time: 0,
                    dwExtraInfo: 0,
                }
            } else {
                KEYBDINPUT {
                    wVk: VIRTUAL_KEY(vk),
                    wScan: 0,
                    dwFlags: flags,
                    time: 0,
                    dwExtraInfo: 0,
                }
            };
            let input = INPUT { r#type: INPUT_KEYBOARD, Anonymous: INPUT_0 { ki } };
            SendInput(&[input], std::mem::size_of::<INPUT>() as i32);
        }
    }

    pub fn key_is_down(vk: u32) -> bool {
        if vk == 0 {
            return false;
        }
        unsafe { (GetAsyncKeyState(vk as i32) as u16 & 0x8000) != 0 }
    }

    pub fn cursor_pos() -> (i32, i32) {
        unsafe {
            let mut p = POINT::default();
            let _ = GetCursorPos(&mut p);
            (p.x, p.y)
        }
    }

    pub fn screen_pixel(x: i32, y: i32) -> Option<(u8, u8, u8)> {
        unsafe {
            let hdc = GetDC(None);
            if hdc.is_invalid() {
                return None;
            }
            let c = GetPixel(hdc, x, y);
            ReleaseDC(None, hdc);
            if c.0 == 0xFFFF_FFFF {
                return None;
            }
            Some(((c.0 & 0xFF) as u8, ((c.0 >> 8) & 0xFF) as u8, ((c.0 >> 16) & 0xFF) as u8))
        }
    }

    pub fn begin_high_res_timer() {
        unsafe {
            let _ = timeBeginPeriod(1);
        }
    }
    pub fn end_high_res_timer() {
        unsafe {
            let _ = timeEndPeriod(1);
        }
    }

    pub fn set_window_hidden(hidden: bool) {
        unsafe {
            let hwnd = app_hwnd();
            if hwnd.0.is_null() {
                return;
            }
            if hidden {
                let _ = ShowWindow(hwnd, SW_HIDE);
            } else {
                let _ = ShowWindow(hwnd, SW_SHOW);
                let _ = SetForegroundWindow(hwnd);
            }
        }
    }

    pub fn request_app_close() {
        unsafe {
            let hwnd = app_hwnd();
            if !hwnd.0.is_null() {
                let _ = PostMessageW(Some(hwnd), WM_CLOSE, WPARAM(0), LPARAM(0));
            }
        }
    }

    pub fn acquire_single_instance() -> bool {
        unsafe {
            match CreateMutexW(None, true, w!("Local\\AutoClicker_SingleInstance_v1")) {
                Ok(handle) => {
                    if GetLastError() == ERROR_ALREADY_EXISTS {
                        let _ = CloseHandle(handle);
                        false
                    } else {
                        // Never closed on purpose: the mutex must outlive main().
                        true
                    }
                }
                Err(_) => true,
            }
        }
    }

    pub fn focus_existing_instance() {
        unsafe {
            let title = wide(APP_TITLE);
            if let Ok(hwnd) = FindWindowW(None, PCWSTR(title.as_ptr())) {
                if !hwnd.0.is_null() {
                    let _ = ShowWindow(hwnd, SW_RESTORE);
                    let _ = SetForegroundWindow(hwnd);
                }
            }
        }
    }

    /// Resolved dynamically so the crate needs no Win32_System_Console feature.
    pub fn attach_parent_console() {
        const ATTACH_PARENT_PROCESS: u32 = 0xFFFF_FFFF;
        unsafe {
            let Ok(kernel32) = GetModuleHandleW(w!("kernel32.dll")) else {
                return;
            };
            let Some(sym) = GetProcAddress(kernel32, PCSTR(b"AttachConsole\0".as_ptr())) else {
                return;
            };
            let attach: unsafe extern "system" fn(u32) -> i32 = std::mem::transmute(sym);
            let _ = attach(ATTACH_PARENT_PROCESS);
        }
    }

    pub fn set_dpi_awareness() {
        unsafe {
            let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        }
    }
}

#[cfg(not(windows))]
mod platform {
    use super::MouseButton;

    pub fn app_hwnd() {}
    pub fn apply_system_backdrop(_: (), _: i32) {}
    pub fn move_cursor(_: i32, _: i32) {}
    pub fn mouse_button_event(_: MouseButton, _: bool) {}
    pub fn key_event(_: u16, _: bool) {}
    pub fn key_is_down(_: u32) -> bool {
        false
    }
    pub fn cursor_pos() -> (i32, i32) {
        (0, 0)
    }
    pub fn screen_pixel(_: i32, _: i32) -> Option<(u8, u8, u8)> {
        None
    }
    pub fn begin_high_res_timer() {}
    pub fn end_high_res_timer() {}
    pub fn set_window_hidden(_: bool) {}
    pub fn request_app_close() {}
    pub fn acquire_single_instance() -> bool {
        true
    }
    pub fn focus_existing_instance() {}
    pub fn attach_parent_console() {}
    pub fn set_dpi_awareness() {}
    pub fn normalize_abs(_: i32, _: i32, _: i32, _: i32, _: i32, _: i32) -> (i32, i32) {
        (0, 0)
    }
}

// ============================================================================
// Shared state
// ============================================================================

static WINDOW_VISIBLE: AtomicBool = AtomicBool::new(true);
static ALLOW_CLOSE: AtomicBool = AtomicBool::new(false);

fn set_window_visible(visible: bool) {
    WINDOW_VISIBLE.store(visible, Ordering::Relaxed);
    platform::set_window_hidden(!visible);
}

fn toggle_main_window() {
    set_window_visible(!WINDOW_VISIBLE.load(Ordering::Relaxed));
}

fn quit_application() {
    ALLOW_CLOSE.store(true, Ordering::Relaxed);
    set_window_visible(true);
    platform::request_app_close();
}

pub struct AppState {
    pub running: AtomicBool,
    pub stop: AtomicBool,
    pub generation: AtomicU64,

    // timing
    pub interval_us: AtomicU64,
    pub random_offset_us: AtomicU64,
    pub hold_us: AtomicU64,

    // action
    pub action_mode: AtomicU32,
    pub mouse_button: AtomicU32,
    pub click_type: AtomicU32,
    pub key_vk: AtomicU32,

    // repeat
    pub repeat_infinite: AtomicBool,
    pub repeat_times: AtomicU64,

    // position
    pub position_mode: AtomicU32,
    pub pos_x: AtomicI32,
    pub pos_y: AtomicI32,
    pub jitter_px: AtomicI32,
    pub return_cursor: AtomicBool,
    pub points: Mutex<Vec<(i32, i32)>>,

    // stop conditions
    pub limit_us: AtomicU64,
    pub pixel_enabled: AtomicBool,
    pub pixel_x: AtomicI32,
    pub pixel_y: AtomicI32,
    pub pixel_rgb: AtomicU32,
    pub pixel_tolerance: AtomicU32,
    pub pixel_mode: AtomicU32,

    // hold-to-click
    pub hold_mode: AtomicBool,
    pub hold_vk: AtomicU32,

    // stats
    pub clicks: AtomicU64,
    pub started_us: AtomicU64,
    pub stopped_by_pixel: AtomicBool,
}

impl AppState {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            running: AtomicBool::new(false),
            stop: AtomicBool::new(false),
            generation: AtomicU64::new(0),

            interval_us: AtomicU64::new(100_000),
            random_offset_us: AtomicU64::new(0),
            hold_us: AtomicU64::new(0),

            action_mode: AtomicU32::new(0),
            mouse_button: AtomicU32::new(0),
            click_type: AtomicU32::new(0),
            key_vk: AtomicU32::new(0x20),

            repeat_infinite: AtomicBool::new(true),
            repeat_times: AtomicU64::new(1),

            position_mode: AtomicU32::new(0),
            pos_x: AtomicI32::new(0),
            pos_y: AtomicI32::new(0),
            jitter_px: AtomicI32::new(0),
            return_cursor: AtomicBool::new(false),
            points: Mutex::new(Vec::new()),

            limit_us: AtomicU64::new(0),
            pixel_enabled: AtomicBool::new(false),
            pixel_x: AtomicI32::new(0),
            pixel_y: AtomicI32::new(0),
            pixel_rgb: AtomicU32::new(0xFF_0000),
            pixel_tolerance: AtomicU32::new(20),
            pixel_mode: AtomicU32::new(0),

            hold_mode: AtomicBool::new(false),
            hold_vk: AtomicU32::new(0),

            clicks: AtomicU64::new(0),
            started_us: AtomicU64::new(0),
            stopped_by_pixel: AtomicBool::new(false),
        })
    }
}

/// Pushes every setting into the live state.
///
/// Called at startup and once per UI frame, so edits apply to a run in progress.
fn apply_config_to_state(cfg: &AppConfig, state: &AppState) {
    state.interval_us.store(cfg.interval_us(), Ordering::Relaxed);
    state.random_offset_us.store(
        if cfg.random_offset_enabled { cfg.random_offset_ms * 1_000 } else { 0 },
        Ordering::Relaxed,
    );
    state.hold_us.store(cfg.hold_ms * 1_000, Ordering::Relaxed);

    state.action_mode.store(cfg.action_mode as u32, Ordering::Relaxed);
    state.mouse_button.store(cfg.mouse_button as u32, Ordering::Relaxed);
    state.click_type.store(cfg.click_type as u32, Ordering::Relaxed);
    state.key_vk.store(cfg.key_vk, Ordering::Relaxed);

    state.repeat_infinite.store(cfg.repeat_infinite, Ordering::Relaxed);
    state.repeat_times.store(cfg.repeat_times, Ordering::Relaxed);

    state.position_mode.store(cfg.position_mode as u32, Ordering::Relaxed);
    state.pos_x.store(cfg.pos_x, Ordering::Relaxed);
    state.pos_y.store(cfg.pos_y, Ordering::Relaxed);
    state.jitter_px.store(cfg.jitter_px, Ordering::Relaxed);
    state.return_cursor.store(cfg.return_cursor, Ordering::Relaxed);
    *state.points.lock() = cfg.points.clone();

    state.limit_us.store(if cfg.limit_enabled { cfg.limit_us() } else { 0 }, Ordering::Relaxed);
    state.pixel_enabled.store(cfg.pixel_enabled, Ordering::Relaxed);
    state.pixel_x.store(cfg.pixel_x, Ordering::Relaxed);
    state.pixel_y.store(cfg.pixel_y, Ordering::Relaxed);
    let rgb = ((cfg.pixel_r as u32) << 16) | ((cfg.pixel_g as u32) << 8) | cfg.pixel_b as u32;
    state.pixel_rgb.store(rgb, Ordering::Relaxed);
    state.pixel_tolerance.store(cfg.pixel_tolerance, Ordering::Relaxed);
    state.pixel_mode.store(cfg.pixel_mode as u32, Ordering::Relaxed);

    state.hold_mode.store(cfg.hold_mode, Ordering::Relaxed);
    state.hold_vk.store(cfg.hotkey_toggle.vk, Ordering::Relaxed);
}

// ============================================================================
// Click engine
// ============================================================================

fn pixel_condition_met(state: &AppState) -> bool {
    if !state.pixel_enabled.load(Ordering::Relaxed) {
        return false;
    }
    let (x, y) = (state.pixel_x.load(Ordering::Relaxed), state.pixel_y.load(Ordering::Relaxed));
    let Some((r, g, b)) = platform::screen_pixel(x, y) else {
        return false;
    };
    let want = state.pixel_rgb.load(Ordering::Relaxed);
    let (wr, wg, wb) = (
        ((want >> 16) & 0xFF) as i32,
        ((want >> 8) & 0xFF) as i32,
        (want & 0xFF) as i32,
    );
    let tol = state.pixel_tolerance.load(Ordering::Relaxed) as i32;
    let matches = (r as i32 - wr).abs() <= tol
        && (g as i32 - wg).abs() <= tol
        && (b as i32 - wb).abs() <= tol;
    if state.pixel_mode.load(Ordering::Relaxed) == 0 { matches } else { !matches }
}

/// Performs one click (or keystroke) with the configured repeat count and hold time.
fn perform_action(state: &AppState) {
    let hold = Duration::from_micros(state.hold_us.load(Ordering::Relaxed));
    let repeats = match state.click_type.load(Ordering::Relaxed) {
        1 => 2,
        2 => 3,
        _ => 1,
    };
    let keyboard = state.action_mode.load(Ordering::Relaxed) == 1;
    let button = MouseButton::from_index(state.mouse_button.load(Ordering::Relaxed) as usize);
    let vk = state.key_vk.load(Ordering::Relaxed) as u16;

    for i in 0..repeats {
        if keyboard {
            platform::key_event(vk, true);
            if !hold.is_zero() {
                spin_sleep::sleep(hold);
            }
            platform::key_event(vk, false);
        } else {
            platform::mouse_button_event(button, true);
            if !hold.is_zero() {
                spin_sleep::sleep(hold);
            }
            platform::mouse_button_event(button, false);
        }
        // Windows only treats two presses as a double click if they are close
        // together, so the gap between them stays fixed and short.
        if i + 1 < repeats {
            spin_sleep::sleep(Duration::from_millis(30));
        }
    }
}

/// Chunked wait that stays responsive to Stop.
fn wait_us(state: &AppState, generation: u64, total_us: u64) -> bool {
    let deadline = Instant::now() + Duration::from_micros(total_us);
    loop {
        if state.stop.load(Ordering::Relaxed)
            || state.generation.load(Ordering::Relaxed) != generation
        {
            return false;
        }
        let now = Instant::now();
        if now >= deadline {
            return true;
        }
        let remaining = (deadline - now).as_micros() as u64;
        if remaining > SPIN_THRESHOLD_US {
            let chunk = remaining.saturating_sub(1_000).min(SLEEP_CHUNK_US);
            std::thread::sleep(Duration::from_micros(chunk.max(1)));
        } else {
            spin_sleep::sleep(Duration::from_micros(remaining));
            return true;
        }
    }
}

fn clicker_loop(state: Arc<AppState>, generation: u64) {
    platform::begin_high_res_timer();
    state.clicks.store(0, Ordering::Relaxed);
    state.stopped_by_pixel.store(false, Ordering::Relaxed);
    state.started_us.store(now_us(), Ordering::Relaxed);

    let mut rng = Rng::new();
    let start = Instant::now();
    let mut count: u64 = 0;
    let mut point_idx: usize = 0;
    let mut last_pixel_check: u64 = 0;
    let entry_cursor = platform::cursor_pos();

    loop {
        if state.stop.load(Ordering::Relaxed)
            || state.generation.load(Ordering::Relaxed) != generation
        {
            break;
        }

        // ---- hold-to-click gate --------------------------------------------
        if state.hold_mode.load(Ordering::Relaxed) {
            let vk = state.hold_vk.load(Ordering::Relaxed);
            if !platform::key_is_down(vk) {
                std::thread::sleep(Duration::from_millis(8));
                continue;
            }
        }

        let elapsed_us = start.elapsed().as_micros() as u64;

        // ---- stop conditions -------------------------------------------------
        let limit = state.limit_us.load(Ordering::Relaxed);
        if limit > 0 && elapsed_us >= limit {
            break;
        }
        if state.pixel_enabled.load(Ordering::Relaxed)
            && elapsed_us.saturating_sub(last_pixel_check) >= PIXEL_CHECK_TTL_US
        {
            last_pixel_check = elapsed_us;
            if pixel_condition_met(&state) {
                info!("pixel condition met - stopping");
                state.stopped_by_pixel.store(true, Ordering::Relaxed);
                break;
            }
        }

        // ---- where to click ---------------------------------------------------
        match state.position_mode.load(Ordering::Relaxed) {
            1 => {
                let jitter = state.jitter_px.load(Ordering::Relaxed) as i64;
                let x = state.pos_x.load(Ordering::Relaxed) + rng.signed(jitter) as i32;
                let y = state.pos_y.load(Ordering::Relaxed) + rng.signed(jitter) as i32;
                platform::move_cursor(x, y);
            }
            2 => {
                let points = state.points.lock().clone();
                if !points.is_empty() {
                    let (px, py) = points[point_idx % points.len()];
                    point_idx = point_idx.wrapping_add(1);
                    let jitter = state.jitter_px.load(Ordering::Relaxed) as i64;
                    platform::move_cursor(
                        px + rng.signed(jitter) as i32,
                        py + rng.signed(jitter) as i32,
                    );
                }
            }
            _ => {} // current location: never move the cursor
        }

        perform_action(&state);
        count += 1;
        state.clicks.store(count, Ordering::Relaxed);

        if !state.repeat_infinite.load(Ordering::Relaxed)
            && count >= state.repeat_times.load(Ordering::Relaxed)
        {
            break;
        }

        // ---- wait for the next one --------------------------------------------
        let base = state.interval_us.load(Ordering::Relaxed);
        let spread = state.random_offset_us.load(Ordering::Relaxed);
        let wait = if spread == 0 {
            base
        } else {
            // OP-style "+/-": the interval varies symmetrically around the base.
            let off = rng.below(spread * 2 + 1) as i64 - spread as i64;
            (base as i64 + off).max(1_000) as u64
        };
        if !wait_us(&state, generation, wait) {
            break;
        }
    }

    if state.return_cursor.load(Ordering::Relaxed)
        && state.position_mode.load(Ordering::Relaxed) != 0
    {
        platform::move_cursor(entry_cursor.0, entry_cursor.1);
    }

    platform::end_high_res_timer();
    if state.generation.load(Ordering::Relaxed) == generation {
        state.running.store(false, Ordering::Relaxed);
    }
    info!("clicker stopped after {count} action(s)");
}

fn start_clicking(state: &Arc<AppState>) {
    if state.running.load(Ordering::Relaxed) {
        return;
    }
    let generation = state.generation.fetch_add(1, Ordering::SeqCst) + 1;
    state.stop.store(false, Ordering::Relaxed);
    state.running.store(true, Ordering::Relaxed);
    let s = state.clone();
    match std::thread::Builder::new()
        .name("clicker".into())
        .spawn(move || clicker_loop(s, generation))
    {
        Ok(_) => info!("clicker started (generation {generation})"),
        Err(e) => {
            warn!("failed to spawn clicker thread: {e}");
            state.running.store(false, Ordering::Relaxed);
        }
    }
}

fn stop_clicking(state: &AppState) {
    if state.running.load(Ordering::Relaxed) {
        state.stop.store(true, Ordering::Relaxed);
    }
}

fn toggle_clicking(state: &Arc<AppState>) {
    if state.running.load(Ordering::Relaxed) {
        stop_clicking(state);
    } else {
        start_clicking(state);
    }
}

// ============================================================================
// Tray icon
// ============================================================================

#[cfg(windows)]
static GLOBAL_STATE: OnceLock<Arc<AppState>> = OnceLock::new();

#[cfg(windows)]
mod tray {
    use super::win32::*;
    use super::*;
    use std::ffi::c_void;
    use std::sync::atomic::AtomicIsize;

    static TRAY_HWND: AtomicIsize = AtomicIsize::new(0);
    static TRAY_ADDED: AtomicBool = AtomicBool::new(false);

    fn icon_handle(hinst: HINSTANCE) -> HICON {
        unsafe {
            // Resource id 1 is what winresource assigns to the embedded icon.
            if let Ok(icon) = LoadIconW(Some(hinst), PCWSTR(1 as *const u16)) {
                if !icon.is_invalid() {
                    return icon;
                }
            }
            LoadIconW(None, IDI_APPLICATION).unwrap_or_default()
        }
    }

    pub fn init() {
        unsafe {
            let hinst = GetModuleHandleW(None).map(|h| HINSTANCE(h.0)).unwrap_or_default();
            let class = w!("AutoClickerTrayWnd");
            let wc = WNDCLASSW {
                lpfnWndProc: Some(wndproc),
                hInstance: hinst,
                lpszClassName: class,
                ..Default::default()
            };
            RegisterClassW(&wc);

            let hwnd = match CreateWindowExW(
                WINDOW_EX_STYLE(0),
                class,
                w!("Auto Clicker Tray"),
                WINDOW_STYLE(0),
                0,
                0,
                0,
                0,
                Some(HWND_MESSAGE),
                None,
                Some(hinst),
                None,
            ) {
                Ok(h) => h,
                Err(e) => {
                    warn!("tray window could not be created: {e}");
                    return;
                }
            };
            TRAY_HWND.store(hwnd.0 as isize, Ordering::Relaxed);

            let mut nid = NOTIFYICONDATAW {
                cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
                hWnd: hwnd,
                uID: 1,
                uFlags: NIF_ICON | NIF_MESSAGE | NIF_TIP,
                uCallbackMessage: WM_APP_TRAY,
                hIcon: icon_handle(hinst),
                ..Default::default()
            };
            let tip = wide(APP_TITLE);
            let n = tip.len().min(nid.szTip.len());
            nid.szTip[..n].copy_from_slice(&tip[..n]);

            if Shell_NotifyIconW(NIM_ADD, &nid).as_bool() {
                TRAY_ADDED.store(true, Ordering::Relaxed);
                info!("tray icon added");
            } else {
                warn!("Shell_NotifyIconW(NIM_ADD) failed");
            }
        }
    }

    pub fn is_active() -> bool {
        TRAY_ADDED.load(Ordering::Relaxed)
    }

    pub fn shutdown() {
        if !TRAY_ADDED.swap(false, Ordering::Relaxed) {
            return;
        }
        unsafe {
            let hwnd = HWND(TRAY_HWND.load(Ordering::Relaxed) as *mut c_void);
            let nid = NOTIFYICONDATAW {
                cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
                hWnd: hwnd,
                uID: 1,
                ..Default::default()
            };
            let _ = Shell_NotifyIconW(NIM_DELETE, &nid);
            let _ = DestroyWindow(hwnd);
        }
    }

    unsafe fn show_menu(hwnd: HWND) {
        unsafe {
            let Ok(menu) = CreatePopupMenu() else {
                return;
            };
            let show_label = if WINDOW_VISIBLE.load(Ordering::Relaxed) {
                w!("Hide window")
            } else {
                w!("Show window")
            };
            let _ = AppendMenuW(menu, MF_STRING, TRAY_ID_SHOW as usize, show_label);
            let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
            let _ = AppendMenuW(menu, MF_STRING, TRAY_ID_START as usize, w!("Start / stop"));
            let _ = AppendMenuW(menu, MF_STRING, TRAY_ID_STOP as usize, w!("Stop"));
            let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
            let _ = AppendMenuW(menu, MF_STRING, TRAY_ID_EXIT as usize, w!("Exit"));

            let mut pt = POINT::default();
            let _ = GetCursorPos(&mut pt);
            // Required so the menu closes when the user clicks elsewhere.
            let _ = SetForegroundWindow(hwnd);
            let cmd = TrackPopupMenu(
                menu,
                TPM_RIGHTBUTTON | TPM_RETURNCMD | TPM_NONOTIFY,
                pt.x,
                pt.y,
                Some(0),
                hwnd,
                None,
            );
            let _ = DestroyMenu(menu);

            let state = GLOBAL_STATE.get();
            match cmd.0 as u32 {
                TRAY_ID_SHOW => toggle_main_window(),
                TRAY_ID_START => {
                    if let Some(s) = state {
                        toggle_clicking(s);
                    }
                }
                TRAY_ID_STOP => {
                    if let Some(s) = state {
                        stop_clicking(s);
                    }
                }
                TRAY_ID_EXIT => quit_application(),
                _ => {}
            }
        }
    }

    unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
        unsafe {
            if msg == WM_APP_TRAY {
                match lp.0 as u32 {
                    0x0202 => toggle_main_window(), // WM_LBUTTONUP
                    0x0205 | 0x007B => show_menu(hwnd),
                    _ => {}
                }
                return LRESULT(0);
            }
            DefWindowProcW(hwnd, msg, wp, lp)
        }
    }
}

#[cfg(not(windows))]
mod tray {
    pub fn init() {}
    pub fn shutdown() {}
    pub fn is_active() -> bool {
        false
    }
}

// ============================================================================
// Hotkey thread
// ============================================================================

#[cfg(windows)]
unsafe fn register_hotkeys() {
    use win32::*;
    let hk = *PENDING_HOTKEYS.lock();
    let ids = [HK_ID_TOGGLE, HK_ID_STOP];
    let mut failed = 0u32;
    unsafe {
        for (idx, id) in ids.into_iter().enumerate() {
            let _ = UnregisterHotKey(None, id);
            let key = hk[idx];
            if key.vk == 0 {
                continue;
            }
            let mut mods = MOD_NOREPEAT;
            if key.ctrl {
                mods |= MOD_CONTROL;
            }
            if key.alt {
                mods |= MOD_ALT;
            }
            if key.shift {
                mods |= MOD_SHIFT;
            }
            if RegisterHotKey(None, id, mods, key.vk).is_err() {
                failed |= 1 << idx;
                warn!("RegisterHotKey failed for {}", key.label());
            }
        }
    }
    HK_FAILED.store(failed, Ordering::Relaxed);
}

#[cfg(windows)]
fn hotkey_thread(state: Arc<AppState>, with_tray: bool) {
    use win32::*;
    let _ = GLOBAL_STATE.set(state.clone());
    HOOK_THREAD_ID.store(unsafe { GetCurrentThreadId() }, Ordering::Relaxed);

    unsafe {
        register_hotkeys();
        if with_tray {
            tray::init();
        }

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            match msg.message {
                WM_HOTKEY_ID => match msg.wParam.0 as i32 {
                    // In hold mode the engine watches the key itself, so the hotkey
                    // must not toggle anything.
                    HK_ID_TOGGLE => {
                        if !state.hold_mode.load(Ordering::Relaxed) {
                            toggle_clicking(&state);
                        }
                    }
                    HK_ID_STOP => stop_clicking(&state),
                    _ => {}
                },
                WM_APP_REHOTKEY => register_hotkeys(),
                WM_APP_HK_OFF => {
                    for id in [HK_ID_TOGGLE, HK_ID_STOP] {
                        let _ = UnregisterHotKey(None, id);
                    }
                    HK_FAILED.store(0, Ordering::Relaxed);
                }
                _ => {
                    let _ = TranslateMessage(&msg);
                    let _ = DispatchMessageW(&msg);
                }
            }
        }

        tray::shutdown();
        for id in [HK_ID_TOGGLE, HK_ID_STOP] {
            let _ = UnregisterHotKey(None, id);
        }
    }
    info!("hotkey thread exited");
}

// ============================================================================
// Localization
// ============================================================================

#[derive(Clone, Copy, PartialEq, Eq)]
enum Lang {
    En,
    Ru,
    Uk,
    Pt,
    Es,
    Zh,
}

macro_rules! define_strings {
    ($($field:ident),* $(,)?) => {
        #[derive(Clone, Copy)]
        pub struct Strings { $(pub $field: &'static str),* }

        impl Strings {
            fn with_overrides(mut self, map: &BTreeMap<String, String>) -> Self {
                $(
                    if let Some(v) = map.get(stringify!($field)) {
                        if !v.is_empty() {
                            // Leaked on purpose: language tables live for the process.
                            self.$field = Box::leak(v.clone().into_boxed_str());
                        }
                    }
                )*
                self
            }
            fn to_map(&self) -> BTreeMap<&'static str, &'static str> {
                let mut m = BTreeMap::new();
                $( m.insert(stringify!($field), self.$field); )*
                m
            }
        }
    };
}

define_strings!(
    sec_interval, sec_options, sec_repeat, sec_position, sec_limits, sec_pixel,
    sec_hotkeys, sec_appearance, sec_profiles,
    lbl_hours, lbl_mins, lbl_secs, lbl_ms, random_offset, tip_random, warn_fast,
    action_kind, kind_mouse, kind_key, mouse_button, click_type,
    btn_left, btn_right, btn_middle, btn_x1, btn_x2,
    type_single, type_double, type_triple, hold_ms, tip_hold, key_to_press,
    repeat_times_r, repeat_until, times,
    pos_current, pos_fixed, pos_points, pick_location, picking, add_point, del_point,
    jitter_px, tip_jitter, return_cursor,
    limit_cb, limit_h, limit_m, limit_s,
    pixel_cb, pixel_pick, pixel_tol, pixel_match, pixel_differ,
    hk_toggle, hk_stop, hk_bind, hk_press, hk_clear, hk_failed, hold_mode, tip_hold_mode,
    theme, language, lang_auto, transparent_ui, on_top, tray_cb, close_tray_cb, lang_template,
    prof_name, prof_save, prof_delete,
    btn_start, btn_stop, save_settings, reset_all,
    status_ready, status_running, status_hold, status_pixel,
    stat_clicks, stat_cps, stat_elapsed, data_dir,
    saved, loaded, settings_saved, done, save_err,
);

const EN: Strings = Strings {
    sec_interval: "⏱ Click interval", sec_options: "🖱 Click options",
    sec_repeat: "🔁 Click repeat", sec_position: "🎯 Cursor position",
    sec_limits: "⌛ Time limit", sec_pixel: "🌡 Pixel condition",
    sec_hotkeys: "⌨ Hotkeys", sec_appearance: "🎨 Appearance", sec_profiles: "📋 Profiles",
    lbl_hours: "hours", lbl_mins: "mins", lbl_secs: "secs", lbl_ms: "milliseconds",
    random_offset: "Random offset ±", tip_random: "Varies each interval by up to this much, in both directions.",
    warn_fast: "⚠ Very fast — keep the stop key in reach",
    action_kind: "Action:", kind_mouse: "Mouse", kind_key: "Keyboard",
    mouse_button: "Mouse button:", click_type: "Click type:",
    btn_left: "Left", btn_right: "Right", btn_middle: "Middle", btn_x1: "X1", btn_x2: "X2",
    type_single: "Single", type_double: "Double", type_triple: "Triple",
    hold_ms: "Hold (ms)", tip_hold: "How long the button stays down. 0 is an instant tap.",
    key_to_press: "Key:",
    repeat_times_r: "Repeat", repeat_until: "Repeat until stopped", times: "times",
    pos_current: "Current location", pos_fixed: "Fixed point", pos_points: "Point list",
    pick_location: "Pick location", picking: "Move the cursor… {} s",
    add_point: "Add", del_point: "Remove",
    jitter_px: "Spread (px)", tip_jitter: "Random offset around the target point.",
    return_cursor: "Put the cursor back afterwards",
    limit_cb: "Stop after", limit_h: "H", limit_m: "M", limit_s: "S",
    pixel_cb: "Stop on a screen pixel", pixel_pick: "🎯 Pick in 3 s", pixel_tol: "Tolerance",
    pixel_match: "when it matches", pixel_differ: "when it differs",
    hk_toggle: "Start / stop:", hk_stop: "Emergency stop:", hk_bind: "Click, then press a key",
    hk_press: "press a key… (Esc cancels)", hk_clear: "Clear",
    hk_failed: "⚠ Some hotkeys are taken by another app",
    hold_mode: "Click only while the key is held",
    tip_hold_mode: "Turns the start key into a trigger: clicking happens while you hold it.",
    theme: "Theme:", language: "Language:", lang_auto: "Auto (system)",
    transparent_ui: "🌓 Transparent UI", on_top: "📌 Always on Top",
    tray_cb: "Tray icon", close_tray_cb: "Close button minimizes to tray",
    lang_template: "🌍 Export language template",
    prof_name: "Name:", prof_save: "Save", prof_delete: "Delete",
    btn_start: "▶ Start", btn_stop: "⏹ Stop", save_settings: "💾 Save settings",
    reset_all: "↺ Defaults",
    status_ready: "Ready", status_running: "Clicking…", status_hold: "Hold the key to click",
    status_pixel: "Stopped by the pixel condition",
    stat_clicks: "Clicks: {}", stat_cps: "{} / sec", stat_elapsed: "Elapsed: {}",
    data_dir: "📁 Data folder:",
    saved: "Saved: {}", loaded: "Loaded: {}", settings_saved: "Settings saved",
    done: "Done", save_err: "Error: {}",
};

const RU: Strings = Strings {
    sec_interval: "⏱ Интервал кликов", sec_options: "🖱 Параметры клика",
    sec_repeat: "🔁 Повторы", sec_position: "🎯 Позиция курсора",
    sec_limits: "⌛ Лимит времени", sec_pixel: "🌡 Условие по пикселю",
    sec_hotkeys: "⌨ Горячие клавиши", sec_appearance: "🎨 Оформление", sec_profiles: "📋 Профили",
    lbl_hours: "часов", lbl_mins: "минут", lbl_secs: "секунд", lbl_ms: "миллисекунд",
    random_offset: "Случайный разброс ±",
    tip_random: "Каждый интервал меняется в обе стороны не больше чем на это значение.",
    warn_fast: "⚠ Очень быстро — держите клавишу остановки под рукой",
    action_kind: "Действие:", kind_mouse: "Мышь", kind_key: "Клавиатура",
    mouse_button: "Кнопка мыши:", click_type: "Тип клика:",
    btn_left: "Левая", btn_right: "Правая", btn_middle: "Средняя", btn_x1: "X1", btn_x2: "X2",
    type_single: "Одиночный", type_double: "Двойной", type_triple: "Тройной",
    hold_ms: "Удержание (мс)",
    tip_hold: "Сколько кнопка остаётся нажатой. 0 — мгновенное нажатие.",
    key_to_press: "Клавиша:",
    repeat_times_r: "Повторить", repeat_until: "Повторять до остановки", times: "раз",
    pos_current: "Текущее положение", pos_fixed: "Фиксированная точка", pos_points: "Список точек",
    pick_location: "Указать точку", picking: "Наведите курсор… {} с",
    add_point: "Добавить", del_point: "Удалить",
    jitter_px: "Разброс (px)", tip_jitter: "Случайное смещение вокруг целевой точки.",
    return_cursor: "Возвращать курсор обратно",
    limit_cb: "Остановиться через", limit_h: "Ч", limit_m: "М", limit_s: "С",
    pixel_cb: "Останавливаться по пикселю экрана", pixel_pick: "🎯 Взять через 3 с",
    pixel_tol: "Допуск", pixel_match: "когда совпадает", pixel_differ: "когда отличается",
    hk_toggle: "Старт / стоп:", hk_stop: "Аварийный стоп:",
    hk_bind: "Нажмите, затем клавишу", hk_press: "нажмите клавишу… (Esc — отмена)",
    hk_clear: "Сбросить", hk_failed: "⚠ Часть клавиш занята другой программой",
    hold_mode: "Кликать только пока клавиша зажата",
    tip_hold_mode: "Клавиша старта становится триггером: клики идут, пока вы её держите.",
    theme: "Тема:", language: "Язык:", lang_auto: "Авто (система)",
    transparent_ui: "🌓 Прозрачный интерфейс", on_top: "📌 Поверх всех окон",
    tray_cb: "Значок в трее", close_tray_cb: "Крестик сворачивает в трей",
    lang_template: "🌍 Выгрузить шаблон перевода",
    prof_name: "Имя:", prof_save: "Сохранить", prof_delete: "Удалить",
    btn_start: "▶ Старт", btn_stop: "⏹ Стоп", save_settings: "💾 Сохранить настройки",
    reset_all: "↺ По умолчанию",
    status_ready: "Готов", status_running: "Кликаю…",
    status_hold: "Держите клавишу для кликов", status_pixel: "Остановлено по условию пикселя",
    stat_clicks: "Кликов: {}", stat_cps: "{} / сек", stat_elapsed: "Прошло: {}",
    data_dir: "📁 Папка данных:",
    saved: "Сохранено: {}", loaded: "Загружено: {}", settings_saved: "Настройки сохранены",
    done: "Готово", save_err: "Ошибка: {}",
};

const UK: Strings = Strings {
    sec_interval: "⏱ Інтервал кліків", sec_options: "🖱 Параметри кліку",
    sec_repeat: "🔁 Повтори", sec_position: "🎯 Позиція курсора",
    sec_limits: "⌛ Ліміт часу", sec_pixel: "🌡 Умова за пікселем",
    sec_hotkeys: "⌨ Гарячі клавіші", sec_appearance: "🎨 Оформлення", sec_profiles: "📋 Профілі",
    lbl_hours: "годин", lbl_mins: "хвилин", lbl_secs: "секунд", lbl_ms: "мілісекунд",
    random_offset: "Випадковий розкид ±",
    tip_random: "Кожен інтервал змінюється в обидва боки не більше ніж на це значення.",
    warn_fast: "⚠ Дуже швидко — тримайте клавішу зупинки під рукою",
    action_kind: "Дія:", kind_mouse: "Миша", kind_key: "Клавіатура",
    mouse_button: "Кнопка миші:", click_type: "Тип кліку:",
    btn_left: "Ліва", btn_right: "Права", btn_middle: "Середня", btn_x1: "X1", btn_x2: "X2",
    type_single: "Одиночний", type_double: "Подвійний", type_triple: "Потрійний",
    hold_ms: "Утримання (мс)",
    tip_hold: "Скільки кнопка лишається натиснутою. 0 — миттєве натискання.",
    key_to_press: "Клавіша:",
    repeat_times_r: "Повторити", repeat_until: "Повторювати до зупинки", times: "разів",
    pos_current: "Поточне положення", pos_fixed: "Фіксована точка", pos_points: "Список точок",
    pick_location: "Вказати точку", picking: "Наведіть курсор… {} с",
    add_point: "Додати", del_point: "Видалити",
    jitter_px: "Розкид (px)", tip_jitter: "Випадкове зміщення навколо цільової точки.",
    return_cursor: "Повертати курсор назад",
    limit_cb: "Зупинитися через", limit_h: "Г", limit_m: "Х", limit_s: "С",
    pixel_cb: "Зупинятися за пікселем екрана", pixel_pick: "🎯 Взяти через 3 с",
    pixel_tol: "Допуск", pixel_match: "коли збігається", pixel_differ: "коли відрізняється",
    hk_toggle: "Старт / стоп:", hk_stop: "Аварійний стоп:",
    hk_bind: "Натисніть, потім клавішу", hk_press: "натисніть клавішу… (Esc — скасувати)",
    hk_clear: "Скинути", hk_failed: "⚠ Частину клавіш зайнято іншою програмою",
    hold_mode: "Клікати лише поки клавішу затиснуто",
    tip_hold_mode: "Клавіша старту стає тригером: кліки йдуть, поки ви її тримаєте.",
    theme: "Тема:", language: "Мова:", lang_auto: "Авто (система)",
    transparent_ui: "🌓 Прозорий інтерфейс", on_top: "📌 Завжди поверх вікон",
    tray_cb: "Значок у треї", close_tray_cb: "Хрестик згортає у трей",
    lang_template: "🌍 Вивантажити шаблон перекладу",
    prof_name: "Ім'я:", prof_save: "Зберегти", prof_delete: "Видалити",
    btn_start: "▶ Старт", btn_stop: "⏹ Стоп", save_settings: "💾 Зберегти налаштування",
    reset_all: "↺ За замовчуванням",
    status_ready: "Готово", status_running: "Клікаю…",
    status_hold: "Тримайте клавішу для кліків", status_pixel: "Зупинено за умовою пікселя",
    stat_clicks: "Кліків: {}", stat_cps: "{} / сек", stat_elapsed: "Минуло: {}",
    data_dir: "📁 Тека даних:",
    saved: "Збережено: {}", loaded: "Завантажено: {}", settings_saved: "Налаштування збережено",
    done: "Готово", save_err: "Помилка: {}",
};

const PT: Strings = Strings {
    sec_interval: "⏱ Intervalo de clique", sec_options: "🖱 Opções de clique",
    sec_repeat: "🔁 Repetição", sec_position: "🎯 Posição do cursor",
    sec_limits: "⌛ Limite de tempo", sec_pixel: "🌡 Condição de pixel",
    sec_hotkeys: "⌨ Atalhos", sec_appearance: "🎨 Aparência", sec_profiles: "📋 Perfis",
    lbl_hours: "horas", lbl_mins: "min", lbl_secs: "seg", lbl_ms: "milissegundos",
    random_offset: "Variação aleatória ±",
    tip_random: "Cada intervalo varia nos dois sentidos até este valor.",
    warn_fast: "⚠ Muito rápido — deixe a tecla de parada à mão",
    action_kind: "Ação:", kind_mouse: "Mouse", kind_key: "Teclado",
    mouse_button: "Botão do mouse:", click_type: "Tipo de clique:",
    btn_left: "Esquerdo", btn_right: "Direito", btn_middle: "Meio", btn_x1: "X1", btn_x2: "X2",
    type_single: "Único", type_double: "Duplo", type_triple: "Triplo",
    hold_ms: "Pressionar (ms)",
    tip_hold: "Quanto tempo o botão fica pressionado. 0 é um toque instantâneo.",
    key_to_press: "Tecla:",
    repeat_times_r: "Repetir", repeat_until: "Repetir até parar", times: "vezes",
    pos_current: "Posição atual", pos_fixed: "Ponto fixo", pos_points: "Lista de pontos",
    pick_location: "Escolher ponto", picking: "Mova o cursor… {} s",
    add_point: "Adicionar", del_point: "Remover",
    jitter_px: "Dispersão (px)", tip_jitter: "Deslocamento aleatório em torno do ponto.",
    return_cursor: "Devolver o cursor no fim",
    limit_cb: "Parar depois de", limit_h: "H", limit_m: "M", limit_s: "S",
    pixel_cb: "Parar por um pixel da tela", pixel_pick: "🎯 Capturar em 3 s",
    pixel_tol: "Tolerância", pixel_match: "quando coincidir", pixel_differ: "quando diferir",
    hk_toggle: "Iniciar / parar:", hk_stop: "Parada de emergência:",
    hk_bind: "Clique e pressione uma tecla", hk_press: "pressione uma tecla… (Esc cancela)",
    hk_clear: "Limpar", hk_failed: "⚠ Alguns atalhos estão ocupados",
    hold_mode: "Clicar só enquanto a tecla estiver pressionada",
    tip_hold_mode: "A tecla de início vira gatilho: clica enquanto você a segura.",
    theme: "Tema:", language: "Idioma:", lang_auto: "Auto (sistema)",
    transparent_ui: "🌓 Interface transparente", on_top: "📌 Sempre no topo",
    tray_cb: "Ícone na bandeja", close_tray_cb: "Fechar minimiza para a bandeja",
    lang_template: "🌍 Exportar modelo de idioma",
    prof_name: "Nome:", prof_save: "Salvar", prof_delete: "Excluir",
    btn_start: "▶ Iniciar", btn_stop: "⏹ Parar", save_settings: "💾 Salvar configurações",
    reset_all: "↺ Padrões",
    status_ready: "Pronto", status_running: "Clicando…",
    status_hold: "Segure a tecla para clicar", status_pixel: "Parado pela condição de pixel",
    stat_clicks: "Cliques: {}", stat_cps: "{} / seg", stat_elapsed: "Decorrido: {}",
    data_dir: "📁 Pasta de dados:",
    saved: "Salvo: {}", loaded: "Carregado: {}", settings_saved: "Configurações salvas",
    done: "Pronto", save_err: "Erro: {}",
};

const ES: Strings = Strings {
    sec_interval: "⏱ Intervalo de clic", sec_options: "🖱 Opciones de clic",
    sec_repeat: "🔁 Repetición", sec_position: "🎯 Posición del cursor",
    sec_limits: "⌛ Límite de tiempo", sec_pixel: "🌡 Condición de píxel",
    sec_hotkeys: "⌨ Atajos", sec_appearance: "🎨 Apariencia", sec_profiles: "📋 Perfiles",
    lbl_hours: "horas", lbl_mins: "min", lbl_secs: "seg", lbl_ms: "milisegundos",
    random_offset: "Variación aleatoria ±",
    tip_random: "Cada intervalo varía en ambos sentidos hasta este valor.",
    warn_fast: "⚠ Muy rápido — ten la tecla de parada a mano",
    action_kind: "Acción:", kind_mouse: "Ratón", kind_key: "Teclado",
    mouse_button: "Botón del ratón:", click_type: "Tipo de clic:",
    btn_left: "Izquierdo", btn_right: "Derecho", btn_middle: "Central", btn_x1: "X1", btn_x2: "X2",
    type_single: "Simple", type_double: "Doble", type_triple: "Triple",
    hold_ms: "Mantener (ms)",
    tip_hold: "Cuánto permanece pulsado el botón. 0 es un toque instantáneo.",
    key_to_press: "Tecla:",
    repeat_times_r: "Repetir", repeat_until: "Repetir hasta detener", times: "veces",
    pos_current: "Posición actual", pos_fixed: "Punto fijo", pos_points: "Lista de puntos",
    pick_location: "Elegir punto", picking: "Mueve el cursor… {} s",
    add_point: "Añadir", del_point: "Quitar",
    jitter_px: "Dispersión (px)", tip_jitter: "Desplazamiento aleatorio alrededor del punto.",
    return_cursor: "Devolver el cursor al final",
    limit_cb: "Detener tras", limit_h: "H", limit_m: "M", limit_s: "S",
    pixel_cb: "Detener por un píxel de pantalla", pixel_pick: "🎯 Capturar en 3 s",
    pixel_tol: "Tolerancia", pixel_match: "cuando coincida", pixel_differ: "cuando difiera",
    hk_toggle: "Iniciar / detener:", hk_stop: "Parada de emergencia:",
    hk_bind: "Pulsa y luego una tecla", hk_press: "pulsa una tecla… (Esc cancela)",
    hk_clear: "Borrar", hk_failed: "⚠ Algunos atajos están ocupados",
    hold_mode: "Clicar sólo mientras la tecla esté pulsada",
    tip_hold_mode: "La tecla de inicio actúa como gatillo: clica mientras la mantienes.",
    theme: "Tema:", language: "Idioma:", lang_auto: "Auto (sistema)",
    transparent_ui: "🌓 Interfaz transparente", on_top: "📌 Siempre encima",
    tray_cb: "Icono en la bandeja", close_tray_cb: "Cerrar minimiza a la bandeja",
    lang_template: "🌍 Exportar plantilla de idioma",
    prof_name: "Nombre:", prof_save: "Guardar", prof_delete: "Eliminar",
    btn_start: "▶ Iniciar", btn_stop: "⏹ Detener", save_settings: "💾 Guardar ajustes",
    reset_all: "↺ Por defecto",
    status_ready: "Listo", status_running: "Clicando…",
    status_hold: "Mantén la tecla para clicar", status_pixel: "Detenido por la condición de píxel",
    stat_clicks: "Clics: {}", stat_cps: "{} / seg", stat_elapsed: "Transcurrido: {}",
    data_dir: "📁 Carpeta de datos:",
    saved: "Guardado: {}", loaded: "Cargado: {}", settings_saved: "Ajustes guardados",
    done: "Listo", save_err: "Error: {}",
};

const ZH: Strings = Strings {
    sec_interval: "⏱ 点击间隔", sec_options: "🖱 点击选项",
    sec_repeat: "🔁 重复次数", sec_position: "🎯 光标位置",
    sec_limits: "⌛ 时间限制", sec_pixel: "🌡 像素条件",
    sec_hotkeys: "⌨ 快捷键", sec_appearance: "🎨 外观", sec_profiles: "📋 配置",
    lbl_hours: "小时", lbl_mins: "分", lbl_secs: "秒", lbl_ms: "毫秒",
    random_offset: "随机浮动 ±", tip_random: "每次间隔在两个方向上最多浮动这么多。",
    warn_fast: "⚠ 非常快 — 请把停止键放在手边",
    action_kind: "动作:", kind_mouse: "鼠标", kind_key: "键盘",
    mouse_button: "鼠标按键:", click_type: "点击类型:",
    btn_left: "左键", btn_right: "右键", btn_middle: "中键", btn_x1: "X1", btn_x2: "X2",
    type_single: "单击", type_double: "双击", type_triple: "三击",
    hold_ms: "按住 (毫秒)", tip_hold: "按键保持按下的时长。0 表示瞬间点击。",
    key_to_press: "按键:",
    repeat_times_r: "重复", repeat_until: "一直重复直到停止", times: "次",
    pos_current: "当前位置", pos_fixed: "固定点", pos_points: "点列表",
    pick_location: "选取位置", picking: "移动光标… {} 秒",
    add_point: "添加", del_point: "删除",
    jitter_px: "抖动 (px)", tip_jitter: "在目标点周围随机偏移。",
    return_cursor: "结束后把光标放回去",
    limit_cb: "多久后停止", limit_h: "时", limit_m: "分", limit_s: "秒",
    pixel_cb: "按屏幕像素停止", pixel_pick: "🎯 3 秒后取色", pixel_tol: "容差",
    pixel_match: "当匹配时", pixel_differ: "当不匹配时",
    hk_toggle: "开始 / 停止:", hk_stop: "紧急停止:",
    hk_bind: "点击后按一个键", hk_press: "请按一个键…（Esc 取消）",
    hk_clear: "清除", hk_failed: "⚠ 部分快捷键被其他程序占用",
    hold_mode: "仅在按住按键时点击",
    tip_hold_mode: "开始键变成扳机：按住时才会点击。",
    theme: "主题:", language: "语言:", lang_auto: "自动 (系统)",
    transparent_ui: "🌓 透明界面", on_top: "📌 始终置顶",
    tray_cb: "托盘图标", close_tray_cb: "关闭按钮最小化到托盘",
    lang_template: "🌍 导出语言模板",
    prof_name: "名称:", prof_save: "保存", prof_delete: "删除",
    btn_start: "▶ 开始", btn_stop: "⏹ 停止", save_settings: "💾 保存设置",
    reset_all: "↺ 默认值",
    status_ready: "就绪", status_running: "点击中…",
    status_hold: "按住按键即可点击", status_pixel: "已按像素条件停止",
    stat_clicks: "点击: {}", stat_cps: "{} / 秒", stat_elapsed: "已用: {}",
    data_dir: "📁 数据目录:",
    saved: "已保存: {}", loaded: "已加载: {}", settings_saved: "设置已保存",
    done: "完成", save_err: "错误: {}",
};

const LANG_CODES: [&str; 6] = ["en", "ru", "uk", "pt", "es", "zh"];

/// Built-in tables, with `<data>/lang/<code>.json` applied on top when present.
fn tables() -> &'static [&'static Strings; 6] {
    static ACTIVE: OnceLock<[&'static Strings; 6]> = OnceLock::new();
    ACTIVE.get_or_init(|| {
        let base: [&'static Strings; 6] = [&EN, &RU, &UK, &PT, &ES, &ZH];
        let mut out = base;
        for i in 0..6 {
            let path = paths::lang_dir().join(format!("{}.json", LANG_CODES[i]));
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            match serde_json::from_str::<BTreeMap<String, String>>(&text) {
                Ok(map) => {
                    out[i] = Box::leak(Box::new(base[i].with_overrides(&map)));
                    info!("loaded translation overrides from {}", path.display());
                }
                Err(e) => warn!("bad translation file {}: {e}", path.display()),
            }
        }
        out
    })
}

fn export_lang_template(lang_index: usize) -> Result<PathBuf> {
    let idx = lang_index.min(5);
    let map = tables()[idx].to_map();
    let path = paths::lang_dir().join(format!("{}.template.json", LANG_CODES[idx]));
    std::fs::write(&path, serde_json::to_string_pretty(&map)?)?;
    Ok(path)
}

fn detect_system_lang() -> Lang {
    #[cfg(windows)]
    unsafe {
        let lang = win32::GetUserDefaultUILanguage() as u32;
        match lang & 0x3FF {
            0x19 => Lang::Ru,
            0x22 => Lang::Uk,
            0x16 => Lang::Pt,
            0x0A => Lang::Es,
            0x04 => Lang::Zh,
            _ => Lang::En,
        }
    }
    #[cfg(not(windows))]
    Lang::En
}

fn get_strings(lang_mode: usize, system_lang: Lang) -> &'static Strings {
    let lang = match lang_mode {
        1 => Lang::En,
        2 => Lang::Ru,
        3 => Lang::Uk,
        4 => Lang::Pt,
        5 => Lang::Es,
        6 => Lang::Zh,
        _ => system_lang,
    };
    let idx = match lang {
        Lang::En => 0,
        Lang::Ru => 1,
        Lang::Uk => 2,
        Lang::Pt => 3,
        Lang::Es => 4,
        Lang::Zh => 5,
    };
    tables()[idx]
}

// ============================================================================
// Themes
// ============================================================================

#[derive(Clone, Copy, PartialEq, Eq)]
enum Theme {
    Dark,
    Oled,
    Material3,
    Catppuccin,
    Nord,
    Dracula,
    Glass,
    Neumorphism,
    Fluent,
}

const THEMES: [Theme; 9] = [
    Theme::Dark,
    Theme::Oled,
    Theme::Material3,
    Theme::Catppuccin,
    Theme::Nord,
    Theme::Dracula,
    Theme::Glass,
    Theme::Neumorphism,
    Theme::Fluent,
];

const THEME_NAMES: [&str; 9] = [
    "Dark (default)",
    "OLED (Pure Black)",
    "Material Design 3",
    "Catppuccin Mocha",
    "Nord",
    "Dracula",
    "Glassmorphism (Acrylic)",
    "Neumorphism",
    "Fluent (Mica)",
];

fn theme_at(index: usize) -> Theme {
    THEMES.get(index).copied().unwrap_or(Theme::Dark)
}

struct Palette {
    dark: bool,
    bg: egui::Color32,
    panel: egui::Color32,
    widget: egui::Color32,
    widget_hover: egui::Color32,
    widget_active: egui::Color32,
    active_fg: egui::Color32,
    border: egui::Color32,
    hover_border: egui::Color32,
    text: egui::Color32,
    faint: egui::Color32,
    accent: egui::Color32,
    focus_border: egui::Color32,
    widget_round: f32,
    shadow_blur: u8,
    shadow_offset: i8,
    shadow_alpha: u8,
    item_spacing_y: f32,
    button_padding: f32,
    animation_time: f32,
    /// DWMWA_SYSTEMBACKDROP_TYPE: 1 = none, 2 = Mica, 3 = Acrylic.
    backdrop: i32,
}

fn rgb(r: u8, g: u8, b: u8) -> egui::Color32 {
    egui::Color32::from_rgb(r, g, b)
}
fn rgba(r: u8, g: u8, b: u8, a: u8) -> egui::Color32 {
    egui::Color32::from_rgba_unmultiplied(r, g, b, a)
}

#[cfg(windows)]
fn get_system_accent_color() -> Option<egui::Color32> {
    use win32::*;
    unsafe {
        let mut key = HKEY::default();
        let path = windows::core::w!("Software\\Microsoft\\Windows\\DWM");
        if RegOpenKeyExW(HKEY_CURRENT_USER, path, Some(0), KEY_READ, &mut key).is_err() {
            return None;
        }
        let mut data: u32 = 0;
        let mut size: u32 = std::mem::size_of::<u32>() as u32;
        let res = RegQueryValueExW(
            key,
            windows::core::w!("AccentColor"),
            None,
            None,
            Some(&mut data as *mut u32 as *mut u8),
            Some(&mut size),
        );
        let _ = RegCloseKey(key);
        if res.is_ok() {
            // Stored as ABGR.
            return Some(egui::Color32::from_rgb(
                (data & 0xFF) as u8,
                ((data >> 8) & 0xFF) as u8,
                ((data >> 16) & 0xFF) as u8,
            ));
        }
        None
    }
}

#[cfg(not(windows))]
fn get_system_accent_color() -> Option<egui::Color32> {
    None
}

fn get_palette(theme: Theme) -> Palette {
    match theme {
        Theme::Dark => Palette {
            dark: true, bg: rgb(16, 16, 16), panel: rgb(24, 24, 24), widget: rgb(42, 42, 42),
            widget_hover: rgb(58, 58, 58), widget_active: rgb(75, 75, 75),
            active_fg: rgb(255, 255, 255), border: rgb(70, 70, 70), hover_border: rgb(95, 95, 95),
            text: rgb(230, 230, 230), faint: rgb(130, 130, 130), accent: rgb(70, 200, 140),
            focus_border: rgb(0, 220, 160), widget_round: 4.0, shadow_blur: 4, shadow_offset: 1,
            shadow_alpha: 60, item_spacing_y: 5.0, button_padding: 3.0, animation_time: 0.15,
            backdrop: 1,
        },
        Theme::Oled => Palette {
            dark: true, bg: rgb(0, 0, 0), panel: rgb(0, 0, 0), widget: rgb(20, 20, 20),
            widget_hover: rgb(35, 35, 35), widget_active: rgb(50, 50, 50),
            active_fg: rgb(255, 255, 255), border: rgb(40, 40, 40), hover_border: rgb(80, 80, 80),
            text: rgb(240, 240, 240), faint: rgb(120, 120, 120), accent: rgb(0, 200, 140),
            focus_border: rgb(0, 255, 200), widget_round: 2.0, shadow_blur: 0, shadow_offset: 0,
            shadow_alpha: 0, item_spacing_y: 5.0, button_padding: 3.0, animation_time: 0.1,
            backdrop: 1,
        },
        Theme::Material3 => {
            let accent = get_system_accent_color().unwrap_or_else(|| rgb(160, 220, 180));
            Palette {
                dark: true, bg: rgb(17, 22, 19), panel: rgb(23, 29, 26), widget: rgb(31, 39, 35),
                widget_hover: rgb(39, 49, 44), widget_active: accent,
                active_fg: rgb(255, 255, 255), border: rgb(69, 82, 75), hover_border: accent,
                text: rgb(224, 233, 227), faint: rgb(143, 153, 147), accent,
                focus_border: rgb(255, 255, 0), widget_round: 20.0, shadow_blur: 0,
                shadow_offset: 0, shadow_alpha: 0, item_spacing_y: 7.0, button_padding: 6.0,
                animation_time: 0.4, backdrop: 1,
            }
        }
        Theme::Catppuccin => Palette {
            dark: true, bg: rgb(17, 17, 27), panel: rgb(30, 30, 46), widget: rgb(49, 50, 68),
            widget_hover: rgb(69, 71, 90), widget_active: rgb(166, 227, 161),
            active_fg: rgb(17, 17, 27), border: rgb(88, 91, 112), hover_border: rgb(166, 227, 161),
            text: rgb(205, 214, 244), faint: rgb(166, 172, 200), accent: rgb(166, 227, 161),
            focus_border: rgb(250, 178, 102), widget_round: 10.0, shadow_blur: 6, shadow_offset: 2,
            shadow_alpha: 90, item_spacing_y: 5.0, button_padding: 4.0, animation_time: 0.25,
            backdrop: 1,
        },
        Theme::Nord => Palette {
            dark: true, bg: rgb(46, 52, 64), panel: rgb(46, 52, 64), widget: rgb(59, 66, 82),
            widget_hover: rgb(67, 76, 94), widget_active: rgb(163, 190, 140),
            active_fg: rgb(46, 52, 64), border: rgb(76, 86, 106), hover_border: rgb(163, 190, 140),
            text: rgb(216, 222, 233), faint: rgb(148, 155, 168), accent: rgb(163, 190, 140),
            focus_border: rgb(143, 188, 187), widget_round: 6.0, shadow_blur: 5, shadow_offset: 1,
            shadow_alpha: 80, item_spacing_y: 5.0, button_padding: 4.0, animation_time: 0.2,
            backdrop: 1,
        },
        Theme::Dracula => Palette {
            dark: true, bg: rgb(40, 42, 54), panel: rgb(40, 42, 54), widget: rgb(68, 71, 90),
            widget_hover: rgb(80, 83, 105), widget_active: rgb(80, 250, 123),
            active_fg: rgb(40, 42, 54), border: rgb(98, 114, 164), hover_border: rgb(80, 250, 123),
            text: rgb(248, 248, 242), faint: rgb(135, 140, 160), accent: rgb(80, 250, 123),
            focus_border: rgb(189, 147, 249), widget_round: 8.0, shadow_blur: 6, shadow_offset: 2,
            shadow_alpha: 90, item_spacing_y: 5.0, button_padding: 4.0, animation_time: 0.25,
            backdrop: 1,
        },
        Theme::Glass => Palette {
            dark: true, bg: rgb(22, 34, 30), panel: rgba(38, 58, 50, 110),
            widget: rgba(255, 255, 255, 45), widget_hover: rgba(255, 255, 255, 75),
            widget_active: rgba(120, 235, 180, 200), active_fg: rgb(255, 255, 255),
            border: rgba(255, 255, 255, 110), hover_border: rgba(255, 255, 255, 170),
            text: rgb(240, 255, 248), faint: rgb(190, 215, 205), accent: rgb(120, 235, 180),
            focus_border: rgb(255, 255, 255), widget_round: 14.0, shadow_blur: 12,
            shadow_offset: 3, shadow_alpha: 100, item_spacing_y: 5.0, button_padding: 4.0,
            animation_time: 0.3, backdrop: 3,
        },
        Theme::Neumorphism => Palette {
            dark: false, bg: rgb(226, 236, 231), panel: rgb(226, 236, 231),
            widget: rgb(226, 236, 231), widget_hover: rgb(233, 243, 238),
            widget_active: rgb(60, 170, 120), active_fg: rgb(255, 255, 255),
            border: rgb(226, 236, 231), hover_border: rgb(226, 236, 231), text: rgb(58, 78, 68),
            faint: rgb(120, 140, 130), accent: rgb(60, 170, 120), focus_border: rgb(255, 120, 100),
            widget_round: 12.0, shadow_blur: 10, shadow_offset: 5, shadow_alpha: 110,
            item_spacing_y: 6.0, button_padding: 5.0, animation_time: 0.25, backdrop: 1,
        },
        Theme::Fluent => {
            let accent = get_system_accent_color().unwrap_or_else(|| rgb(76, 210, 150));
            Palette {
                dark: true, bg: rgb(32, 32, 32), panel: rgba(43, 43, 43, 150),
                widget: rgba(255, 255, 255, 22), widget_hover: rgba(255, 255, 255, 38),
                widget_active: accent, active_fg: rgb(255, 255, 255),
                border: rgba(255, 255, 255, 40), hover_border: accent, text: rgb(240, 240, 240),
                faint: rgb(165, 165, 165), accent, focus_border: accent, widget_round: 7.0,
                shadow_blur: 0, shadow_offset: 0, shadow_alpha: 0, item_spacing_y: 6.0,
                button_padding: 5.0, animation_time: 0.2, backdrop: 2,
            }
        }
    }
}

fn make_shadow(p: &Palette) -> egui::Shadow {
    egui::Shadow {
        offset: [p.shadow_offset, p.shadow_offset],
        blur: p.shadow_blur,
        spread: 0,
        color: egui::Color32::from_black_alpha(p.shadow_alpha),
    }
}

/// Applies a theme and returns the fill the central panel should use.
///
/// The window's translucency and the background of popups have to come from two
/// different places: egui paints combo-box lists and menus with `panel_fill`, and
/// those float above the app's own content, where anything see-through is unreadable.
#[must_use]
fn apply_theme(ctx: &egui::Context, theme: Theme, transparent_ui: bool) -> egui::Color32 {
    let p = get_palette(theme);
    let mut visuals = if p.dark { egui::Visuals::dark() } else { egui::Visuals::light() };

    visuals.window_fill = p.panel;
    visuals.panel_fill = p.panel;
    visuals.extreme_bg_color = p.bg;
    visuals.window_shadow = make_shadow(&p);
    visuals.popup_shadow = make_shadow(&p);
    visuals.hyperlink_color = p.accent;
    visuals.selection.bg_fill = p.accent;
    visuals.selection.stroke = egui::Stroke::new(2.0, p.focus_border);

    let states = [
        &mut visuals.widgets.noninteractive,
        &mut visuals.widgets.inactive,
        &mut visuals.widgets.hovered,
        &mut visuals.widgets.active,
    ];
    for w in states {
        w.corner_radius = p.widget_round.into();
        w.bg_stroke = egui::Stroke::new(1.0, p.border);
        w.fg_stroke = egui::Stroke::new(1.0, p.text);
    }
    visuals.widgets.noninteractive.bg_fill = p.panel;
    visuals.widgets.noninteractive.fg_stroke = egui::Stroke::new(1.0, p.faint);
    visuals.widgets.inactive.bg_fill = p.widget;
    visuals.widgets.hovered.bg_fill = p.widget_hover;
    visuals.widgets.hovered.bg_stroke = egui::Stroke::new(1.5, p.hover_border);
    visuals.widgets.active.bg_fill = p.widget_active;
    visuals.widgets.active.fg_stroke = egui::Stroke::new(1.0, p.active_fg);
    visuals.widgets.active.bg_stroke = egui::Stroke::new(2.0, p.focus_border);

    let translucent = transparent_ui || p.backdrop > 1;
    let panel_fill = if translucent {
        if p.backdrop > 1 { p.panel } else { rgba(30, 30, 30, 140) }
    } else {
        p.panel
    };

    if translucent {
        // Every floating surface is forced opaque and gets a border plus a shadow.
        visuals.panel_fill = p.bg;
        visuals.window_fill = p.bg;
        visuals.extreme_bg_color = p.bg;
        visuals.window_stroke = egui::Stroke::new(1.0, p.hover_border);
        visuals.popup_shadow = egui::Shadow {
            offset: [0, 4],
            blur: 14,
            spread: 0,
            color: egui::Color32::from_black_alpha(170),
        };
    }

    let mut style = (*ctx.style_of(egui::Theme::Dark)).clone();
    style.visuals = visuals;
    style.animation_time = p.animation_time;
    style.spacing.item_spacing = egui::vec2(8.0, p.item_spacing_y);
    style.spacing.button_padding = egui::vec2(p.button_padding, p.button_padding);

    ctx.set_style_of(egui::Theme::Dark, style.clone());
    ctx.set_style_of(egui::Theme::Light, style);

    // Pushed unconditionally (1 = none) so leaving a backdrop theme clears the effect.
    #[cfg(windows)]
    platform::apply_system_backdrop(platform::app_hwnd(), p.backdrop);

    panel_fill
}

// ============================================================================
// Application
// ============================================================================

struct ClickerApp {
    state: Arc<AppState>,
    config: AppConfig,
    system_lang: Lang,
    status_msg: String,
    theme_dirty: bool,
    panel_fill: egui::Color32,

    profiles: Vec<String>,
    profile_name: String,

    /// Deferred captures: (deadline, what we are picking).
    pick_point_deadline: Option<Instant>,
    pick_pixel_deadline: Option<Instant>,
    capture_started: Option<Instant>,

    // CPS meter
    cps: f32,
    cps_last: (u64, Instant),
}

impl ClickerApp {
    fn new(cc: &eframe::CreationContext<'_>, state: Arc<AppState>, config: AppConfig) -> Self {
        setup_fonts(&cc.egui_ctx);
        let panel_fill =
            apply_theme(&cc.egui_ctx, theme_at(config.default_theme), config.transparent_ui);
        Self {
            panel_fill,
            state,
            config,
            system_lang: detect_system_lang(),
            status_msg: String::new(),
            theme_dirty: true,
            profiles: list_profiles(),
            profile_name: String::new(),
            pick_point_deadline: None,
            pick_pixel_deadline: None,
            capture_started: None,
            cps: 0.0,
            cps_last: (0, Instant::now()),
        }
    }

    fn strs(&self) -> &'static Strings {
        get_strings(self.config.default_lang, self.system_lang)
    }
}

fn setup_fonts(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    let candidates = [
        "C:\\Windows\\Fonts\\msyh.ttc",
        "C:\\Windows\\Fonts\\simhei.ttf",
        "C:\\Windows\\Fonts\\meiryo.ttc",
    ];
    for path in candidates {
        if let Ok(data) = std::fs::read(path) {
            fonts.font_data.insert("cjk".into(), egui::FontData::from_owned(data).into());
            if let Some(f) = fonts.families.get_mut(&egui::FontFamily::Proportional) {
                f.push("cjk".into());
            }
            if let Some(f) = fonts.families.get_mut(&egui::FontFamily::Monospace) {
                f.push("cjk".into());
            }
            break;
        }
    }
    ctx.set_fonts(fonts);
}

/// One row of the hotkey editor.
///
/// Two ways to set a key, because one of them always works: click the button and
/// press anything, or pick from the list.
fn hotkey_row(
    ui: &mut egui::Ui,
    s: &Strings,
    label: &str,
    salt: &str,
    slot: u32,
    hk: &mut Hotkey,
) -> bool {
    let mut changed = false;
    ui.horizontal_wrapped(|ui| {
        ui.label(label);
        let capturing = CAPTURE_SLOT.load(Ordering::Relaxed) == slot;
        let text = if capturing { s.hk_press.to_string() } else { hk.label() };
        let button = egui::Button::new(text).min_size(egui::vec2(140.0, 0.0));
        if ui.add(button).on_hover_text(s.hk_bind).clicked() {
            if capturing {
                end_capture();
            } else {
                begin_capture(slot);
            }
        }
        egui::ComboBox::from_id_salt(salt)
            .selected_text("▾")
            .width(46.0)
            .show_ui(ui, |ui| {
                for (name, vk) in HOTKEY_CHOICES {
                    if ui.selectable_label(hk.vk == vk, name).clicked() && hk.vk != vk {
                        hk.vk = vk;
                        changed = true;
                    }
                }
            });
        changed |= ui.checkbox(&mut hk.ctrl, "Ctrl").changed();
        changed |= ui.checkbox(&mut hk.alt, "Alt").changed();
        changed |= ui.checkbox(&mut hk.shift, "Shift").changed();
        if ui.small_button(s.hk_clear).clicked() && hk.vk != 0 {
            *hk = Hotkey::plain(0);
            changed = true;
        }
    });
    changed
}

/// Seconds left on a countdown, for the "pick in 3 s" labels.
fn seconds_left(deadline: Instant) -> u64 {
    deadline.saturating_duration_since(Instant::now()).as_secs() + 1
}

impl eframe::App for ClickerApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let s = self.strs();
        let running = self.state.running.load(Ordering::Relaxed);

        if self.theme_dirty {
            self.panel_fill = apply_theme(
                ui.ctx(),
                theme_at(self.config.default_theme),
                self.config.transparent_ui,
            );
            self.theme_dirty = false;
        }

        // ---- close button ------------------------------------------------------
        if ui.ctx().input(|i| i.viewport().close_requested()) {
            let to_tray = self.config.tray_enabled
                && self.config.close_to_tray
                && tray::is_active()
                && !ALLOW_CLOSE.load(Ordering::Relaxed);
            if to_tray {
                ui.ctx().send_viewport_cmd(egui::ViewportCommand::CancelClose);
                set_window_visible(false);
            }
        }

        // ---- hotkey binding ----------------------------------------------------
        let slot = CAPTURE_SLOT.load(Ordering::Relaxed);
        if slot != 0 {
            ui.ctx().request_repaint();
            match self.capture_started {
                // Never let a forgotten binding session leave the hotkeys switched off.
                Some(t) if t.elapsed() > Duration::from_secs(15) => {
                    self.capture_started = None;
                    end_capture();
                }
                None => self.capture_started = Some(Instant::now()),
                _ => {}
            }
            if let Some(hk) = scan_pressed_key() {
                if hk.vk != 0 {
                    match slot {
                        1 => self.config.hotkey_toggle = hk,
                        2 => self.config.hotkey_stop = hk,
                        _ => {}
                    }
                    self.status_msg = hk.label();
                }
                self.capture_started = None;
                end_capture();
                publish_hotkeys(&self.config);
            }
        } else {
            self.capture_started = None;
        }

        // ---- deferred pickers ---------------------------------------------------
        if let Some(deadline) = self.pick_point_deadline {
            ui.ctx().request_repaint();
            if Instant::now() >= deadline {
                self.pick_point_deadline = None;
                let (x, y) = platform::cursor_pos();
                if self.config.position_mode == 2 {
                    self.config.points.push((x, y));
                } else {
                    self.config.pos_x = x;
                    self.config.pos_y = y;
                }
                self.status_msg = format!("{x}, {y}");
            }
        }
        if let Some(deadline) = self.pick_pixel_deadline {
            ui.ctx().request_repaint();
            if Instant::now() >= deadline {
                self.pick_pixel_deadline = None;
                let (x, y) = platform::cursor_pos();
                self.config.pixel_x = x;
                self.config.pixel_y = y;
                if let Some((r, g, b)) = platform::screen_pixel(x, y) {
                    self.config.pixel_r = r;
                    self.config.pixel_g = g;
                    self.config.pixel_b = b;
                }
                self.status_msg = s.done.to_string();
            }
        }

        if self.state.stopped_by_pixel.swap(false, Ordering::Relaxed) {
            self.status_msg = s.status_pixel.to_string();
        }

        // ---- CPS meter ----------------------------------------------------------
        {
            let clicks = self.state.clicks.load(Ordering::Relaxed);
            let dt = self.cps_last.1.elapsed().as_secs_f32();
            if dt >= 0.5 {
                let delta = clicks.saturating_sub(self.cps_last.0) as f32;
                self.cps = if running { delta / dt } else { 0.0 };
                self.cps_last = (clicks, Instant::now());
            }
        }

        let panel = egui::Frame::central_panel(ui.style()).fill(self.panel_fill);
        egui::CentralPanel::default().frame(panel).show(ui, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.heading(APP_TITLE);
                    ui.label(egui::RichText::new(format!("v{APP_VERSION}")).weak());
                });
                ui.separator();

                // ---- click interval -------------------------------------------
                ui.group(|ui| {
                    ui.label(egui::RichText::new(s.sec_interval).strong());
                    ui.horizontal_wrapped(|ui| {
                        ui.add(egui::DragValue::new(&mut self.config.interval_h).range(0..=240));
                        ui.label(s.lbl_hours);
                        ui.add(egui::DragValue::new(&mut self.config.interval_m).range(0..=59));
                        ui.label(s.lbl_mins);
                        ui.add(egui::DragValue::new(&mut self.config.interval_s).range(0..=59));
                        ui.label(s.lbl_secs);
                        ui.add(egui::DragValue::new(&mut self.config.interval_ms).range(0..=999));
                        ui.label(s.lbl_ms);
                    });
                    ui.horizontal_wrapped(|ui| {
                        ui.checkbox(&mut self.config.random_offset_enabled, s.random_offset)
                            .on_hover_text(s.tip_random);
                        ui.add_enabled(
                            self.config.random_offset_enabled,
                            egui::DragValue::new(&mut self.config.random_offset_ms)
                                .range(0..=600_000),
                        );
                        ui.label(s.lbl_ms);
                    });
                    if self.config.interval_us() < FAST_WARN_US {
                        ui.colored_label(egui::Color32::from_rgb(255, 170, 60), s.warn_fast);
                    }
                });

                // ---- click options ---------------------------------------------
                ui.group(|ui| {
                    ui.label(egui::RichText::new(s.sec_options).strong());
                    ui.horizontal_wrapped(|ui| {
                        ui.label(s.action_kind);
                        ui.radio_value(&mut self.config.action_mode, 0, s.kind_mouse);
                        ui.radio_value(&mut self.config.action_mode, 1, s.kind_key);
                    });
                    if self.config.action_mode == 0 {
                        ui.horizontal(|ui| {
                            ui.label(s.mouse_button);
                            let names =
                                [s.btn_left, s.btn_right, s.btn_middle, s.btn_x1, s.btn_x2];
                            egui::ComboBox::from_id_salt("mb")
                                .selected_text(names[self.config.mouse_button.min(4)])
                                .show_ui(ui, |ui| {
                                    for (i, n) in names.iter().enumerate() {
                                        ui.selectable_value(
                                            &mut self.config.mouse_button,
                                            i,
                                            *n,
                                        );
                                    }
                                });
                        });
                    } else {
                        ui.horizontal_wrapped(|ui| {
                            ui.label(s.key_to_press);
                            ui.label(
                                egui::RichText::new(vk_name(self.config.key_vk)).strong(),
                            );
                            egui::ComboBox::from_id_salt("keypick")
                                .selected_text("▾")
                                .width(46.0)
                                .show_ui(ui, |ui| {
                                    // Letters, digits and the usual game keys.
                                    let mut list: Vec<u32> = (0x41..=0x5Au32).collect();
                                    list.extend(0x30..=0x39u32);
                                    list.extend([0x20, 0x0D, 0x09, 0x10, 0x11, 0x12]);
                                    list.extend(0x70..=0x7Bu32);
                                    for vk in list {
                                        if ui
                                            .selectable_label(
                                                self.config.key_vk == vk,
                                                vk_name(vk),
                                            )
                                            .clicked()
                                        {
                                            self.config.key_vk = vk;
                                        }
                                    }
                                });
                        });
                    }
                    ui.horizontal(|ui| {
                        ui.label(s.click_type);
                        let names = [s.type_single, s.type_double, s.type_triple];
                        egui::ComboBox::from_id_salt("ct")
                            .selected_text(names[self.config.click_type.min(2)])
                            .show_ui(ui, |ui| {
                                for (i, n) in names.iter().enumerate() {
                                    ui.selectable_value(&mut self.config.click_type, i, *n);
                                }
                            });
                    });
                    ui.horizontal(|ui| {
                        ui.label(s.hold_ms).on_hover_text(s.tip_hold);
                        ui.add(egui::DragValue::new(&mut self.config.hold_ms).range(0..=5_000));
                    });
                });

                // ---- repeat -----------------------------------------------------
                ui.group(|ui| {
                    ui.label(egui::RichText::new(s.sec_repeat).strong());
                    ui.horizontal_wrapped(|ui| {
                        if ui.radio(!self.config.repeat_infinite, s.repeat_times_r).clicked() {
                            self.config.repeat_infinite = false;
                        }
                        ui.add_enabled(
                            !self.config.repeat_infinite,
                            egui::DragValue::new(&mut self.config.repeat_times)
                                .range(1..=100_000_000),
                        );
                        ui.label(s.times);
                    });
                    if ui.radio(self.config.repeat_infinite, s.repeat_until).clicked() {
                        self.config.repeat_infinite = true;
                    }
                });

                // ---- cursor position ---------------------------------------------
                ui.group(|ui| {
                    ui.label(egui::RichText::new(s.sec_position).strong());
                    ui.horizontal_wrapped(|ui| {
                        ui.radio_value(&mut self.config.position_mode, 0, s.pos_current);
                        ui.radio_value(&mut self.config.position_mode, 1, s.pos_fixed);
                        ui.radio_value(&mut self.config.position_mode, 2, s.pos_points);
                    });

                    if self.config.position_mode == 1 {
                        ui.horizontal_wrapped(|ui| {
                            match self.pick_point_deadline {
                                Some(d) => {
                                    ui.label(
                                        s.picking
                                            .replace("{}", &seconds_left(d).to_string()),
                                    );
                                }
                                None => {
                                    if ui.button(s.pick_location).clicked() {
                                        self.pick_point_deadline =
                                            Some(Instant::now() + Duration::from_secs(3));
                                    }
                                }
                            }
                            ui.label("X");
                            ui.add(
                                egui::DragValue::new(&mut self.config.pos_x)
                                    .range(-32000..=32000),
                            );
                            ui.label("Y");
                            ui.add(
                                egui::DragValue::new(&mut self.config.pos_y)
                                    .range(-32000..=32000),
                            );
                        });
                    }

                    if self.config.position_mode == 2 {
                        ui.horizontal_wrapped(|ui| {
                            match self.pick_point_deadline {
                                Some(d) => {
                                    ui.label(
                                        s.picking
                                            .replace("{}", &seconds_left(d).to_string()),
                                    );
                                }
                                None => {
                                    if ui.button(s.add_point).clicked() {
                                        self.pick_point_deadline =
                                            Some(Instant::now() + Duration::from_secs(3));
                                    }
                                }
                            }
                            if ui.button(s.del_point).clicked() {
                                self.config.points.pop();
                            }
                        });
                        let points = self.config.points.clone();
                        if !points.is_empty() {
                            egui::ScrollArea::vertical()
                                .max_height(90.0)
                                .id_salt("points")
                                .show(ui, |ui| {
                                    for (i, (x, y)) in points.iter().enumerate() {
                                        ui.label(
                                            egui::RichText::new(format!(
                                                "{:>3}.  {x}, {y}",
                                                i + 1
                                            ))
                                            .monospace()
                                            .small(),
                                        );
                                    }
                                });
                        }
                    }

                    if self.config.position_mode != 0 {
                        ui.horizontal_wrapped(|ui| {
                            ui.label(s.jitter_px).on_hover_text(s.tip_jitter);
                            ui.add(
                                egui::DragValue::new(&mut self.config.jitter_px).range(0..=500),
                            );
                            ui.checkbox(&mut self.config.return_cursor, s.return_cursor);
                        });
                    }
                });

                // ---- start / stop -------------------------------------------------
                ui.add_space(4.0);
                ui.horizontal_wrapped(|ui| {
                    let start = egui::Button::new(
                        egui::RichText::new(format!(
                            "{}  ({})",
                            s.btn_start,
                            self.config.hotkey_toggle.label()
                        ))
                        .strong(),
                    )
                    .min_size(egui::vec2(150.0, 30.0));
                    if ui.add_enabled(!running, start).clicked() {
                        start_clicking(&self.state);
                    }
                    let stop = egui::Button::new(
                        egui::RichText::new(format!(
                            "{}  ({})",
                            s.btn_stop,
                            self.config.hotkey_stop.label()
                        ))
                        .strong(),
                    )
                    .min_size(egui::vec2(150.0, 30.0));
                    if ui.add_enabled(running, stop).clicked() {
                        stop_clicking(&self.state);
                    }
                });

                ui.horizontal_wrapped(|ui| {
                    let clicks = self.state.clicks.load(Ordering::Relaxed);
                    ui.label(s.stat_clicks.replace("{}", &clicks.to_string()));
                    ui.label(
                        s.stat_cps.replace("{}", &format!("{:.1}", self.cps)),
                    );
                    if running {
                        let started = self.state.started_us.load(Ordering::Relaxed);
                        ui.label(
                            s.stat_elapsed
                                .replace("{}", &format_hms(now_us().saturating_sub(started))),
                        );
                    }
                });

                ui.separator();

                // ---- time limit ----------------------------------------------------
                egui::CollapsingHeader::new(s.sec_limits).show(ui, |ui| {
                    ui.horizontal_wrapped(|ui| {
                        ui.checkbox(&mut self.config.limit_enabled, s.limit_cb);
                        ui.add(egui::DragValue::new(&mut self.config.limit_h).range(0..=240));
                        ui.label(s.limit_h);
                        ui.add(egui::DragValue::new(&mut self.config.limit_m).range(0..=59));
                        ui.label(s.limit_m);
                        ui.add(egui::DragValue::new(&mut self.config.limit_s).range(0..=59));
                        ui.label(s.limit_s);
                    });
                });

                // ---- pixel condition -------------------------------------------------
                egui::CollapsingHeader::new(s.sec_pixel).show(ui, |ui| {
                    ui.checkbox(&mut self.config.pixel_enabled, s.pixel_cb);
                    ui.horizontal_wrapped(|ui| {
                        ui.label("X");
                        ui.add(
                            egui::DragValue::new(&mut self.config.pixel_x).range(-32000..=32000),
                        );
                        ui.label("Y");
                        ui.add(
                            egui::DragValue::new(&mut self.config.pixel_y).range(-32000..=32000),
                        );
                        let mut col =
                            [self.config.pixel_r, self.config.pixel_g, self.config.pixel_b];
                        if ui.color_edit_button_srgb(&mut col).changed() {
                            self.config.pixel_r = col[0];
                            self.config.pixel_g = col[1];
                            self.config.pixel_b = col[2];
                        }
                    });
                    ui.horizontal_wrapped(|ui| {
                        ui.label(s.pixel_tol);
                        ui.add(
                            egui::DragValue::new(&mut self.config.pixel_tolerance).range(0..=255),
                        );
                        ui.selectable_value(&mut self.config.pixel_mode, 0, s.pixel_match);
                        ui.selectable_value(&mut self.config.pixel_mode, 1, s.pixel_differ);
                    });
                    match self.pick_pixel_deadline {
                        Some(d) => {
                            ui.label(s.picking.replace("{}", &seconds_left(d).to_string()));
                        }
                        None => {
                            if ui.button(s.pixel_pick).clicked() {
                                self.pick_pixel_deadline =
                                    Some(Instant::now() + Duration::from_secs(3));
                            }
                        }
                    }
                });

                // ---- hotkeys -----------------------------------------------------------
                egui::CollapsingHeader::new(s.sec_hotkeys).show(ui, |ui| {
                    let mut changed = false;
                    changed |=
                        hotkey_row(ui, s, s.hk_toggle, "hk1", 1, &mut self.config.hotkey_toggle);
                    changed |=
                        hotkey_row(ui, s, s.hk_stop, "hk2", 2, &mut self.config.hotkey_stop);
                    if changed {
                        publish_hotkeys(&self.config);
                        request_hotkey_message(WM_APP_REHOTKEY);
                    }
                    ui.checkbox(&mut self.config.hold_mode, s.hold_mode)
                        .on_hover_text(s.tip_hold_mode);
                    if HK_FAILED.load(Ordering::Relaxed) != 0 {
                        ui.colored_label(egui::Color32::from_rgb(255, 170, 60), s.hk_failed);
                    }
                });

                // ---- appearance ---------------------------------------------------------
                egui::CollapsingHeader::new(s.sec_appearance).show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.label(s.theme);
                        egui::ComboBox::from_id_salt("theme")
                            .selected_text(THEME_NAMES[self.config.default_theme])
                            .show_ui(ui, |ui| {
                                for (i, name) in THEME_NAMES.iter().enumerate() {
                                    if ui
                                        .selectable_label(self.config.default_theme == i, *name)
                                        .clicked()
                                    {
                                        self.config.default_theme = i;
                                        self.theme_dirty = true;
                                    }
                                }
                            });
                    });
                    if ui.checkbox(&mut self.config.transparent_ui, s.transparent_ui).changed() {
                        self.theme_dirty = true;
                    }
                    if ui.checkbox(&mut self.config.always_on_top, s.on_top).changed() {
                        let level = if self.config.always_on_top {
                            egui::viewport::WindowLevel::AlwaysOnTop
                        } else {
                            egui::viewport::WindowLevel::Normal
                        };
                        ui.ctx()
                            .send_viewport_cmd(egui::viewport::ViewportCommand::WindowLevel(level));
                    }
                    ui.checkbox(&mut self.config.tray_enabled, s.tray_cb);
                    if self.config.tray_enabled {
                        ui.checkbox(&mut self.config.close_to_tray, s.close_tray_cb);
                    }
                    ui.horizontal(|ui| {
                        ui.label(s.language);
                        egui::ComboBox::from_id_salt("lang")
                            .selected_text(match self.config.default_lang {
                                1 => "English",
                                2 => "Русский",
                                3 => "Українська",
                                4 => "Português",
                                5 => "Español",
                                6 => "中文",
                                _ => s.lang_auto,
                            })
                            .show_ui(ui, |ui| {
                                let names = [
                                    s.lang_auto,
                                    "English",
                                    "Русский",
                                    "Українська",
                                    "Português",
                                    "Español",
                                    "中文",
                                ];
                                for (i, name) in names.iter().enumerate() {
                                    if ui
                                        .selectable_label(self.config.default_lang == i, *name)
                                        .clicked()
                                    {
                                        self.config.default_lang = i;
                                    }
                                }
                            });
                    });
                    if ui.button(s.lang_template).clicked() {
                        let idx = self.config.default_lang.saturating_sub(1);
                        match export_lang_template(idx) {
                            Ok(p) => {
                                self.status_msg = s.saved.replace(
                                    "{}",
                                    &p.file_name()
                                        .map(|n| n.to_string_lossy().to_string())
                                        .unwrap_or_default(),
                                )
                            }
                            Err(e) => self.status_msg = s.save_err.replace("{}", &e.to_string()),
                        }
                    }
                });

                // ---- profiles ------------------------------------------------------------
                egui::CollapsingHeader::new(s.sec_profiles).show(ui, |ui| {
                    ui.horizontal_wrapped(|ui| {
                        ui.label(s.prof_name);
                        ui.add(
                            egui::TextEdit::singleline(&mut self.profile_name)
                                .desired_width(110.0),
                        );
                        if ui.button(s.prof_save).clicked()
                            && !self.profile_name.trim().is_empty()
                        {
                            let path = profile_path(&self.profile_name);
                            match save_config_to(&path, &self.config) {
                                Ok(()) => {
                                    self.profiles = list_profiles();
                                    self.status_msg =
                                        s.saved.replace("{}", self.profile_name.trim());
                                }
                                Err(e) => {
                                    self.status_msg = s.save_err.replace("{}", &e.to_string())
                                }
                            }
                        }
                        if ui.button(s.prof_delete).clicked()
                            && !self.profile_name.trim().is_empty()
                        {
                            let _ = std::fs::remove_file(profile_path(&self.profile_name));
                            self.profiles = list_profiles();
                            self.status_msg = s.done.to_string();
                        }
                    });
                    let names = self.profiles.clone();
                    ui.horizontal_wrapped(|ui| {
                        for name in names {
                            if ui.small_button(&name).clicked() {
                                self.config = load_config_from(&profile_path(&name));
                                self.profile_name = name.clone();
                                self.theme_dirty = true;
                                publish_hotkeys(&self.config);
                                request_hotkey_message(WM_APP_REHOTKEY);
                                self.status_msg = s.loaded.replace("{}", &name);
                            }
                        }
                    });
                });

                ui.horizontal_wrapped(|ui| {
                    if ui.button(s.save_settings).clicked() {
                        self.config.sanitize();
                        match save_config_to(&paths::config_path(), &self.config) {
                            Ok(()) => self.status_msg = s.settings_saved.to_string(),
                            Err(e) => self.status_msg = s.save_err.replace("{}", &e.to_string()),
                        }
                    }
                    if ui.add_enabled(!running, egui::Button::new(s.reset_all)).clicked() {
                        self.config = AppConfig::default();
                        self.theme_dirty = true;
                        publish_hotkeys(&self.config);
                        request_hotkey_message(WM_APP_REHOTKEY);
                        self.status_msg = s.done.to_string();
                    }
                });

                ui.label(
                    egui::RichText::new(format!("{} {}", s.data_dir, paths::data_dir().display()))
                        .weak()
                        .small(),
                );

                ui.separator();
                let status = if !self.status_msg.is_empty() {
                    self.status_msg.clone()
                } else if running {
                    s.status_running.to_string()
                } else if self.config.hold_mode {
                    s.status_hold.to_string()
                } else {
                    s.status_ready.to_string()
                };
                ui.label(format!("ℹ {status}"));
            });
        });

        // Idempotent every frame: the engine can never drift from the UI.
        self.config.sanitize();
        apply_config_to_state(&self.config, &self.state);
        ui.ctx().request_repaint_after(Duration::from_millis(100));
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        if CAPTURE_SLOT.load(Ordering::Relaxed) != 0 {
            end_capture();
        }
        self.state.stop.store(true, Ordering::Relaxed);

        let deadline = Instant::now() + Duration::from_millis(300);
        while self.state.running.load(Ordering::Relaxed) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }

        self.config.sanitize();
        if let Err(e) = save_config_to(&paths::config_path(), &self.config) {
            warn!("could not autosave config: {e}");
        }

        #[cfg(windows)]
        {
            let tid = HOOK_THREAD_ID.load(Ordering::Relaxed);
            if tid != 0 {
                unsafe {
                    let _ = win32::PostThreadMessageW(
                        tid,
                        win32::WM_QUIT,
                        win32::WPARAM(0),
                        win32::LPARAM(0),
                    );
                }
            }
        }
        info!("application exiting gracefully");
    }
}

// ============================================================================
// Entry point
// ============================================================================

fn init_logging() -> Option<tracing_appender::non_blocking::WorkerGuard> {
    let appender = tracing_appender::rolling::daily(paths::log_dir(), "auto-clicker.log");
    let (writer, guard) = tracing_appender::non_blocking(appender);
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(writer)
        .with_ansi(false)
        .init();
    Some(guard)
}

fn load_window_icon() -> egui::IconData {
    if ICON_RGBA.len() == (ICON_SIZE * ICON_SIZE * 4) as usize {
        egui::IconData { rgba: ICON_RGBA.to_vec(), width: ICON_SIZE, height: ICON_SIZE }
    } else {
        warn!("embedded icon has an unexpected size - using the OS default");
        egui::IconData::default()
    }
}

const HELP_TEXT: &str = "\
Auto Clicker - a configurable clicker and key repeater for Windows.

USAGE:
    auto-clicker [OPTIONS]

OPTIONS:
    -h, --help       Show this help
    -V, --version    Show the version

Everything else is configured in the window and saved to config.json.
";

fn main() -> Result<()> {
    init_epoch();
    let _log_guard = init_logging();
    platform::set_dpi_awareness();

    for arg in std::env::args().skip(1) {
        match arg.as_str() {
            "--help" | "-h" => {
                platform::attach_parent_console();
                print!("{HELP_TEXT}");
                return Ok(());
            }
            "--version" | "-V" => {
                platform::attach_parent_console();
                println!("auto-clicker {APP_VERSION}");
                return Ok(());
            }
            _ => {}
        }
    }

    if !platform::acquire_single_instance() {
        platform::focus_existing_instance();
        info!("another instance is already running - exiting");
        return Ok(());
    }

    let mut config = load_config_from(&paths::config_path());
    config.sanitize();
    publish_hotkeys(&config);
    info!("data directory: {}", paths::data_dir().display());

    let state = AppState::new();
    apply_config_to_state(&config, &state);

    #[cfg(windows)]
    {
        let st = state.clone();
        let tray_on = config.tray_enabled;
        std::thread::Builder::new()
            .name("hotkeys".into())
            .spawn(move || hotkey_thread(st, tray_on))?;
    }

    let mut viewport = egui::ViewportBuilder::default()
        .with_inner_size([470.0, 720.0])
        .with_min_inner_size([390.0, 420.0])
        .with_icon(load_window_icon())
        .with_transparent(true);
    if config.always_on_top {
        viewport = viewport.with_always_on_top();
    }

    let options = eframe::NativeOptions { viewport, ..Default::default() };
    let st = state.clone();

    eframe::run_native(
        APP_TITLE,
        options,
        Box::new(move |cc| Ok(Box::new(ClickerApp::new(cc, st, config)))),
    )
    .map_err(|e| anyhow::anyhow!("{e}"))
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interval_math_and_floor() {
        let cfg = AppConfig {
            interval_h: 0,
            interval_m: 1,
            interval_s: 2,
            interval_ms: 500,
            ..Default::default()
        };
        assert_eq!(cfg.interval_us(), 62_500_000);

        // Zero everywhere still leaves a 1 ms floor, so the loop can never spin free.
        let zero = AppConfig {
            interval_h: 0,
            interval_m: 0,
            interval_s: 0,
            interval_ms: 0,
            ..Default::default()
        };
        assert_eq!(zero.interval_us(), 1_000);
    }

    #[test]
    fn default_matches_op_auto_clicker() {
        let cfg = AppConfig::default();
        assert_eq!(cfg.interval_us(), 100_000); // 100 ms
        assert_eq!(cfg.random_offset_ms, 40);
        assert_eq!(cfg.hotkey_toggle.vk, 0x75); // F6
        assert!(cfg.repeat_infinite);
    }

    #[test]
    fn sanitize_clamps_everything() {
        let mut cfg = AppConfig {
            interval_ms: 9999,
            repeat_times: 0,
            jitter_px: -5,
            click_type: 77,
            mouse_button: 9,
            default_theme: 99,
            pixel_tolerance: 900,
            key_vk: 0xFFFF,
            ..Default::default()
        };
        cfg.sanitize();
        assert_eq!(cfg.interval_ms, 999);
        assert_eq!(cfg.repeat_times, 1);
        assert_eq!(cfg.jitter_px, 0);
        assert_eq!(cfg.click_type, 2);
        assert_eq!(cfg.mouse_button, 4);
        assert_eq!(cfg.default_theme, THEME_NAMES.len() - 1);
        assert_eq!(cfg.pixel_tolerance, 255);
        assert_eq!(cfg.key_vk, 0x20);
    }

    #[test]
    fn limit_math() {
        let cfg =
            AppConfig { limit_h: 1, limit_m: 2, limit_s: 3, ..Default::default() };
        assert_eq!(cfg.limit_us(), 3_723_000_000);
    }

    #[test]
    fn signed_jitter_stays_in_range() {
        let mut rng = Rng::new();
        for _ in 0..2000 {
            let v = rng.signed(7);
            assert!((-7..=7).contains(&v));
        }
        assert_eq!(rng.signed(0), 0);
        assert_eq!(rng.below(0), 0);
    }

    #[test]
    fn mouse_button_indices() {
        assert_eq!(MouseButton::from_index(0), MouseButton::Left);
        assert_eq!(MouseButton::from_index(4), MouseButton::X2);
        assert_eq!(MouseButton::from_index(99), MouseButton::Left);
    }

    #[test]
    fn vk_names_cover_the_common_keys() {
        assert_eq!(vk_name(0x75), "F6");
        assert_eq!(vk_name(0x41), "A");
        assert_eq!(vk_name(0x20), "Space");
        assert_eq!(vk_name(0x00), "—");
        assert!(vk_name(0xFE).starts_with("VK "));
    }

    #[test]
    fn hotkey_labels_include_modifiers() {
        let hk = Hotkey { vk: 0x41, ctrl: true, alt: false, shift: true };
        assert_eq!(hk.label(), "Ctrl+Shift+A");
        assert_eq!(Hotkey::plain(0).label(), "—");
    }

    #[test]
    fn language_overrides_apply() {
        let mut map = BTreeMap::new();
        map.insert("btn_start".to_string(), "GO".to_string());
        map.insert("btn_stop".to_string(), String::new()); // empty is ignored
        let s = EN.with_overrides(&map);
        assert_eq!(s.btn_start, "GO");
        assert_eq!(s.btn_stop, EN.btn_stop);
        assert!(s.to_map().contains_key("sec_interval"));
    }

    #[test]
    fn absolute_normalization_hits_both_edges() {
        assert_eq!(platform::normalize_abs(0, 0, 0, 0, 1920, 1080), (0, 0));
        assert_eq!(platform::normalize_abs(1919, 1079, 0, 0, 1920, 1080), (65535, 65535));
    }

    #[test]
    fn profile_names_are_sanitized() {
        let p = profile_path("farm/../evil");
        assert!(p.file_name().unwrap().to_string_lossy().starts_with("farm_"));
    }
}