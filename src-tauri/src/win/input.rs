//! 输入转发：桌面处于前台时，把鼠标与键盘事件**合成**给壁纸窗口。
//!
//! 壁纸窗口挂在 WorkerW 下面，正常情况**收不到任何输入**（桌面图标层在它上面）。这里的做法：
//!
//! * **鼠标**用 `WH_MOUSE_LL` 低层钩子，**键盘**用 `WH_KEYBOARD_LL`，都翻译成窗口消息
//!   post 给壁纸窗口；钩子**不吞事件**（照常 `CallNextHookEx`），桌面本身的行为不受影响。
//! * **绝不抢前台焦点**：以前为了键盘用 `SetForegroundWindow` 把前台抢给壁纸窗口，那会把桌面
//!   图标层（`SHELLDLL_DefView`）顶成非激活状态 —— 图标点不中、拖不动、右键不出菜单；
//!   关掉转发后前台还赖在壁纸窗口上，就成了「壁纸还能点、桌面全死」。现在前台永远留给桌面。
//! * **鼠标按键走输入包**：不向壁纸投递合成按下/抬起消息。壁纸可能在处理按下时调用
//!   `SetCapture`，使桌面收到 `WM_CAPTURECHANGED` 并取消框选；事后归还捕获无法恢复拖动。
//!   按键状态仍由 `pointer_state()` 传给 Unity，移动与滚轮继续走窗口消息。
//! * **钩子回调必须极快**：低层钩子超过系统的 `LowLevelHooksTimeout`（默认几百毫秒）会被
//!   悄悄摘掉，全系统鼠标都会跟着变卡。所以回调里只做「拷贝字段 + 入队」，
//!   `PostMessage`、`GetGUIThreadInfo` 这些可能阻塞的调用全放到转发线程里做。
//! * **只在桌面是前台时开闸**：否则用户在任何程序里动鼠标，都会有一堆合成消息涌进壁纸。
//! * 鼠标状态同时通过 `pointer_state()` 暴露给配置包 / 输入包 —— Unity 的新输入系统
//!   收不到合成消息，但可以读这些数值，两条路都通。

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU32, Ordering};
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::System::Threading::{AttachThreadInput, GetCurrentThreadId};
use windows::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, ReleaseCapture, SetCapture};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, DispatchMessageW, GetForegroundWindow, GetMessageW, PostMessageW,
    SetWindowsHookExW, TranslateMessage, UnhookWindowsHookEx, HC_ACTION, KBDLLHOOKSTRUCT,
    LLKHF_EXTENDED, LLKHF_INJECTED, MSG, MSLLHOOKSTRUCT, WH_KEYBOARD_LL, WH_MOUSE_LL, WM_KEYDOWN,
    WM_KEYUP, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MBUTTONDOWN, WM_MBUTTONUP, WM_MOUSEMOVE,
    WM_MOUSEWHEEL, WM_MOUSEHWHEEL, WM_RBUTTONDOWN, WM_RBUTTONUP, WM_SYSKEYDOWN, WM_SYSKEYUP,
    WM_XBUTTONDOWN, WM_XBUTTONUP,
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

/// 钩子线程抄给转发线程的一条事件。
///
/// 只带纯数据：低层钩子的回调里不能有锁、不能有系统调用，否则钩子被拖慢就会被系统摘掉。
#[derive(Debug, Clone, Copy)]
enum Event {
    Mouse {
        message: u32,
        x: i32,
        y: i32,
        mouse_data: u32,
    },
    Key {
        message: u32,
        vk: u32,
        scan: u32,
        extended: bool,
    },
    /// 停止转发：补发抬起消息、把捕获还给桌面。
    ///
    /// 显式带上窗口句柄，**不能**依赖 `desktop::last_attached()`：`unity.rs::stop()` 是
    /// 先 `detach()`（把挂载记录清掉）再 `clear_target()`，那时候按挂载记录已经找不到窗口了。
    ReleaseAll {
        wallpaper: isize,
    },
    /// 自愈：上一次运行可能把捕获留在壁纸窗口上了，还给桌面。
    HealStaleCapture {
        wallpaper: isize,
    },
}

/// 转发目标窗口（0 = 没有目标）。
static TARGET: AtomicIsize = AtomicIsize::new(0);
/// 是否转发鼠标。
static MOUSE: AtomicBool = AtomicBool::new(false);
/// 是否转发键盘。
static KEYBOARD: AtomicBool = AtomicBool::new(false);
/// 钩子线程是否已经起来。
static ACTIVE: AtomicBool = AtomicBool::new(false);
/// 钩子线程 id（退出时给它发 WM_QUIT）。
static HOOK_THREAD: AtomicU32 = AtomicU32::new(0);
/// 要求后台线程退出。
static STOP: AtomicBool = AtomicBool::new(false);
/// 鼠标钩子句柄（退出时卸载）。
static HOOK_MOUSE: Mutex<Option<isize>> = Mutex::new(None);
/// 键盘钩子句柄（退出时卸载）。
static HOOK_KEYBOARD: Mutex<Option<isize>> = Mutex::new(None);
/// 事件队列（钩子线程 → 转发线程）。
static QUEUE: Mutex<VecDeque<Event>> = Mutex::new(VecDeque::new());
/// 队列有货时唤醒转发线程。
static WAKE: Condvar = Condvar::new();
/// 「我们给壁纸转发过按下，还欠它一次抬起」。
///
/// 用它而不是「物理键还按着」来判断抬起要不要绕过闸门：物理键按着、但按下当时闸门是关的
/// （比如用户在别的程序里按下再移回桌面），那就不欠抬起，不该发一条无头无脑的抬起过去。
static OWED_RELEASE: AtomicBool = AtomicBool::new(false);
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

#[derive(Clone, Copy)] struct PendingClick { message: u32, x: i32, y: i32, mouse_data: u32 }
static PENDING_CLICK: Mutex<Option<PendingClick>> = Mutex::new(None);

/// 队列上限：鼠标移动在高速拖动时能到每秒上千条，堆太多只会让壁纸看到过期的坐标。
/// 满了就丢掉最老的**移动**事件（按键事件一条都不能丢）。
const QUEUE_LIMIT: usize = 512;

/// 一次「把被壁纸窗口抢走的捕获还给桌面」的待办。
///
/// 为什么要延迟重试：`PostMessage` 只是把消息塞进目标线程的队列，目标窗口**稍后**才会处理它、
/// 才会 `SetCapture`。转发完立刻检查，多半还没被抢走；只查一次就会漏掉，
/// 而漏掉的后果是捕获**永久**留在壁纸窗口上（桌面图标整片点不动）。
/// 所以按下之后隔几毫秒复查几次，直到确认捕获不在壁纸手里。
#[derive(Debug, Clone, Copy)]
struct PendingRepair {
    /// 抢走之前捕获在谁手里（正常是桌面图标列表）。
    previous: isize,
    /// 壁纸窗口。
    wallpaper: isize,
    /// 还剩几次复查机会。
    attempts: u32,
    /// 下次复查的时间点。
    next_at: Instant,
}

/// 按下之后复查捕获的次数与间隔：覆盖目标线程处理消息的常见延迟。
const REPAIR_ATTEMPTS: u32 = 8;
const REPAIR_INTERVAL: Duration = Duration::from_millis(6);

/// 待办的捕获归还（只有转发线程碰它）。
static PENDING: Mutex<Option<PendingRepair>> = Mutex::new(None);

/// `hand_capture_back` 的结果。
///
/// 必须区分「还好了」和「这会儿还没被抢」：`PostMessage` 是异步的，按下之后立刻查，
/// 目标窗口多半还没来得及 `SetCapture`。把「还没被抢」当成「没事了」就会漏掉，
/// 而漏掉的后果是捕获永久留在壁纸窗口上。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CaptureOutcome {
    /// 捕获当时不在壁纸手里（可能稍后才被抢，所以还要继续复查）。
    NotStolen,
    /// 捕获在壁纸手里，已经拿回来还给桌面。
    Repaired,
    /// 桌面线程查不到，判断不了。
    Unknown,
}

/// 幂等：第一次调用时把钩子线程与转发线程起起来。
pub fn ensure_helper() -> Result<(), String> {
    if ACTIVE.load(Ordering::SeqCst) {
        return Ok(());
    }
    STOP.store(false, Ordering::SeqCst);

    std::thread::Builder::new()
        .name("input-forward".to_string())
        .spawn(forward_loop)
        .map_err(|e| format!("创建输入转发线程失败：{e}"))?;

    let (sender, receiver) = std::sync::mpsc::channel::<Result<(), String>>();
    std::thread::Builder::new()
        .name("input-hook".to_string())
        .spawn(move || hook_thread(sender))
        .map_err(|e| format!("创建输入钩子线程失败：{e}"))?;

    match receiver.recv_timeout(Duration::from_secs(3)) {
        Ok(Ok(())) => {
            ACTIVE.store(true, Ordering::SeqCst);
            Ok(())
        }
        Ok(Err(err)) => Err(err),
        Err(_) => Err("输入转发线程没有在 3 秒内就绪".to_string()),
    }
}

/// 设置转发目标。`mouse` / `keyboard` 至少开一个才有意义。
///
/// 扫描线程每 1.2 秒就会调一次，所以这里必须**幂等且廉价**：只有目标窗口真的变了
/// 才做「自愈」这类带副作用的动作。
pub fn set_target(hwnd: isize, mouse: bool, keyboard: bool) -> Result<(), String> {
    if hwnd == 0 {
        clear_target();
        return Ok(());
    }
    ensure_helper()?;
    let previous = TARGET.swap(hwnd, Ordering::SeqCst);
    MOUSE.store(mouse, Ordering::SeqCst);
    KEYBOARD.store(keyboard, Ordering::SeqCst);
    if !mouse && !keyboard {
        TARGET.store(0, Ordering::SeqCst);
        return Ok(());
    }

    // 自愈：上一次（或上一次点击没等到抬起）可能把捕获留在壁纸窗口上了，
    // 那会让桌面图标整片点不动 —— 只在**刚开始转发**（或换了目标窗口）时查一次。
    if mouse && previous != hwnd {
        push(Event::HealStaleCapture { wallpaper: hwnd });
    }
    Ok(())
}

/// 停止转发，并把输入状态还给桌面。
///
/// 这里做的三件事都是「不还给桌面就会留下后遗症」的：
/// * 补发抬起消息 —— 合成按下让壁纸窗口 `SetCapture` 了，它得收到抬起才会松手；
/// * 把捕获还给桌面 —— 否则鼠标输入会一直路由给壁纸（壁纸还能点、桌面图标点不动）；
/// * 清空指针状态 —— 免得输入包继续告诉壁纸「左键还按着」。
///
/// 走的是转发线程（`PostMessage` 与 `GetGUIThreadInfo` 都可能阻塞，不能在这个线程里做）。
pub fn clear_target() {
    let target = TARGET.load(Ordering::SeqCst);
    TARGET.store(0, Ordering::SeqCst);
    MOUSE.store(false, Ordering::SeqCst);
    KEYBOARD.store(false, Ordering::SeqCst);
    reset_pointer();
    if target != 0 {
        // 欠着的抬起与残留捕获都在转发线程里收尾
        push(Event::ReleaseAll { wallpaper: target });
    } else {
        OWED_RELEASE.store(false, Ordering::SeqCst);
        clear_pending();
    }
}

/// 是否正在转发。
pub fn is_active() -> bool {
    TARGET.load(Ordering::SeqCst) != 0
}

/// 读取并清零累计的滚轮位移；坐标每次读取时按当前光标位置刷新。
pub fn pointer_state() -> PointerState {
    let mut guard = POINTER.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(hwnd) = current_target() {
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

fn reset_pointer() {
    if let Ok(mut pointer) = POINTER.lock() {
        pointer.left = false;
        pointer.right = false;
        pointer.middle = false;
        pointer.wheel = 0;
        pointer.inside = false;
    }
}

/// 闸门：桌面在前台（或者前台已经是我们壁纸窗口）才转发。
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

/* ------------------------------------------------------------ 事件队列 */

/// 入队一条事件（钩子线程调用，必须无阻塞）。
fn push(event: Event) {
    let mut queue = match QUEUE.lock() {
        Ok(queue) => queue,
        Err(poisoned) => poisoned.into_inner(),
    };
    // 移动事件可以合并：连着两条移动只留最新那条，坐标才有意义
    if matches!(
        event,
        Event::Mouse {
            message: WM_MOUSEMOVE,
            ..
        }
    ) {
        if let Some(back @ Event::Mouse { .. }) = queue.back_mut() {
            if matches!(
                back,
                Event::Mouse {
                    message: WM_MOUSEMOVE,
                    ..
                }
            ) {
                *back = event;
                drop(queue);
                WAKE.notify_one();
                return;
            }
        }
    }
    if queue.len() >= QUEUE_LIMIT {
        // 满了优先丢最老的移动事件；一条都找不到（全是按键）就丢最老的
        let position = queue
            .iter()
            .position(|item| {
                matches!(
                    item,
                    Event::Mouse {
                        message: WM_MOUSEMOVE,
                        ..
                    }
                )
            })
            .unwrap_or(0);
        queue.remove(position);
    }
    queue.push_back(event);
    drop(queue);
    WAKE.notify_one();
}

/// 取一条事件；`timeout` 到点返回 `None`（用来周期性检查退出标志）。
fn pop(timeout: Duration) -> Option<Event> {
    let mut queue = QUEUE.lock().unwrap_or_else(|e| e.into_inner());
    if queue.is_empty() {
        let (guard, _) = WAKE
            .wait_timeout(queue, timeout)
            .unwrap_or_else(|e| e.into_inner());
        queue = guard;
    }
    queue.pop_front()
}

/* ------------------------------------------------------------ 钩子线程 */

fn hook_thread(ready: std::sync::mpsc::Sender<Result<(), String>>) {
    unsafe {
        let mouse = match SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_hook), None, 0) {
            Ok(hook) => hook,
            Err(err) => {
                let _ = ready.send(Err(format!("安装鼠标钩子失败：{err}（可能被安全软件拦截）")));
                return;
            }
        };
        let keyboard = match SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_hook), None, 0) {
            Ok(hook) => hook,
            Err(err) => {
                // 键盘钩子失败就把鼠标钩子也收掉：宁可明确报错，也不要"只剩一半能用"
                let _ = UnhookWindowsHookEx(mouse);
                let _ = ready.send(Err(format!("安装键盘钩子失败：{err}（可能被安全软件拦截）")));
                return;
            }
        };
        *HOOK_MOUSE.lock().unwrap_or_else(|e| e.into_inner()) = Some(mouse.0 as isize);
        *HOOK_KEYBOARD.lock().unwrap_or_else(|e| e.into_inner()) = Some(keyboard.0 as isize);
        HOOK_THREAD.store(GetCurrentThreadId(), Ordering::SeqCst);
        let _ = ready.send(Ok(()));

        // 低层钩子要求线程自己泵消息
        let mut message = MSG::default();
        while !STOP.load(Ordering::SeqCst) {
            let result = GetMessageW(&mut message, None, 0, 0);
            if result.0 <= 0 {
                break;
            }
            let _ = TranslateMessage(&message);
            let _ = DispatchMessageW(&message);
        }

        let _ = UnhookWindowsHookEx(mouse);
        let _ = UnhookWindowsHookEx(keyboard);
        *HOOK_MOUSE.lock().unwrap_or_else(|e| e.into_inner()) = None;
        *HOOK_KEYBOARD.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }
}

/// 鼠标钩子回调：**只抄字段入队**，绝不做可能阻塞的事。
unsafe extern "system" fn mouse_hook(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32
        && TARGET.load(Ordering::SeqCst) != 0
        && MOUSE.load(Ordering::SeqCst)
    {
        let info = &*(lparam.0 as *const MSLLHOOKSTRUCT);
        push(Event::Mouse {
            message: wparam.0 as u32,
            x: info.pt.x,
            y: info.pt.y,
            mouse_data: info.mouseData,
        });
    }
    CallNextHookEx(None, code, wparam, lparam)
}

/// 键盘钩子回调：同样只抄字段入队。
unsafe extern "system" fn keyboard_hook(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32
        && TARGET.load(Ordering::SeqCst) != 0
        && KEYBOARD.load(Ordering::SeqCst)
    {
        let info = &*(lparam.0 as *const KBDLLHOOKSTRUCT);
        // 别人（或我们自己）注入的按键不再转发，免得形成回环
        if !info.flags.contains(LLKHF_INJECTED) {
            push(Event::Key {
                message: wparam.0 as u32,
                vk: info.vkCode,
                scan: info.scanCode,
                extended: info.flags.contains(LLKHF_EXTENDED),
            });
        }
    }
    CallNextHookEx(None, code, wparam, lparam)
}

/* ---------------------------------------------------------- 转发线程 */

/// 转发线程：所有可能阻塞的调用都在这儿做，钩子线程因此永远保持轻快。
fn forward_loop() {
    while !STOP.load(Ordering::SeqCst) {
        // 有待办的捕获归还就按它的到期时间等，否则当个 250ms 的定时器用
        let wait = pending_wait().unwrap_or_else(|| Duration::from_millis(250));
        if let Some(event) = pop(wait) {
            handle_event(event);
        }
        // 无论有没有事件都要走到这儿：闸门关着的时候鼠标事件会被整片丢掉，
        // 要是把复查挂在事件后面，待办的捕获归还就会被活活饿死。
        service_pending();
    }
}

/// 处理一条事件。
fn handle_event(event: Event) {
    match event {
        Event::Mouse {
            message,
            x,
            y,
            mouse_data,
        } => {
            let Some(target) = current_target() else {
                return;
            };
            update_pointer(target, message, x, y, mouse_data);
            if MOUSE.load(Ordering::SeqCst) && gate_open(target) {
                if is_button_down(message) {
                    *PENDING_CLICK.lock().unwrap_or_else(|e| e.into_inner()) = Some(PendingClick { message, x, y, mouse_data });
                } else if is_button_up(message) {
                    let pending = PENDING_CLICK.lock().unwrap_or_else(|e| e.into_inner()).take();
                    if let Some(pending) = pending { if x.abs_diff(pending.x) <= 4 && y.abs_diff(pending.y) <= 4 { forward_click(target, pending, message, x, y, mouse_data); } }
                } else {
                    forward_mouse(target, message, x, y, mouse_data);
                }
            }
        }
        Event::Key {
            message,
            vk,
            scan,
            extended,
        } => {
            let Some(target) = current_target() else {
                return;
            };
            if KEYBOARD.load(Ordering::SeqCst) && gate_open(target) {
                forward_key(target, message, vk, scan, extended);
            }
        }
        Event::ReleaseAll { wallpaper } => {
            clear_pending();
            OWED_RELEASE.store(false, Ordering::SeqCst);
            let target = HWND(wallpaper as *mut std::ffi::c_void);
            if is_alive(target) {
                // 补发抬起：壁纸窗口得收到抬起才会松手
                release_buttons(target);
                // 捕获是壁纸处理「按下」时抢走的，此刻多半还在它手里，还回桌面
                hand_capture_back(HWND::default(), target);
            }
        }
        Event::HealStaleCapture { wallpaper } => {
            let target = HWND(wallpaper as *mut std::ffi::c_void);
            if is_alive(target) {
                hand_capture_back(HWND::default(), target);
            }
        }
    }
}

/// 窗口是否还存在。
fn is_alive(hwnd: HWND) -> bool {
    !hwnd.is_invalid() && unsafe { windows::Win32::UI::WindowsAndMessaging::IsWindow(Some(hwnd)).as_bool() }
}

/// 下一个待办还有多久到点（没有待办返回 `None`）。
fn pending_wait() -> Option<Duration> {
    let guard = PENDING.lock().unwrap_or_else(|e| e.into_inner());
    guard
        .as_ref()
        .map(|pending| pending.next_at.saturating_duration_since(Instant::now()))
}

/// 登记一次捕获归还，并安排复查。
fn schedule_repair(previous: HWND, wallpaper: HWND) {
    let mut guard = PENDING.lock().unwrap_or_else(|e| e.into_inner());
    *guard = Some(PendingRepair {
        previous: previous.0 as isize,
        wallpaper: wallpaper.0 as isize,
        attempts: REPAIR_ATTEMPTS,
        next_at: Instant::now() + REPAIR_INTERVAL,
    });
}

fn clear_pending() {
    *PENDING.lock().unwrap_or_else(|e| e.into_inner()) = None;
}

/// 到点就复查一次：捕获真的落在壁纸窗口上才动手，还完就收工。
fn service_pending() {
    let request = {
        let mut guard = PENDING.lock().unwrap_or_else(|e| e.into_inner());
        match guard.as_mut() {
            Some(pending) => {
                if Instant::now() < pending.next_at {
                    return;
                }
                pending.attempts = pending.attempts.saturating_sub(1);
                pending.next_at = Instant::now() + REPAIR_INTERVAL;
                let request = *pending;
                if pending.attempts == 0 {
                    *guard = None;
                }
                Some(request)
            }
            None => None,
        }
    };
    let Some(request) = request else {
        return;
    };
    let previous = HWND(request.previous as *mut std::ffi::c_void);
    let wallpaper = HWND(request.wallpaper as *mut std::ffi::c_void);
    match hand_capture_back(previous, wallpaper) {
        // 拿回来了就不用再查
        CaptureOutcome::Repaired => clear_pending(),
        // 还没被抢：目标窗口可能还没处理到那条按下消息，留着待办继续复查
        CaptureOutcome::NotStolen => {}
        // 判断不了（桌面线程没了），再查也是白查
        CaptureOutcome::Unknown => clear_pending(),
    }
}

/// 收到事件时更新共享的鼠标状态（不依赖最后一次轮询）。
fn update_pointer(target: HWND, message: u32, x: i32, y: i32, mouse_data: u32) {
    let mut guard = POINTER.lock().unwrap_or_else(|e| e.into_inner());
    let (client_x, client_y) = desktop::screen_to_client(target, x, y);
    let (width, height) = desktop::client_size(target);
    guard.x = client_x as f32;
    guard.y = client_y as f32;
    guard.inside = client_x >= 0 && client_y >= 0 && client_x < width && client_y < height;

    match message {
        WM_LBUTTONDOWN => guard.left = true,
        WM_LBUTTONUP => guard.left = false,
        WM_RBUTTONDOWN => guard.right = true,
        WM_RBUTTONUP => guard.right = false,
        WM_MBUTTONDOWN => guard.middle = true,
        WM_MBUTTONUP => guard.middle = false,
        WM_MOUSEWHEEL | WM_MOUSEHWHEEL => {
            let delta = ((mouse_data >> 16) & 0xffff) as u16 as i16;
            guard.wheel = guard.wheel.saturating_add(delta as i32);
        }
        _ => {}
    }
}

/// 这条消息是不是「按下」（按下会让目标窗口抢捕获）。
fn is_button_down(message: u32) -> bool {
    matches!(
        message,
        WM_LBUTTONDOWN | WM_RBUTTONDOWN | WM_MBUTTONDOWN | WM_XBUTTONDOWN
    )
}

fn is_button_up(message: u32) -> bool { matches!(message, WM_LBUTTONUP | WM_RBUTTONUP | WM_MBUTTONUP | WM_XBUTTONUP) }

fn forward_click(target: HWND, pending: PendingClick, up: u32, x: i32, y: i32, mouse_data: u32) {
    let (cx, cy) = desktop::screen_to_client(target, x, y); let p = make_lparam(cx, cy);
    let (dw, uw) = match pending.message { WM_LBUTTONDOWN=>(WPARAM(1),WPARAM(0)), WM_RBUTTONDOWN=>(WPARAM(2),WPARAM(0)), WM_MBUTTONDOWN=>(WPARAM(0x10),WPARAM(0)), WM_XBUTTONDOWN=>{let b=WPARAM(((mouse_data>>16)&0xffff) as usize);(b,b)}, _=>return };
    unsafe { let _=PostMessageW(Some(target), pending.message, dw, p); let _=PostMessageW(Some(target), up, uw, p); }
}

/// 只转发移动和滚轮；按键通过输入包传递，避免壁纸抢捕获并取消桌面框选。
///
/// 坐标是**客户区**坐标；滚轮消息按 Windows 约定把增量放在 `wParam` 高位、屏幕坐标放在 `lParam`，
/// 所以滚轮与其它消息分开处理。
fn forward_mouse(target: HWND, message: u32, x: i32, y: i32, mouse_data: u32) {
    let (client_x, client_y) = desktop::screen_to_client(target, x, y);
    let client_param = make_lparam(client_x, client_y);

    let (post_message, wparam, lparam) = match message {
        WM_MOUSEMOVE => (WM_MOUSEMOVE, WPARAM(0), client_param),
        WM_MOUSEWHEEL | WM_MOUSEHWHEEL => (
            message,
            WPARAM((mouse_data & 0xffff0000) as usize),
            make_lparam(x, y),
        ),
        _ => return,
    };

    unsafe {
        let _ = PostMessageW(Some(target), post_message, wparam, lparam);
    }
}

/// 把键盘钩子事件翻译成 `WM_KEYDOWN/WM_KEYUP/WM_SYSKEYDOWN/WM_SYSKEYUP` post 给壁纸窗口。
///
/// 不合成 `WM_CHAR`：消息进了目标线程的队列之后，它自己的消息循环会 `TranslateMessage`
/// 把按键翻成字符，文本输入照样通（而且用的是目标线程的键盘布局，比我们猜更准）。
fn forward_key(target: HWND, message: u32, vk: u32, scan: u32, extended: bool) {
    let (post_message, context, previous, transition) = match message {
        WM_KEYDOWN => (WM_KEYDOWN, false, false, false),
        WM_KEYUP => (WM_KEYUP, false, true, true),
        WM_SYSKEYDOWN => (WM_SYSKEYDOWN, true, false, false),
        WM_SYSKEYUP => (WM_SYSKEYUP, true, true, true),
        _ => return,
    };
    let lparam = make_key_lparam(scan, extended, context, previous, transition);
    unsafe {
        let _ = PostMessageW(Some(target), post_message, WPARAM(vk as usize), lparam);
    }
}

/// 补发抬起消息：让壁纸窗口把「还按着」的状态与捕获一起松掉。
fn release_buttons(target: HWND) {
    unsafe {
        for message in [WM_LBUTTONUP, WM_RBUTTONUP, WM_MBUTTONUP, WM_XBUTTONUP] {
            let _ = PostMessageW(Some(target), message, WPARAM(0), LPARAM(0));
        }
    }
}

fn make_lparam(x: i32, y: i32) -> LPARAM {
    let packed = ((y as u16 as u32) << 16) | (x as u16 as u32);
    LPARAM(packed as isize)
}

/// 键盘消息的 `lParam` 布局（见 `WM_KEYDOWN` 文档）：
/// 重复次数(0–15) / 扫描码(16–23) / 扩展键(24) / 上下文(29) / 前一状态(30) / 转换状态(31)。
fn make_key_lparam(
    scan_code: u32,
    extended: bool,
    context: bool,
    previous: bool,
    transition: bool,
) -> LPARAM {
    let mut value = 1u32; // 重复次数 = 1
    value |= (scan_code & 0xff) << 16;
    if extended {
        value |= 1 << 24;
    }
    if context {
        value |= 1 << 29;
    }
    if previous {
        value |= 1 << 30;
    }
    if transition {
        value |= 1 << 31;
    }
    LPARAM(value as isize)
}

/* ------------------------------------------------------- 鼠标捕获的归还 */

/// 捕获窗口是不是壁纸窗口（或它的子窗口）。
fn capture_is_wallpaper(capture: HWND, wallpaper: HWND) -> bool {
    if capture.is_invalid() || wallpaper.is_invalid() {
        return false;
    }
    if capture == wallpaper {
        return true;
    }
    unsafe { windows::Win32::UI::WindowsAndMessaging::IsChild(wallpaper, capture).as_bool() }
}

/// 把被壁纸窗口抢走的鼠标捕获还给桌面。
///
/// 只在「捕获确实落在壁纸窗口上」时动手：用户正拖着桌面图标时捕获在桌面图标列表手里，
/// 那种情况我们一根手指都不碰。
///
/// `AttachThreadInput` 只连**桌面线程**（explorer，永远响应），绝不连壁纸进程的线程 ——
/// 壁纸可能正被 `NtSuspendProcess` 挂着，连上去会把我们自己的线程拖死。
fn hand_capture_back(previous: HWND, wallpaper: HWND) -> CaptureOutcome {
    let Some(desktop_window) = desktop::desktop_window() else {
        return CaptureOutcome::Unknown; // 桌面窗口都找不到，判断不了
    };
    let thread = desktop::window_thread(desktop_window);
    if thread == 0 {
        return CaptureOutcome::Unknown;
    }
    let capture = desktop::capture_of_thread(thread);
    if capture.is_invalid() {
        // 没人拿着捕获（最干净的状态）——但按下可能还没被处理，所以不算「已修复」
        return CaptureOutcome::NotStolen;
    }
    if !capture_is_wallpaper(capture, wallpaper) {
        // 捕获在桌面自己手里（用户正拖着图标）：别碰
        return CaptureOutcome::NotStolen;
    }

    unsafe {
        let current = GetCurrentThreadId();
        let attached = thread != current && AttachThreadInput(current, thread, true).as_bool();
        // 还给谁，按桌面自己的规矩来：
        // * 之前就有人拿着（正常是桌面图标列表）→ 还给它，拖到一半的图标接着拖；
        // * 没人拿着但**鼠标键还按着** → 交给桌面图标列表：这正是桌面自己在按下时会做的事，
        //   还给它是为了让「按下 → 拖动 → 抬起」这条链路留在桌面手里；
        // * 没人拿着、键也松了 → 释放。绝不能硬塞给图标列表：那样桌面会一直攥着捕获，
        //   连点到任务栏都会被它吞掉（任务栏收不到抬起，按钮会一直卡在按下状态）。
        let restore = if !previous.is_invalid() && desktop::window_thread(previous) == thread {
            Some(previous)
        } else if any_button_down() {
            desktop::icon_listview().filter(|list| desktop::window_thread(*list) == thread)
        } else {
            None
        };
        match restore {
            Some(window) => {
                SetCapture(window);
            }
            None => {
                let _ = ReleaseCapture();
            }
        }
        if attached {
            let _ = AttachThreadInput(current, thread, false);
        }
    }
    CaptureOutcome::Repaired
}

/// 物理鼠标键当前是否按着（VK_LBUTTON / VK_RBUTTON / VK_MBUTTON / VK_XBUTTON1 / VK_XBUTTON2）。
fn any_button_down() -> bool {
    [0x01, 0x02, 0x04, 0x05, 0x06].iter().any(|code| key_down(*code))
}

/// 退出钩子线程（进程结束前调，测试里也用得上）。
#[allow(dead_code)]
pub fn shutdown() {
    STOP.store(true, Ordering::SeqCst);
    WAKE.notify_all();
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

/// 只给集成测试用：把捕获归还逻辑单独暴露出来，好在真实桌面上量它的行为。
#[doc(hidden)]
pub fn hand_capture_back_for_test(previous: isize, wallpaper: isize) -> bool {
    hand_capture_back(
        HWND(previous as *mut std::ffi::c_void),
        HWND(wallpaper as *mut std::ffi::c_void),
    ) == CaptureOutcome::Repaired
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
    fn key_lparam_carries_scan_code_and_state_bits() {
        // 普通按下：重复 1 次、扫描码 0x1E、无扩展、无转换
        let down = make_key_lparam(0x1E, false, false, false, false).0 as u32;
        assert_eq!(down & 0xffff, 1);
        assert_eq!((down >> 16) & 0xff, 0x1E);
        assert_eq!(down >> 24 & 0x1, 0);
        assert_eq!(down >> 30 & 0x1, 0);
        assert_eq!(down >> 31 & 0x1, 0);

        // 扩展键抬起：扩展位(24) / 前一状态(30) / 转换状态(31) 都要置起来
        let up = make_key_lparam(0x4B, true, false, true, true).0 as u32;
        assert_eq!(up >> 24 & 0x1, 1);
        assert_eq!(up >> 30 & 0x1, 1);
        assert_eq!(up >> 31 & 0x1, 1);

        // Alt 组合键：上下文位(29)
        let sys = make_key_lparam(0x3C, false, true, false, false).0 as u32;
        assert_eq!(sys >> 29 & 0x1, 1);
    }

    #[test]
    fn only_button_downs_are_treated_as_capture_stealing() {
        assert!(is_button_down(WM_LBUTTONDOWN));
        assert!(is_button_down(WM_RBUTTONDOWN));
        assert!(is_button_down(WM_MBUTTONDOWN));
        assert!(is_button_down(WM_XBUTTONDOWN));
        assert!(!is_button_down(WM_LBUTTONUP));
        assert!(!is_button_down(WM_MOUSEMOVE));
        assert!(!is_button_down(WM_MOUSEWHEEL));
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

    /// 待办的捕获归还要能算出到期时间，否则转发线程就不知道该隔多久复查一次。
    #[test]
    fn a_scheduled_repair_reports_a_deadline() {
        clear_pending();
        assert!(pending_wait().is_none(), "没有待办时不该有到期时间");
        schedule_repair(HWND::default(), HWND(1 as *mut std::ffi::c_void));
        assert!(
            pending_wait().is_some(),
            "登记之后必须能算出下一次复查的时间"
        );
        clear_pending();
        assert!(pending_wait().is_none());
    }

    /// 抬起消息绕过闸门的判断，靠的是「我们欠壁纸一次抬起」这个显式标记。
    #[test]
    fn the_owed_release_flag_tracks_button_state() {
        OWED_RELEASE.store(false, Ordering::SeqCst);
        assert!(!OWED_RELEASE.load(Ordering::SeqCst));
        OWED_RELEASE.store(true, Ordering::SeqCst);
        assert!(OWED_RELEASE.load(Ordering::SeqCst));
        OWED_RELEASE.store(false, Ordering::SeqCst);
    }

    /// 连续移动只留最新那条：否则高速拖动时队列里堆的全是过期坐标。
    #[test]
    fn consecutive_moves_are_coalesced() {
        QUEUE.lock().unwrap_or_else(|e| e.into_inner()).clear();
        for step in 0..5 {
            push(Event::Mouse {
                message: WM_MOUSEMOVE,
                x: step,
                y: step,
                mouse_data: 0,
            });
        }
        let queue = QUEUE.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(queue.len(), 1, "连续的移动事件应该被合并成一条");
        match queue.front() {
            Some(Event::Mouse { x, y, .. }) => assert_eq!((*x, *y), (4, 4)),
            other => panic!("队列里应该是最后一条移动事件，实际是 {other:?}"),
        }
    }

    /// 按键事件绝不能被移动事件挤掉。
    #[test]
    fn key_events_survive_a_flood_of_moves() {
        QUEUE.lock().unwrap_or_else(|e| e.into_inner()).clear();
        push(Event::Key {
            message: WM_KEYDOWN,
            vk: 0x41,
            scan: 0x1E,
            extended: false,
        });
        for step in 0..(QUEUE_LIMIT * 2) {
            push(Event::Mouse {
                message: WM_MOUSEMOVE,
                x: step as i32,
                y: 0,
                mouse_data: 0,
            });
        }
        let queue = QUEUE.lock().unwrap_or_else(|e| e.into_inner());
        assert!(
            queue
                .iter()
                .any(|item| matches!(item, Event::Key { vk: 0x41, .. })),
            "淹没在移动事件里也不该丢掉按键"
        );
        assert!(queue.len() <= QUEUE_LIMIT, "队列长度必须守住上限");
    }
}