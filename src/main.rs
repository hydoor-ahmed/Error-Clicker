#![windows_subsystem = "windows"]

use eframe::egui;
use enigo::{Button, Direction, Enigo, Mouse, Settings};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicU8, Ordering};
use std::sync::{Arc, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use windows::core::{w, HSTRING, PWSTR};
use windows::Win32::Foundation::{
    CloseHandle, GetLastError, ERROR_ALREADY_EXISTS, HINSTANCE, LPARAM, LRESULT, WPARAM,
};
use windows::Win32::Media::{timeBeginPeriod, timeEndPeriod};
use windows::Win32::System::Diagnostics::Debug::Beep;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::{
    CreateMutexW, GetCurrentThreadId, OpenProcess, QueryFullProcessImageNameW, ReleaseMutex,
    PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState;
use windows::Win32::UI::WindowsAndMessaging::*;

// ============================================================
// ثوابت
// ============================================================
const MIN_CPS: f32 = 1.0;
const MAX_CPS: f32 = 30.0;
const DEFAULT_CPS: u64 = 13;

const VK_SHIFT_CODE: u32 = 0x10;
const VK_MENU_CODE: u32 = 0x12; // Alt
const VK_ESCAPE_CODE: u32 = 0x1B;

const CAPTURE_NONE: u8 = 0;
const CAPTURE_HOTKEY: u8 = 1;
const CAPTURE_MANUAL: u8 = 2;

// ============================================================
// حالة عامة (من الـ hook ومن الإغلاق)
// ============================================================
static IS_LEFT_DOWN: AtomicBool = AtomicBool::new(false);
static IS_RIGHT_DOWN: AtomicBool = AtomicBool::new(false);
static SHUTDOWN: AtomicBool = AtomicBool::new(false);
static HOOK_THREAD_ID: AtomicU32 = AtomicU32::new(0);
static START: OnceLock<Instant> = OnceLock::new();

fn now_ms() -> u64 {
    START.get_or_init(Instant::now).elapsed().as_millis() as u64
}

// ============================================================
// الإعدادات المشتركة بين الواجهة والـ threads
// القيم الافتراضية = السلوك القديم قبل التحديث
// ============================================================
struct Shared {
    running: AtomicBool,

    // السرعة
    cps: AtomicU64,
    use_range: AtomicBool,
    cps_min: AtomicU64,
    cps_max: AtomicU64,
    hold_manual: AtomicBool,
    hold_ms: AtomicU64,

    // الأزرار والشروط
    left_enabled: AtomicBool,
    right_enabled: AtomicBool,
    only_minecraft: AtomicBool,
    pause_menu_bedrock: AtomicBool,
    pause_on_shift: AtomicBool,

    // المفاتيح (0 = بدون)
    hotkey: AtomicU32,
    manual_key: AtomicU32,
    capture: AtomicU8,

    // التنبيهات
    sound_alert: AtomicBool,
    visual_alert: AtomicBool,

    // الإحصائيات
    total_clicks: AtomicU64,
    live_cps: AtomicU64,
    active_ms_accum: AtomicU64,
    on_since_ms: AtomicU64,
    last_toggle_ms: AtomicU64,
}

impl Shared {
    fn new() -> Self {
        Self {
            running: AtomicBool::new(false),

            cps: AtomicU64::new(DEFAULT_CPS),
            use_range: AtomicBool::new(false),
            cps_min: AtomicU64::new(10),
            cps_max: AtomicU64::new(14),
            hold_manual: AtomicBool::new(false),
            hold_ms: AtomicU64::new(40),

            left_enabled: AtomicBool::new(true),
            right_enabled: AtomicBool::new(false),
            only_minecraft: AtomicBool::new(false),
            pause_menu_bedrock: AtomicBool::new(true),
            pause_on_shift: AtomicBool::new(false),

            hotkey: AtomicU32::new(0),
            manual_key: AtomicU32::new(VK_MENU_CODE),
            capture: AtomicU8::new(CAPTURE_NONE),

            sound_alert: AtomicBool::new(false),
            visual_alert: AtomicBool::new(false),

            total_clicks: AtomicU64::new(0),
            live_cps: AtomicU64::new(0),
            active_ms_accum: AtomicU64::new(0),
            on_since_ms: AtomicU64::new(0),
            last_toggle_ms: AtomicU64::new(0),
        }
    }
}

/// تشغيل/إيقاف المحرك مع تحديث الإحصائيات والتنبيهات
fn set_running(s: &Shared, on: bool) {
    let was = s.running.swap(on, Ordering::SeqCst);
    if was == on {
        return;
    }
    let now = now_ms();
    if on {
        s.on_since_ms.store(now, Ordering::SeqCst);
    } else {
        let since = s.on_since_ms.load(Ordering::SeqCst);
        s.active_ms_accum
            .fetch_add(now.saturating_sub(since), Ordering::SeqCst);
    }
    s.last_toggle_ms.store(now, Ordering::SeqCst);

    if s.sound_alert.load(Ordering::SeqCst) {
        thread::spawn(move || beep(on));
    }
}

fn beep(on: bool) {
    unsafe {
        if on {
            let _ = Beep(900, 70);
            let _ = Beep(1300, 90);
        } else {
            let _ = Beep(1300, 70);
            let _ = Beep(800, 110);
        }
    }
}

fn active_ms(s: &Shared) -> u64 {
    let base = s.active_ms_accum.load(Ordering::SeqCst);
    if s.running.load(Ordering::SeqCst) {
        base + now_ms().saturating_sub(s.on_since_ms.load(Ordering::SeqCst))
    } else {
        base
    }
}

fn reset_stats(s: &Shared) {
    s.total_clicks.store(0, Ordering::SeqCst);
    s.active_ms_accum.store(0, Ordering::SeqCst);
    s.on_since_ms.store(now_ms(), Ordering::SeqCst);
}

// ============================================================
// أدوات ويندوز
// ============================================================
fn msg_box(text: &str) {
    unsafe {
        let _ = MessageBoxW(
            None,
            &HSTRING::from(text),
            w!("Error Clicker"),
            MB_OK | MB_ICONWARNING,
        );
    }
}

fn key_down(vk: u32) -> bool {
    unsafe { (GetAsyncKeyState(vk as i32) as u16 & 0x8000) != 0 }
}

/// يبحث عن أي مفتاح مضغوط حالياً (للتعيين من الواجهة). يتجاهل الزرين الأيسر والأيمن للماوس.
fn scan_pressed_key() -> Option<u32> {
    for vk in 0x04u32..=0xFE {
        if vk == 0x07 || vk == VK_ESCAPE_CODE {
            continue;
        }
        if key_down(vk) {
            return Some(vk);
        }
    }
    None
}

fn vk_name(vk: u32) -> String {
    match vk {
        0 => "None".into(),
        0x04 => "Mouse Middle".into(),
        0x05 => "Mouse X1".into(),
        0x06 => "Mouse X2".into(),
        0x08 => "Backspace".into(),
        0x09 => "Tab".into(),
        0x0D => "Enter".into(),
        0x10 => "Shift".into(),
        0x11 => "Ctrl".into(),
        0x12 => "Alt".into(),
        0x13 => "Pause".into(),
        0x14 => "CapsLock".into(),
        0x20 => "Space".into(),
        0x21 => "PageUp".into(),
        0x22 => "PageDown".into(),
        0x23 => "End".into(),
        0x24 => "Home".into(),
        0x25 => "Left".into(),
        0x26 => "Up".into(),
        0x27 => "Right".into(),
        0x28 => "Down".into(),
        0x2D => "Insert".into(),
        0x2E => "Delete".into(),
        0x30..=0x39 => ((b'0' + (vk - 0x30) as u8) as char).to_string(),
        0x41..=0x5A => ((b'A' + (vk - 0x41) as u8) as char).to_string(),
        0x60..=0x69 => format!("Num{}", vk - 0x60),
        0x6A => "Num*".into(),
        0x6B => "Num+".into(),
        0x6D => "Num-".into(),
        0x6E => "Num.".into(),
        0x6F => "Num/".into(),
        0x70..=0x87 => format!("F{}", vk - 0x70 + 1),
        0x90 => "NumLock".into(),
        0x91 => "ScrollLock".into(),
        0xA0 => "Left Shift".into(),
        0xA1 => "Right Shift".into(),
        0xA2 => "Left Ctrl".into(),
        0xA3 => "Right Ctrl".into(),
        0xA4 => "Left Alt".into(),
        0xA5 => "Right Alt".into(),
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
        _ => format!("Key 0x{:02X}", vk),
    }
}

fn cursor_visible() -> bool {
    unsafe {
        let mut ci = CURSORINFO {
            cbSize: std::mem::size_of::<CURSORINFO>() as u32,
            ..Default::default()
        };
        if GetCursorInfo(&mut ci).is_ok() {
            (ci.flags.0 & CURSOR_SHOWING.0) != 0
        } else {
            false
        }
    }
}

/// اسم ملف العملية المالكة للنافذة النشطة (بحروف صغيرة، الاسم فقط بدون المسار)
fn foreground_process_name() -> String {
    unsafe {
        let hwnd = GetForegroundWindow();
        if hwnd.0.is_null() {
            return String::new();
        }

        let mut pid: u32 = 0;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        if pid == 0 {
            return String::new();
        }

        let Ok(handle) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else {
            return String::new();
        };

        let mut buf = [0u16; 260];
        let mut size = buf.len() as u32;
        let result = QueryFullProcessImageNameW(
            handle,
            PROCESS_NAME_WIN32,
            PWSTR(buf.as_mut_ptr()),
            &mut size,
        );
        let _ = CloseHandle(handle);

        if result.is_ok() {
            let full = String::from_utf16_lossy(&buf[..size as usize]).to_lowercase();
            full.rsplit('\\').next().unwrap_or("").to_string()
        } else {
            String::new()
        }
    }
}

fn is_bedrock(name: &str) -> bool {
    name.contains("minecraft.windows")
}

fn is_minecraft(name: &str) -> bool {
    name == "javaw.exe" || name == "java.exe" || name.starts_with("minecraft")
}

// ============================================================
// Mouse Hook
// ============================================================
unsafe extern "system" fn mouse_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code >= 0 {
        let info = unsafe { &*(lparam.0 as *const MSLLHOOKSTRUCT) };
        let injected = info.flags & LLMHF_INJECTED != 0;
        if !injected {
            match wparam.0 as u32 {
                WM_LBUTTONDOWN => IS_LEFT_DOWN.store(true, Ordering::SeqCst),
                WM_LBUTTONUP => IS_LEFT_DOWN.store(false, Ordering::SeqCst),
                WM_RBUTTONDOWN => IS_RIGHT_DOWN.store(true, Ordering::SeqCst),
                WM_RBUTTONUP => IS_RIGHT_DOWN.store(false, Ordering::SeqCst),
                _ => {}
            }
        }
    }
    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}

fn start_mouse_hook() -> JoinHandle<()> {
    thread::spawn(|| unsafe {
        HOOK_THREAD_ID.store(GetCurrentThreadId(), Ordering::SeqCst);

        let hmod = match GetModuleHandleW(None) {
            Ok(h) => h,
            Err(_) => {
                msg_box("GetModuleHandleW failed.");
                return;
            }
        };

        let hook = match SetWindowsHookExW(
            WH_MOUSE_LL,
            Some(mouse_proc),
            Some(HINSTANCE(hmod.0)),
            0,
        ) {
            Ok(h) => h,
            Err(_) => {
                msg_box("Failed to install the mouse hook.");
                return;
            }
        };

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }

        // تنظيف: فك الـ hook عند الخروج
        let _ = UnhookWindowsHookEx(hook);
    })
}

// ============================================================
// Thread المفاتيح: الهوتكي + تعيين المفاتيح من الواجهة
// ============================================================
fn start_keys_thread(s: Arc<Shared>) -> JoinHandle<()> {
    thread::spawn(move || {
        let mut prev_down = false;

        while !SHUTDOWN.load(Ordering::SeqCst) {
            let cap = s.capture.load(Ordering::SeqCst);

            if cap != CAPTURE_NONE {
                // وضع التعيين: Esc للإلغاء، أو أول مفتاح يُضغط يُعيَّن
                if key_down(VK_ESCAPE_CODE) {
                    s.capture.store(CAPTURE_NONE, Ordering::SeqCst);
                    prev_down = true;
                } else if let Some(vk) = scan_pressed_key() {
                    if cap == CAPTURE_HOTKEY {
                        s.hotkey.store(vk, Ordering::SeqCst);
                    } else {
                        s.manual_key.store(vk, Ordering::SeqCst);
                    }
                    s.capture.store(CAPTURE_NONE, Ordering::SeqCst);
                    prev_down = true; // لا تفعّل الهوتكي فور التعيين
                }
            } else {
                let hk = s.hotkey.load(Ordering::SeqCst);
                if hk != 0 {
                    let down = key_down(hk);
                    if down && !prev_down {
                        let now_running = s.running.load(Ordering::SeqCst);
                        set_running(&s, !now_running);
                    }
                    prev_down = down;
                } else {
                    prev_down = false;
                }
            }

            thread::sleep(Duration::from_millis(10));
        }
    })
}

// ============================================================
// منطق النقر
// ============================================================
fn can_click(s: &Shared) -> bool {
    let manual = s.manual_key.load(Ordering::SeqCst);
    if manual != 0 && key_down(manual) {
        return false; // تحكم يدوي كامل
    }

    let need_name = s.only_minecraft.load(Ordering::SeqCst)
        || s.pause_menu_bedrock.load(Ordering::SeqCst);

    if need_name {
        let name = foreground_process_name();

        if s.only_minecraft.load(Ordering::SeqCst) && !is_minecraft(&name) {
            return false;
        }
        if s.pause_menu_bedrock.load(Ordering::SeqCst) && is_bedrock(&name) && cursor_visible() {
            return false;
        }
    }

    if s.pause_on_shift.load(Ordering::SeqCst) && key_down(VK_SHIFT_CODE) {
        return false;
    }

    true
}

fn pick_cps(s: &Shared) -> u64 {
    if s.use_range.load(Ordering::SeqCst) {
        let a = s.cps_min.load(Ordering::SeqCst);
        let b = s.cps_max.load(Ordering::SeqCst);
        let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
        let (lo, hi) = (lo.max(1), hi.max(1));
        if lo == hi {
            lo
        } else {
            rand::random_range(lo..=hi)
        }
    } else {
        s.cps.load(Ordering::SeqCst).max(1)
    }
}

/// نوم قابل للمقاطعة عند الإغلاق
fn nap(ms: u64) {
    let end = Instant::now() + Duration::from_millis(ms);
    while !SHUTDOWN.load(Ordering::SeqCst) {
        let now = Instant::now();
        if now >= end {
            break;
        }
        thread::sleep((end - now).min(Duration::from_millis(10)));
    }
}

fn update_live_cps(s: &Shared, recent: &mut VecDeque<Instant>) {
    while let Some(t) = recent.front() {
        if t.elapsed() > Duration::from_secs(1) {
            recent.pop_front();
        } else {
            break;
        }
    }
    s.live_cps.store(recent.len() as u64, Ordering::SeqCst);
}

fn start_click_thread(s: Arc<Shared>) -> JoinHandle<()> {
    thread::spawn(move || {
        let mut enigo = match Enigo::new(&Settings::default()) {
            Ok(e) => e,
            Err(_) => {
                msg_box("Failed to initialize input injection (Enigo).");
                return;
            }
        };
        let mut recent: VecDeque<Instant> = VecDeque::new();

        while !SHUTDOWN.load(Ordering::SeqCst) {
            let running = s.running.load(Ordering::SeqCst);
            let want_left =
                running && s.left_enabled.load(Ordering::SeqCst) && IS_LEFT_DOWN.load(Ordering::SeqCst);
            let want_right = running
                && s.right_enabled.load(Ordering::SeqCst)
                && IS_RIGHT_DOWN.load(Ordering::SeqCst);

            if (want_left || want_right) && can_click(&s) {
                let cps = pick_cps(&s);
                let interval = 1000 / cps;

                let manual_hold = s.hold_manual.load(Ordering::SeqCst);
                let hold: u64 = if manual_hold {
                    s.hold_ms.load(Ordering::SeqCst).max(1)
                } else {
                    // تلقائي: يغطي إطاراً كاملاً في بيدروك (30-50ms)
                    (interval / 2).clamp(30, 50) + rand::random_range(0..8)
                };
                let min_wait = if manual_hold { 5 } else { 15 };

                if want_left {
                    let _ = enigo.button(Button::Left, Direction::Press);
                }
                if want_right {
                    let _ = enigo.button(Button::Right, Direction::Press);
                }

                thread::sleep(Duration::from_millis(hold));

                // الرفع دائماً حتى لو طُلب الإغلاق (لا يبقى زر عالقاً)
                if want_left {
                    let _ = enigo.button(Button::Left, Direction::Release);
                    s.total_clicks.fetch_add(1, Ordering::SeqCst);
                    recent.push_back(Instant::now());
                }
                if want_right {
                    let _ = enigo.button(Button::Right, Direction::Release);
                    s.total_clicks.fetch_add(1, Ordering::SeqCst);
                    recent.push_back(Instant::now());
                }

                update_live_cps(&s, &mut recent);

                let wait = interval.saturating_sub(hold).max(min_wait);
                nap(wait);
            } else {
                update_live_cps(&s, &mut recent);
                thread::sleep(Duration::from_millis(5));
            }
        }
    })
}

// ============================================================
// Profiles (في الذاكرة خلال الجلسة)
// ============================================================
#[derive(Clone)]
struct Profile {
    name: String,
    cps: u64,
    use_range: bool,
    min: u64,
    max: u64,
    left: bool,
    right: bool,
}

impl Profile {
    fn apply(&self, s: &Shared) {
        s.cps.store(self.cps, Ordering::SeqCst);
        s.use_range.store(self.use_range, Ordering::SeqCst);
        s.cps_min.store(self.min, Ordering::SeqCst);
        s.cps_max.store(self.max, Ordering::SeqCst);
        s.left_enabled.store(self.left, Ordering::SeqCst);
        s.right_enabled.store(self.right, Ordering::SeqCst);
    }

    fn capture(s: &Shared, name: String) -> Self {
        Self {
            name,
            cps: s.cps.load(Ordering::SeqCst),
            use_range: s.use_range.load(Ordering::SeqCst),
            min: s.cps_min.load(Ordering::SeqCst),
            max: s.cps_max.load(Ordering::SeqCst),
            left: s.left_enabled.load(Ordering::SeqCst),
            right: s.right_enabled.load(Ordering::SeqCst),
        }
    }

    fn summary(&self) -> String {
        let speed = if self.use_range {
            format!("{}-{} CPS", self.min, self.max)
        } else {
            format!("{} CPS", self.cps)
        };
        let mut btns = String::new();
        if self.left {
            btns.push_str(" L");
        }
        if self.right {
            btns.push_str(" R");
        }
        format!("{speed}{btns}")
    }
}

fn default_profiles() -> Vec<Profile> {
    vec![
        Profile { name: "Default".into(), cps: DEFAULT_CPS, use_range: false, min: 10, max: 14, left: true, right: false },
        Profile { name: "Mining".into(), cps: 10, use_range: false, min: 8, max: 12, left: true, right: false },
        Profile { name: "Building".into(), cps: 8, use_range: false, min: 6, max: 10, left: false, right: true },
        Profile { name: "Fast".into(), cps: 16, use_range: true, min: 14, max: 18, left: true, right: false },
    ]
}

// ============================================================
// main
// ============================================================
fn main() -> eframe::Result<()> {
    let _ = now_ms(); // تهيئة المؤقت

    // منع تشغيل نسختين
    let mutex = unsafe { CreateMutexW(None, true, w!("Local\\ErrorClickerProSingleInstance")) };
    let already_running = unsafe { GetLastError() } == ERROR_ALREADY_EXISTS;
    if already_running {
        msg_box("Error Clicker is already running.");
        if let Ok(h) = mutex {
            unsafe {
                let _ = CloseHandle(h);
            }
        }
        return Ok(());
    }

    unsafe {
        timeBeginPeriod(1);
    }

    let shared = Arc::new(Shared::new());

    let hook_handle = start_mouse_hook();
    let keys_handle = start_keys_thread(Arc::clone(&shared));
    let click_handle = start_click_thread(Arc::clone(&shared));

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([430.0, 400.0])
            .with_min_inner_size([400.0, 400.0]),
        ..Default::default()
    };

    let app_shared = Arc::clone(&shared);
    let result = eframe::run_native(
        "Erro Clicker Pro 🛠️",
        options,
        Box::new(move |cc| {
            cc.egui_ctx.set_visuals(egui::Visuals::dark());
            Ok(Box::new(ClickerApp::new(app_shared)))
        }),
    );

    // ======== تنظيف عند الإغلاق ========
    SHUTDOWN.store(true, Ordering::SeqCst);

    let tid = HOOK_THREAD_ID.load(Ordering::SeqCst);
    if tid != 0 {
        unsafe {
            let _ = PostThreadMessageW(tid, WM_QUIT, WPARAM(0), LPARAM(0));
        }
    }

    let _ = click_handle.join();
    let _ = keys_handle.join();
    let _ = hook_handle.join();

    unsafe {
        timeEndPeriod(1);
        if let Ok(h) = mutex {
            let _ = ReleaseMutex(h);
            let _ = CloseHandle(h);
        }
    }

    result
}

// ============================================================
// الواجهة
// ============================================================
struct ClickerApp {
    shared: Arc<Shared>,
    profiles: Vec<Profile>,
    always_on_top: bool,
    applied_on_top: bool,
}

impl ClickerApp {
    fn new(shared: Arc<Shared>) -> Self {
        Self {
            shared,
            profiles: default_profiles(),
            always_on_top: false,
            applied_on_top: false,
        }
    }
}

fn atomic_checkbox(ui: &mut egui::Ui, flag: &AtomicBool, text: &str) -> egui::Response {
    let mut v = flag.load(Ordering::SeqCst);
    let resp = ui.checkbox(&mut v, text);
    if resp.changed() {
        flag.store(v, Ordering::SeqCst);
    }
    resp
}

fn cps_slider(ui: &mut egui::Ui, value: &AtomicU64, text: &str) -> bool {
    let mut v = value.load(Ordering::SeqCst) as f32;
    let changed = ui
        .add(egui::Slider::new(&mut v, MIN_CPS..=MAX_CPS).step_by(1.0).text(text))
        .changed();
    if changed {
        value.store(v as u64, Ordering::SeqCst);
    }
    changed
}

fn key_row(ui: &mut egui::Ui, label: &str, key: &AtomicU32, which: u8, capture: &AtomicU8) {
    ui.horizontal(|ui| {
        ui.label(label);
        let capturing = capture.load(Ordering::SeqCst) == which;
        let text = if capturing {
            "Press a key... (Esc = cancel)".to_string()
        } else {
            vk_name(key.load(Ordering::SeqCst))
        };
        if ui.button(text).clicked() {
            capture.store(if capturing { CAPTURE_NONE } else { which }, Ordering::SeqCst);
        }
        if ui.button("Clear").clicked() {
            key.store(0, Ordering::SeqCst);
            if capturing {
                capture.store(CAPTURE_NONE, Ordering::SeqCst);
            }
        }
    });
}

fn fmt_hms(ms: u64) -> String {
    let s = ms / 1000;
    format!("{:02}:{:02}:{:02}", s / 3600, (s / 60) % 60, s % 60)
}

impl eframe::App for ClickerApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let s = Arc::clone(&self.shared);

        egui::CentralPanel::default().show(ctx, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| {
                ui.vertical_centered(|ui| {
                    ui.add_space(6.0);
                    ui.heading("🖱️ Error Clicker v2.0");
                    ui.separator();
                });

                let running = s.running.load(Ordering::SeqCst);

                // ---------- تنبيه مرئي ----------
                if s.visual_alert.load(Ordering::SeqCst) {
                    let last = s.last_toggle_ms.load(Ordering::SeqCst);
                    let recent = last > 0 && now_ms().saturating_sub(last) < 1500;
                    let (text, color) = if !recent {
                        (" ", egui::Color32::GRAY)
                    } else if running {
                        ("● ENGINE ON", egui::Color32::GREEN)
                    } else {
                        ("● ENGINE OFF", egui::Color32::from_rgb(230, 60, 60))
                    };
                    ui.vertical_centered(|ui| {
                        ui.label(egui::RichText::new(text).size(22.0).strong().color(color));
                    });
                }

                ui.add_space(8.0);

                // ---------- زر التشغيل ----------
                let btn_text = if running { "STOP ENGINE" } else { "START ENGINE" };
                let btn_color = if running {
                    egui::Color32::from_rgb(200, 0, 0)
                } else {
                    egui::Color32::from_rgb(0, 150, 0)
                };
                if ui
                    .add_sized(
                        [ui.available_width(), 40.0],
                        egui::Button::new(btn_text).fill(btn_color),
                    )
                    .clicked()
                {
                    set_running(&s, !running);
                }

                ui.add_space(10.0);

                // ---------- السرعة ----------
                ui.group(|ui| {
                    ui.label("Target Speed (CPS):");
                    let use_range = s.use_range.load(Ordering::SeqCst);

                    if !use_range {
                        cps_slider(ui, &s.cps, "");
                    } else {
                        let ch_min = cps_slider(ui, &s.cps_min, "min");
                        let ch_max = cps_slider(ui, &s.cps_max, "max");
                        let lo = s.cps_min.load(Ordering::SeqCst);
                        let hi = s.cps_max.load(Ordering::SeqCst);
                        if ch_min && lo > hi {
                            s.cps_max.store(lo, Ordering::SeqCst);
                        }
                        if ch_max && hi < lo {
                            s.cps_min.store(hi, Ordering::SeqCst);
                        }
                    }

                    atomic_checkbox(ui, &s.use_range, "Use CPS range (random between min & max)");
                });

                ui.add_space(8.0);

                // ---------- الحالة + CPS الحي ----------
                ui.horizontal(|ui| {
                    ui.label("System Status:");
                    if running {
                        ui.label(egui::RichText::new("READY ✅").color(egui::Color32::GREEN));
                    } else {
                        ui.label(egui::RichText::new("IDLE 💤").color(egui::Color32::YELLOW));
                    }
                    ui.separator();
                    let live = s.live_cps.load(Ordering::SeqCst);
                    let col = if live > 0 { egui::Color32::LIGHT_BLUE } else { egui::Color32::GRAY };
                    ui.label(egui::RichText::new(format!("Live CPS: {live}")).color(col));
                });

                ui.add_space(8.0);

                // ---------- الأزرار والشروط ----------
                egui::CollapsingHeader::new("🎯 Buttons & Conditions")
                    .default_open(false)
                    .show(ui, |ui| {
                        atomic_checkbox(ui, &s.left_enabled, "Left click (hold left button)");
                        atomic_checkbox(ui, &s.right_enabled, "Right click (hold right button)");
                        if !s.left_enabled.load(Ordering::SeqCst)
                            && !s.right_enabled.load(Ordering::SeqCst)
                        {
                            ui.colored_label(egui::Color32::YELLOW, "⚠ Both buttons are disabled");
                        }
                        ui.separator();
                        atomic_checkbox(ui, &s.only_minecraft, "Only click inside Minecraft")
                            .on_hover_text("javaw.exe / java.exe / Minecraft.Windows.exe");
                        atomic_checkbox(
                            ui,
                            &s.pause_menu_bedrock,
                            "Pause when menu/inventory is open (Bedrock)",
                        );
                        atomic_checkbox(ui, &s.pause_on_shift, "Pause while Shift is held");
                        ui.separator();
                        atomic_checkbox(ui, &s.hold_manual, "Manual press duration")
                            .on_hover_text("Unchecked = automatic (30-50ms)");
                        if s.hold_manual.load(Ordering::SeqCst) {
                            let mut h = s.hold_ms.load(Ordering::SeqCst) as f32;
                            if ui
                                .add(egui::Slider::new(&mut h, 5.0..=100.0).step_by(1.0).text("ms"))
                                .changed()
                            {
                                s.hold_ms.store(h as u64, Ordering::SeqCst);
                            }
                        }
                    });

                // ---------- المفاتيح ----------
                egui::CollapsingHeader::new("⌨️ Keys")
                    .default_open(false)
                    .show(ui, |ui| {
                        key_row(ui, "Toggle hotkey:", &s.hotkey, CAPTURE_HOTKEY, &s.capture);
                        key_row(ui, "Manual control key:", &s.manual_key, CAPTURE_MANUAL, &s.capture);
                        ui.label(
                            egui::RichText::new("None = disabled. Mouse middle/side buttons are supported.")
                                .small()
                                .color(egui::Color32::GRAY),
                        );
                    });

                // ---------- التنبيهات ----------
                egui::CollapsingHeader::new("🔔 Alerts")
                    .default_open(false)
                    .show(ui, |ui| {
                        atomic_checkbox(ui, &s.sound_alert, "Sound alert on ON/OFF");
                        atomic_checkbox(ui, &s.visual_alert, "Visual alert banner on ON/OFF");
                        ui.checkbox(&mut self.always_on_top, "Keep this window on top")
                            .on_hover_text("Useful to see the visual alert over a windowed game");
                    });

                // ---------- Profiles ----------
                egui::CollapsingHeader::new("📁 Profiles")
                    .default_open(false)
                    .show(ui, |ui| {
                        for i in 0..self.profiles.len() {
                            let p = &mut self.profiles[i];
                            ui.horizontal(|ui| {
                                ui.add(egui::TextEdit::singleline(&mut p.name).desired_width(90.0));
                                ui.label(
                                    egui::RichText::new(p.summary())
                                        .small()
                                        .color(egui::Color32::GRAY),
                                );
                                if ui.button("Load").clicked() {
                                    p.apply(&s);
                                }
                                if ui.button("Save").on_hover_text("Overwrite with current settings").clicked() {
                                    *p = Profile::capture(&s, p.name.clone());
                                }
                            });
                        }
                        ui.label(
                            egui::RichText::new("Profiles are kept in memory for this session.")
                                .small()
                                .color(egui::Color32::GRAY),
                        );
                    });

                // ---------- الإحصائيات ----------
                egui::CollapsingHeader::new("📊 Session Stats")
                    .default_open(false)
                    .show(ui, |ui| {
                        let clicks = s.total_clicks.load(Ordering::SeqCst);
                        let ms = active_ms(&s);
                        let avg = if ms > 0 { clicks as f64 / (ms as f64 / 1000.0) } else { 0.0 };
                        ui.label(format!("Total clicks: {clicks}"));
                        ui.label(format!("Active time: {}", fmt_hms(ms)));
                        ui.label(format!("Average CPS: {avg:.1}"));
                        if ui.button("Reset stats").clicked() {
                            reset_stats(&s);
                        }
                    });

                ui.add_space(10.0);
                ui.separator();

                let manual = s.manual_key.load(Ordering::SeqCst);
                if manual != 0 {
                    ui.vertical_centered(|ui| {
                        ui.label(
                            egui::RichText::new(format!(
                                "Hold {} to take manual control",
                                vk_name(manual)
                            ))
                            .small()
                            .color(egui::Color32::GRAY),
                        );
                    });
                }
                ui.vertical_centered(|ui| {
                    ui.label("Works on Java & Bedrock");
                    ui.label("Created By error404 ⚡");
                });
            });
        });

        // تطبيق خيار "فوق كل النوافذ" عند تغييره فقط
        if self.always_on_top != self.applied_on_top {
            ctx.send_viewport_cmd(egui::ViewportCommand::WindowLevel(if self.always_on_top {
                egui::WindowLevel::AlwaysOnTop
            } else {
                egui::WindowLevel::Normal
            }));
            self.applied_on_top = self.always_on_top;
        }

        ctx.request_repaint_after(Duration::from_millis(100));
    }
}