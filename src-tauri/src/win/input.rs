//! 输入转发：桌面处于前台时，把鼠标事件送给壁纸窗口，并把真实焦点交给它让键盘能用。
//!
//! 壁纸窗口挂在 WorkerW 下面，正常情况**收不到任何输入**（桌面图层在它上面）。这里的做法：
//!
//! * **鼠标**用 `WH_MOUSE_LL` 低层钩子：它给出绝对屏幕坐标、按键切换与滚轮增量，
//!   正好一一对应我们要 post 的 `WM_*` 消息；钩子**不吞事件**（照常 `CallNextHookEx`），
//!   桌面本身的行为不受影响。
//! * **键盘**不需要合成：Chromium / Unity 的文本栈会丢掉合成按键。改成在桌面前台时
//!   把真实焦点给壁纸窗口（`AttachThreadInput` + `SetForegroundWindow`），由系统正常投递。
//! * **只在桌面是前台时开闸**：否则用户在任何程序里动鼠标，都会有一堆合成消息涌进壁纸。
//! * 鼠标状态同时通过 `pointer_state()` 暴露给配置包 / 输入包 —— Unity 的新输入系统
//!   收不到合成消息，但可以读这些数值，两条路都通。

use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU32, Ordering};
use std::sync::mpsc;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState;
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, DispatchMessageW, GetForegroundWindow, GetMessageW, PostMessageW,
    SetWindowsHookExW, TranslateMessage, UnhookWindowsHookEx, HC_ACTION, MSLLHOOKSTRUCT,
    WH_MOUSE_LL, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MBUTTONDOWN, WM_MBUTTONUP, WM_MOUSEMOVE,
    WM_MOUSEWHEEL, WM_MOUSEHWHEEL, WM_RBUTTONDOWN, WM_RBUTTONUP, WM_XBUTTONDOWN, WM_XBUTTONUP,
};

use super::desktop;

/// 目标窗口里的鼠标状态，随输入包一起发给 Unity。
#[derive(Debug, Clone, Copy, Default)]
pub struct PointerState {
    /// 客户区坐标（像素，左上角原点）。
    pub x: f32,
    pub y: f32,
    /// 光标是否落在窗口内。
    pub inside: bool,
    pub left: bool,
    pub right: bool,
    pub middle: bool,
    /// 自上次读取以来累计的滚轮位移。
    pub wheel: i32,
}

/// 转发目标窗口（0 = 没有目标）。
static TARGET: AtomicIsize = AtomicIsize::new(0);
/// 是否转发鼠标。
static MOUSE: AtomicBool = AtomicBool::new(false);
/// 是否把真实焦点给壁纸（键盘）。
static KEYBOARD: AtomicBool = AtomicBool::new(false);
/// 钩子线程是否已经起来。
static ACTIVE: AtomicBool = AtomicBool::new(false);
/// 钩子线程 id（退出时给它发 WM_QUIT）。
static HOOK_THREAD: AtomicU32 = AtomicU32::new(0);
/// 要求钩子线程退出。
static STOP: AtomicBool = AtomicBool::new(false);
/// 钩子句柄（退出时卸载）。
static HOOK: Mutex<Option<isize>> = Mutex::new(None);
/// 鼠标状态。
static POINTER: Mutex<PointerState> = Mutex::new(PointerState {
    x: 0.0,
    y: 0.0,
    inside: false,
    left: false,
    right: false,
    middle: false,
    wheel: 0,
});

/// 幂等：第一次调用时把钩子线程和焦点线程起起来。
pub fn ensure_helper() -> Result<(), String> {
    if ACTIVE.load(Ordering::SeqCst) {
        return Ok(());
    }
    let (sender, receiver) = mpsc::channel::<Result<(), String>>();
    std::thread::Builder::new()
        .name("input-hook".to_string())
        .spawn(move || hook_thread(sender))
        .map_err(|e| format!("创建输入转发线程失败：{e}"))?;

    match receiver.recv_timeout(Duration::from_secs(3)) {
        Ok(Ok(())) => {
            ACTIVE.store(true, Ordering::SeqCst);
            spawn_focus_thread();
            Ok(())
        }
        Ok(Err(err)) => Err(err),
        Err(_) => Err("输入转发线程没有在 3 秒内就绪".to_string()),
    }
}

/// 设置转发目标。`mouse` / `keyboard` 至少开一个才有意义。
pub fn set_target(hwnd: isize, mouse: bool, keyboard: bool) -> Result<(), String> {
    if hwnd == 0 {
        clear_target();
        return Ok(());
    }
    ensure_helper()?;
    MOUSE.store(mouse, Ordering::SeqCst);
    KEYBOARD.store(keyboard, Ordering::SeqCst);
    TARGET.store(hwnd, Ordering::SeqCst);
    if !mouse && !keyboard {
        TARGET.store(0, Ordering::SeqCst);
    }
    Ok(())
}

/// 停止转发。
pub fn clear_target() {
    TARGET.store(0, Ordering::SeqCst);
    MOUSE.store(false, Ordering::SeqCst);
    KEYBOARD.store(false, Ordering::SeqCst);
    if let Ok(mut pointer) = POINTER.lock() {
        pointer.left = false;
        pointer.right = false;
        pointer.middle = false;
        pointer.wheel = 0;
        pointer.inside = false;
    }
}

/// 是否正在转发。
pub fn is_active() -> bool {
    TARGET.load(Ordering::SeqCst) != 0
}

/// 读取并清零累计的滚轮位移；坐标每次读取时按当前光标位置刷新。
pub fn pointer_state() -> PointerState {
    let mut guard = POINTER.lock().unwrap_or_else(|e| e.into_inner());
    let target = current_target();
    if let Some(hwnd) = target {
        refresh_pointer(&mut guard, hwnd);
    }
    let snapshot = *guard;
    guard.wheel = 0;
    snapshot
}

fn current_target() -> Option<HWND> {
    let value = TARGET.load(Ordering::SeqCst);
    if value == 0 {
        None
    } else {
        Some(HWND(value as *mut std::ffi::c_void))
    }
}

/// 闸门：桌面在前台，或者焦点已经在我们壁纸窗口上（键盘接管后就是这个状态）。
fn gate_open(target: HWND) -> bool {
    unsafe {
        if GetForegroundWindow() == target {
            return true;
        }
    }
    desktop::is_desktop_foreground()
}

/// 用当前光标位置刷新鼠标状态。
fn refresh_pointer(state: &mut PointerState, target: HWND) {
    unsafe {
        let mut point = POINT { x: 0, y: 0 };
        if windows::Win32::UI::WindowsAndMessaging::GetCursorPos(&mut point).is_err() {
            return;
        }
        let (x, y) = desktop::screen_to_client(target, point.x, point.y);
        let (width, height) = desktop::client_size(target);
        state.x = x as f32;
        state.y = y as f32;
        state.inside = x >= 0 && y >= 0 && x < width && y < height;
        state.left = key_down(0x01); // VK_LBUTTON
        state.right = key_down(0x02);
        state.middle = key_down(0x04);
    }
}

fn key_down(code: i32) -> bool {
    unsafe { GetAsyncKeyState(code) as u16 & 0x8000 != 0 }
}

/* ------------------------------------------------------------ 钩子线程 */

fn hook_thread(ready: mpsc::Sender<Result<(), String>>) {
    unsafe {
        let hook = match SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_hook), None, 0) {
            Ok(hook) => hook,
            Err(err) => {
                let _ = ready.send(Err(format!("安装鼠标钩子失败：{err}（可能被安全软件拦截）")));
                return;
            }
        };
        *HOOK.lock().unwrap_or_else(|e| e.into_inner()) = Some(hook.0 as isize);
        HOOK_THREAD.store(
            windows::Win32::System::Threading::GetCurrentThreadId(),
            Ordering::SeqCst,
        );
        let _ = ready.send(Ok(()));

        // 低层钩子要求线程自己泵消息
        let mut message = windows::Win32::UI::WindowsAndMessaging::MSG::default();
        while !STOP.load(Ordering::SeqCst) {
            let result = GetMessageW(&mut message, None, 0, 0);
            if result.0 <= 0 {
                break;
            }
            let _ = TranslateMessage(&message);
            let _ = DispatchMessageW(&message);
        }

        let _ = UnhookWindowsHookEx(hook);
        *HOOK.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }
}

unsafe extern "system" fn mouse_hook(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 {
        if let Some(target) = current_target() {
            let info = &*(lparam.0 as *const MSLLHOOKSTRUCT);
            let message = wparam.0 as u32;
            update_pointer(target, message, info);
            if MOUSE.load(Ordering::SeqCst) && gate_open(target) {
                forward_mouse(target, message, info);
            }
        }
    }
    CallNextHookEx(None, code, wparam, lparam)
}

/// 收到钩子事件时更新共享的鼠标状态（不依赖最后一次轮询）。
unsafe fn update_pointer(target: HWND, message: u32, info: &MSLLHOOKSTRUCT) {
    let mut guard = POINTER.lock().unwrap_or_else(|e| e.into_inner());
    let (x, y) = desktop::screen_to_client(target, info.pt.x, info.pt.y);
    let (width, height) = desktop::client_size(target);
    guard.x = x as f32;
    guard.y = y as f32;
    guard.inside = x >= 0 && y >= 0 && x < width && y < height;

    match message {
        WM_LBUTTONDOWN => guard.left = true,
        WM_LBUTTONUP => guard.left = false,
        WM_RBUTTONDOWN => guard.right = true,
        WM_RBUTTONUP => guard.right = false,
        WM_MBUTTONDOWN => guard.middle = true,
        WM_MBUTTONUP => guard.middle = false,
        WM_MOUSEWHEEL => {
            let delta = ((info.mouseData >> 16) & 0xffff) as u16 as i16;
            guard.wheel = guard.wheel.saturating_add(delta as i32);
        }
        WM_MOUSEHWHEEL => {
            let delta = ((info.mouseData >> 16) & 0xffff) as u16 as i16;
            guard.wheel = guard.wheel.saturating_add(delta as i32);
        }
        _ => {}
    }
}

/// 把钩子事件翻译成消息 post 给壁纸窗口。
///
/// 坐标是**客户区**坐标；滚轮消息按 Windows 约定把增量放在 `wParam` 高位、屏幕坐标放在 `lParam`，
/// 所以滚轮与其它消息分开处理。
unsafe fn forward_mouse(target: HWND, message: u32, info: &MSLLHOOKSTRUCT) {
    let (client_x, client_y) = desktop::screen_to_client(target, info.pt.x, info.pt.y);
    let client_param = make_lparam(client_x, client_y);

    let (post_message, wparam, lparam) = match message {
        WM_MOUSEMOVE => (WM_MOUSEMOVE, WPARAM(0), client_param),
        WM_LBUTTONDOWN => (WM_LBUTTONDOWN, WPARAM(0x0001), client_param),
        WM_LBUTTONUP => (WM_LBUTTONUP, WPARAM(0), client_param),
        WM_RBUTTONDOWN => (WM_RBUTTONDOWN, WPARAM(0x0002), client_param),
        WM_RBUTTONUP => (WM_RBUTTONUP, WPARAM(0), client_param),
        WM_MBUTTONDOWN => (WM_MBUTTONDOWN, WPARAM(0x0010), client_param),
        WM_MBUTTONUP => (WM_MBUTTONUP, WPARAM(0), client_param),
        WM_XBUTTONDOWN | WM_XBUTTONUP => (
            message,
            WPARAM(((info.mouseData >> 16) & 0xffff) as usize),
            client_param,
        ),
        WM_MOUSEWHEEL | WM_MOUSEHWHEEL => (
            message,
            WPARAM(((info.mouseData & 0xffff) << 16) as usize),
            make_lparam(info.pt.x, info.pt.y),
        ),
        _ => return,
    };

    let _ = PostMessageW(Some(target), post_message, wparam, lparam);
}

fn make_lparam(x: i32, y: i32) -> LPARAM {
    let packed = ((y as u16 as u32) << 16) | (x as u16 as u32);
    LPARAM(packed as isize)
}

/* ------------------------------------------------------------ 焦点线程 */

/// 桌面前台时把真实焦点交给壁纸窗口，键盘就能正常投递（包括 Unity 的新输入系统）。
///
/// `SetForegroundWindow` 对「不是前台的进程」有限制，所以先把自己和当前前台线程
/// attach 到一起 —— 这是这个 API 的标准绕法。
fn spawn_focus_thread() {
    std::thread::Builder::new()
        .name("input-focus".to_string())
        .spawn(|| {
            let mut last_grant: Option<Instant> = None;
            loop {
                std::thread::sleep(Duration::from_millis(250));
                if STOP.load(Ordering::SeqCst) {
                    return;
                }
                if !KEYBOARD.load(Ordering::SeqCst) {
                    continue;
                }
                let Some(target) = current_target() else {
                    continue;
                };
                unsafe {
                    if GetForegroundWindow() == target {
                        continue;
                    }
                }
                // 只在桌面前台时接管：用户切到别的程序后立刻放手
                if !desktop::is_desktop_foreground() {
                    continue;
                }
                // 别 250ms 就抢一次，给系统一点面子
                if last_grant.map(|at| at.elapsed() < Duration::from_secs(2)).unwrap_or(false) {
                    continue;
                }
                last_grant = Some(Instant::now());
                grab_foreground(target);
            }
        })
        .ok();
}

fn grab_foreground(target: HWND) {
    use windows::Win32::System::Threading::{AttachThreadInput, GetCurrentThreadId};
    use windows::Win32::UI::WindowsAndMessaging::{
        GetForegroundWindow, GetWindowThreadProcessId, SetForegroundWindow,
    };

    unsafe {
        let foreground = GetForegroundWindow();
        let foreground_thread = if foreground.is_invalid() {
            0
        } else {
            GetWindowThreadProcessId(foreground, None)
        };
        let current = GetCurrentThreadId();

        if foreground_thread != 0 && foreground_thread != current {
            let _ = AttachThreadInput(current, foreground_thread, true);
        }
        let _ = SetForegroundWindow(target);
        if foreground_thread != 0 && foreground_thread != current {
            let _ = AttachThreadInput(current, foreground_thread, false);
        }
    }
}

/// 退出钩子线程（进程结束前调，测试里也用得上）。
#[allow(dead_code)]
pub fn shutdown() {
    STOP.store(true, Ordering::SeqCst);
    let thread = HOOK_THREAD.load(Ordering::SeqCst);
    if thread != 0 {
        unsafe {
            let _ = windows::Win32::UI::WindowsAndMessaging::PostThreadMessageW(
                thread,
                windows::Win32::UI::WindowsAndMessaging::WM_QUIT,
                WPARAM(0),
                LPARAM(0),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lparam_packs_signed_coordinates() {
        let packed = make_lparam(-3, -7);
        let value = packed.0 as u32;
        let x = value as u16 as i16;
        let y = (value >> 16) as u16 as i16;
        assert_eq!((x, y), (-3, -7));
    }

    #[test]
    fn clearing_the_target_disables_forwarding() {
        TARGET.store(4321, Ordering::SeqCst);
        MOUSE.store(true, Ordering::SeqCst);
        assert!(is_active());
        clear_target();
        assert!(!is_active());
        assert!(!MOUSE.load(Ordering::SeqCst));
    }
}
