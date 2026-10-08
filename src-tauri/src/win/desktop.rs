//! WorkerW 桌面层：发现壁纸层、把外部 exe 的窗口挂上去、算几何、列显示器。
//!
//! 关键点（都是踩过的坑）：
//! * WorkerW 不是一直都在，先给 Progman 发 `0x052C` 让它把图层生出来；
//! * Windows 11 24H2 起 WorkerW 变成 Progman 的**子窗口**，不再与 `SHELLDLL_DefView` 平级，
//!   两条路都要试；
//! * `SetParent` 之后窗口坐标变成相对**父窗口客户区**，必须换算，否则壁纸会跑到屏幕外；
//! * 挂上去的窗口要去掉标题栏 / 边框 / `WS_EX_APPWINDOW`，否则任务栏和 Alt-Tab 里会多一个条目。

use std::ffi::c_void;
use std::sync::Mutex;

use serde::Serialize;
use windows::core::{w, BOOL, PCWSTR};
use windows::Win32::Foundation::{
    GetLastError, HWND, LPARAM, POINT, RECT, TRUE, WPARAM,
};
use windows::Win32::Graphics::Dwm::{
    DwmSetWindowAttribute, DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_DONOTROUND,
};
use windows::Win32::Graphics::Gdi::{
    ClientToScreen, CreateCompatibleBitmap, CreateCompatibleDC, DeleteDC, DeleteObject,
    EnumDisplayMonitors, GetDC, GetDIBits, GetMonitorInfoW, MonitorFromWindow, ReleaseDC,
    ScreenToClient, SelectObject, SetStretchBltMode, StretchBlt, BITMAPINFO, BITMAPINFOHEADER,
    BI_RGB, DIB_RGB_COLORS, HALFTONE, HDC, HGDIOBJ, HMONITOR, MONITORINFOEXW,
    MONITOR_DEFAULTTONEAREST, SRCCOPY,
};
use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, FindWindowExW, FindWindowW, GetAncestor, GetClassNameW, GetClientRect,
    GetForegroundWindow, GetGUIThreadInfo, GetParent, GetShellWindow, GetSystemMetrics,
    GetWindowLongPtrW, GetWindowRect, GetWindowThreadProcessId, IsChild, IsIconic, IsWindow,
    IsWindowVisible, SendMessageTimeoutW, SetParent, SetWindowLongPtrW, SetWindowPos, ShowWindow,
    GA_ROOT, GUITHREADINFO, GWL_EXSTYLE, GWL_STYLE, HWND_TOP, SMTO_NORMAL, SM_CXVIRTUALSCREEN,
    SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN, SWP_FRAMECHANGED, SWP_NOACTIVATE,
    SWP_SHOWWINDOW, SW_SHOW, WS_CAPTION, WS_CHILD, WS_EX_APPWINDOW, WS_EX_NOACTIVATE,
    WS_EX_TOOLWINDOW, WS_MAXIMIZEBOX, WS_MINIMIZEBOX, WS_POPUP, WS_SYSMENU, WS_THICKFRAME,
};

// `windows` crate 把 `PrintWindow` 的元数据归到了 `Win32::Storage::Xps` 下（元数据归类如此），
// 为了一个符号去打开整个 Xps feature 不划算 —— 它本来就是 user32 的导出，自己声明即可。
#[link(name = "user32")]
extern "system" {
    fn PrintWindow(hwnd: HWND, hdc: HDC, nflags: u32) -> BOOL;
}

/// `PRINT_WINDOW_FLAGS` 的取值：让 DirectComposition 的内容也参与绘制。
const PW_RENDERFULLCONTENT: u32 = 2;

/// 让 Progman 生成 WorkerW 的私有消息。
const WM_SPAWN_WORKER: u32 = 0x052C;

/// 一个显示器的信息（契约 2.5）。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MonitorInfo {
    pub index: i32,
    pub name: String,
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
    pub primary: bool,
    pub scale_factor: f32,
}

/// 记录上一次挂载的窗口，供重挂时判断。
static ATTACHED: Mutex<Option<isize>> = Mutex::new(None);

fn last_error(context: &str) -> String {
    let code = unsafe { GetLastError() };
    format!("{context} 失败（Win32 错误 {}）", code.0)
}

/// `FindWindowExW` 的薄包装：窗口名一律传空，只按类名找。
fn find_ex(parent: Option<HWND>, after: Option<HWND>, class: PCWSTR) -> HWND {
    unsafe { FindWindowExW(parent, after, class, PCWSTR::null()).unwrap_or_default() }
}

/* ------------------------------------------------------------- WorkerW */

/// 找到桌面壁纸层（WorkerW）。找不到时返回 `None`，调用方可以过一会儿再试。
pub fn find_worker_w() -> Option<HWND> {
    unsafe {
        let progman = FindWindowW(w!("Progman"), PCWSTR::null()).ok()?;

        for attempt in 0..3 {
            if attempt > 0 {
                std::thread::sleep(std::time::Duration::from_millis(120));
            }

            // 让 Progman 把 WorkerW 生成出来（已存在时是空操作）
            let _ = SendMessageTimeoutW(
                progman,
                WM_SPAWN_WORKER,
                WPARAM(0xD),
                LPARAM(0x1),
                SMTO_NORMAL,
                1000,
                None,
            );

            let mut found: HWND = HWND::default();
            let _ = EnumWindows(
                Some(enum_for_worker),
                LPARAM(&mut found as *mut HWND as isize),
            );
            if !found.is_invalid() {
                return Some(found);
            }

            // Windows 11 24H2+：WorkerW 成了 Progman 的子窗口
            let child = find_ex(Some(progman), Some(HWND::default()), w!("WorkerW"));
            if !child.is_invalid() {
                return Some(child);
            }
        }
        None
    }
}

unsafe extern "system" fn enum_for_worker(window: HWND, reference: LPARAM) -> BOOL {
    // 有 SHELLDLL_DefView 子窗口的那个顶层窗口，它后面紧跟着的兄弟 WorkerW 就是壁纸层
    let view = find_ex(Some(window), Some(HWND::default()), w!("SHELLDLL_DefView"));
    if !view.is_invalid() {
        let worker = find_ex(Some(HWND::default()), Some(window), w!("WorkerW"));
        if !worker.is_invalid() {
            *(reference.0 as *mut HWND) = worker;
        }
    }
    TRUE
}

/* --------------------------------------------------------------- 挂载 */

/// 把外部进程的窗口挂到桌面壁纸层，并铺满指定显示器（`-1` = 整个虚拟桌面）。
pub fn attach(hwnd: HWND, monitor_index: i32) -> Result<(), String> {
    if hwnd.is_invalid() {
        return Err("壁纸窗口还没出现，稍后再试".to_string());
    }
    let worker = find_worker_w().ok_or_else(|| "找不到桌面壁纸层 WorkerW".to_string())?;

    unsafe {
        // 1. 变成无边框子窗口，并从任务栏 / Alt-Tab 里藏起来
        let style = GetWindowLongPtrW(hwnd, GWL_STYLE);
        let stripped = (style
            & !((WS_CAPTION.0
                | WS_THICKFRAME.0
                | WS_SYSMENU.0
                | WS_POPUP.0
                | WS_MINIMIZEBOX.0
                | WS_MAXIMIZEBOX.0) as isize))
            | WS_CHILD.0 as isize;
        SetWindowLongPtrW(hwnd, GWL_STYLE, stripped);

        let ex_style = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
        // `WS_EX_NOACTIVATE`：壁纸窗口永远不许被激活。
        //
        // 这是「桌面图标点不动」这类问题的根：壁纸窗口一旦成了前台窗口，桌面图标层
        // （`SHELLDLL_DefView`）就不是激活窗口了 —— 单击选不中、拖不动、右键不出菜单，
        // 而且焦点会一直赖在壁纸窗口上。挂载时就把它标成不可激活，
        // 就算壁纸进程自己（或别的什么）想抢前台也抢不走。
        let ex_new = (ex_style & !(WS_EX_APPWINDOW.0 as isize))
            | WS_EX_TOOLWINDOW.0 as isize
            | WS_EX_NOACTIVATE.0 as isize;
        SetWindowLongPtrW(hwnd, GWL_EXSTYLE, ex_new);

        // 2. 挂到壁纸层
        SetParent(hwnd, Some(worker)).map_err(|e| format!("SetParent 失败：{e}"))?;

        // 3. Windows 11 的圆角会在壁纸层边缘留缝
        let preference = DWMWCP_DONOTROUND;
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_WINDOW_CORNER_PREFERENCE,
            &preference as *const _ as *const c_void,
            std::mem::size_of_val(&preference) as u32,
        );

        // 4. 铺满目标区域（坐标要换算成父窗口客户区）
        let rect = target_rect(monitor_index);
        let (x, y) = parent_client_offset(worker, rect.left, rect.top);
        SetWindowPos(
            hwnd,
            Some(HWND_TOP),
            x,
            y,
            rect.right - rect.left,
            rect.bottom - rect.top,
            SWP_NOACTIVATE | SWP_FRAMECHANGED | SWP_SHOWWINDOW,
        )
        .map_err(|e| format!("SetWindowPos 失败：{e}"))?;

        let _ = ShowWindow(hwnd, SW_SHOW);
    }

    *ATTACHED.lock().unwrap_or_else(|e| e.into_inner()) = Some(hwnd.0 as isize);
    Ok(())
}

/// 从壁纸层摘下来，恢复成普通的顶层窗口。
pub fn detach(hwnd: HWND) -> Result<(), String> {
    if hwnd.is_invalid() {
        return Ok(());
    }
    unsafe {
        SetParent(hwnd, None).map_err(|e| format!("SetParent(恢复) 失败：{e}"))?;

        let style = GetWindowLongPtrW(hwnd, GWL_STYLE);
        let restored = (style & !(WS_CHILD.0 as isize))
            | WS_CAPTION.0 as isize
            | WS_THICKFRAME.0 as isize
            | WS_SYSMENU.0 as isize
            | WS_MINIMIZEBOX.0 as isize
            | WS_MAXIMIZEBOX.0 as isize;
        SetWindowLongPtrW(hwnd, GWL_STYLE, restored);

        let ex_style = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
        // 摘下来就恢复成普通窗口：`WS_EX_NOACTIVATE` 一起清掉，否则点它不进前台
        let ex_new = (ex_style & !(WS_EX_TOOLWINDOW.0 as isize | WS_EX_NOACTIVATE.0 as isize))
            | WS_EX_APPWINDOW.0 as isize;
        SetWindowLongPtrW(hwnd, GWL_EXSTYLE, ex_new);

        // 从 WorkerW 的子窗口变回顶层窗口后，位置要重新放回可见区域
        let rect = target_rect(-1);
        SetWindowPos(
            hwnd,
            Some(HWND_TOP),
            rect.left + 40,
            rect.top + 40,
            1280,
            720,
            SWP_NOACTIVATE | SWP_FRAMECHANGED | SWP_SHOWWINDOW,
        )
        .ok();
    }
    *ATTACHED.lock().unwrap_or_else(|e| e.into_inner()) = None;
    Ok(())
}

/// 确保壁纸窗口带着「不可激活」标记。
///
/// 为什么要单独有个补救函数：`attach()` 只在**挂载那一刻**设过一次扩展样式。
/// 用户从旧版本升上来、或窗口是别的路径挂上去的，可能已经挂在 WorkerW 上了却没有这个标记，
/// 于是它照样能当前台窗口，桌面图标还是会被顶成非激活。每轮扫描补一次，成本是一次
/// `GetWindowLongPtr` / `SetWindowLongPtr`，可以忽略。
///
/// 返回是否**改动**了样式。
pub fn enforce_no_activate(hwnd: HWND) -> bool {
    if hwnd.is_invalid() {
        return false;
    }
    unsafe {
        let ex_style = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
        if ex_style & WS_EX_NOACTIVATE.0 as isize != 0 {
            return false;
        }
        SetWindowLongPtrW(
            hwnd,
            GWL_EXSTYLE,
            ex_style | WS_EX_NOACTIVATE.0 as isize,
        );
    }
    true
}

/// 记在案的目标窗口是否还挂在壁纸层上。
pub fn is_attached(hwnd: HWND) -> bool {
    if hwnd.is_invalid() || !unsafe { IsWindow(Some(hwnd)).as_bool() } {
        return false;
    }
    unsafe {
        let parent = GetParent(hwnd).unwrap_or_default();
        if parent.is_invalid() {
            return false;
        }
        // explorer 重启后 WorkerW 会被销毁重建，父窗口对不上就说明掉下来了
        match find_worker_w() {
            Some(worker) => parent == worker,
            None => false,
        }
    }
}

/// 上一次挂载的窗口（供扫描线程检查是否需要重挂）。
pub fn last_attached() -> Option<isize> {
    *ATTACHED.lock().unwrap_or_else(|e| e.into_inner())
}

/// 这个窗口是不是我们挂进桌面壁纸层的那个壁纸窗口（含它的子窗口）。
///
/// 「前台窗口铺满显示器」这种判定必须排掉壁纸自己：壁纸窗口本来就铺满整块屏，
/// 一旦焦点落到桌面/壁纸层，它会被算成"有个全屏应用"，
/// 于是壁纸被自己的画面暂停掉。
pub fn is_wallpaper_window(hwnd: HWND) -> bool {
    unsafe {
        if hwnd.is_invalid() {
            return false;
        }
        let Some(attached) = last_attached() else {
            return false;
        };
        let root = HWND(attached as *mut std::ffi::c_void);
        if hwnd == root {
            return true;
        }
        if IsChild(root, hwnd).as_bool() {
            return true;
        }
        // 前台可能是壁纸的子窗口：往上找到顶层再比一次
        let ancestor = GetAncestor(hwnd, GA_ROOT);
        !ancestor.is_invalid() && ancestor == root
    }
}

/* --------------------------------------------------------------- 几何 */

/// 目标区域的屏幕坐标：`index < 0` 表示整个虚拟桌面。
pub fn target_rect(index: i32) -> RECT {
    let monitors = monitors();
    if index >= 0 {
        if let Some(monitor) = monitors.iter().find(|item| item.index == index) {
            return RECT {
                left: monitor.x,
                top: monitor.y,
                right: monitor.x + monitor.width,
                bottom: monitor.y + monitor.height,
            };
        }
    }
    unsafe {
        let left = GetSystemMetrics(SM_XVIRTUALSCREEN);
        let top = GetSystemMetrics(SM_YVIRTUALSCREEN);
        let width = GetSystemMetrics(SM_CXVIRTUALSCREEN);
        let height = GetSystemMetrics(SM_CYVIRTUALSCREEN);
        RECT {
            left,
            top,
            right: left + width.max(1),
            bottom: top + height.max(1),
        }
    }
}

/// 屏幕坐标 → 父窗口客户区坐标。
///
/// `SetParent` 之后子窗口用的是父窗口客户区坐标系；WorkerW 一般铺满整个虚拟屏幕，
/// 但也会有偏移（多屏、负坐标），所以老老实实算一次客户区原点。
fn parent_client_offset(parent: HWND, screen_x: i32, screen_y: i32) -> (i32, i32) {
    unsafe {
        let mut origin = POINT { x: 0, y: 0 };
        let _ = ClientToScreen(parent, &mut origin);
        (screen_x - origin.x, screen_y - origin.y)
    }
}

/// 屏幕坐标 → 某个窗口的客户区坐标（输入转发要用）。
pub fn screen_to_client(hwnd: HWND, x: i32, y: i32) -> (i32, i32) {
    unsafe {
        let mut point = POINT { x, y };
        let _ = ScreenToClient(hwnd, &mut point);
        (point.x, point.y)
    }
}

/// 窗口客户区尺寸。
pub fn client_size(hwnd: HWND) -> (i32, i32) {
    unsafe {
        let mut rect = RECT::default();
        if GetClientRect(hwnd, &mut rect).is_ok() {
            (rect.right - rect.left, rect.bottom - rect.top)
        } else {
            (0, 0)
        }
    }
}

/// 把窗口挪到指定屏幕矩形（挂在壁纸层时坐标会自动换算成父窗口客户区）。
pub fn move_window(hwnd: HWND, rect: RECT) -> Result<(), String> {
    if hwnd.is_invalid() {
        return Err("窗口无效".to_string());
    }
    let (x, y) = unsafe {
        match GetParent(hwnd) {
            Ok(parent) if !parent.is_invalid() => parent_client_offset(parent, rect.left, rect.top),
            _ => (rect.left, rect.top),
        }
    };
    unsafe {
        SetWindowPos(
            hwnd,
            Some(HWND_TOP),
            x,
            y,
            rect.right - rect.left,
            rect.bottom - rect.top,
            SWP_NOACTIVATE,
        )
        .map_err(|e| format!("SetWindowPos 失败：{e}"))
    }
}

/* ----------------------------------------------------------- 显示器 */

/// 枚举显示器（顺序稳定：主屏排第一，其余按坐标排序）。
pub fn monitors() -> Vec<MonitorInfo> {
    let mut collected: Vec<MonitorInfo> = Vec::new();

    unsafe extern "system" fn callback(
        monitor: HMONITOR,
        _dc: HDC,
        _rect: *mut RECT,
        data: LPARAM,
    ) -> BOOL {
        let out = &mut *(data.0 as *mut Vec<MonitorInfo>);
        let mut info = MONITORINFOEXW::default();
        info.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
        if GetMonitorInfoW(monitor, &mut info.monitorInfo).as_bool() {
            let device = String::from_utf16_lossy(
                &info
                    .szDevice
                    .iter()
                    .take_while(|c| **c != 0)
                    .copied()
                    .collect::<Vec<u16>>(),
            );
            let mut dpi_x = 96u32;
            let mut dpi_y = 96u32;
            let _ = GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y);
            let rect = info.monitorInfo.rcMonitor;
            out.push(MonitorInfo {
                index: 0,
                name: device,
                x: rect.left,
                y: rect.top,
                width: rect.right - rect.left,
                height: rect.bottom - rect.top,
                primary: info.monitorInfo.dwFlags & 1 == 1,
                scale_factor: dpi_x as f32 / 96.0,
            });
        }
        TRUE
    }

    unsafe {
        let _ = EnumDisplayMonitors(
            None,
            None,
            Some(callback),
            LPARAM(&mut collected as *mut _ as isize),
        );
    }

    collected.sort_by(|a, b| {
        b.primary
            .cmp(&a.primary)
            .then_with(|| a.x.cmp(&b.x))
            .then_with(|| a.y.cmp(&b.y))
    });
    for (index, monitor) in collected.iter_mut().enumerate() {
        monitor.index = index as i32;
    }
    collected
}

/// 某个窗口当前所在（最近的）显示器。
pub fn monitor_of_window(hwnd: HWND) -> Option<MonitorInfo> {
    if hwnd.is_invalid() {
        return None;
    }
    unsafe {
        let monitor = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
        if monitor.is_invalid() {
            return None;
        }
        let mut info = MONITORINFOEXW::default();
        info.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
        if !GetMonitorInfoW(monitor, &mut info.monitorInfo).as_bool() {
            return None;
        }
        let rect = info.monitorInfo.rcMonitor;
        Some(MonitorInfo {
            index: -1,
            name: String::new(),
            x: rect.left,
            y: rect.top,
            width: rect.right - rect.left,
            height: rect.bottom - rect.top,
            primary: info.monitorInfo.dwFlags & 1 == 1,
            scale_factor: 1.0,
        })
    }
}

/* --------------------------------------------------- 窗口查找 / 桌面态 */

/// 按 PID 找它的主窗口：可见、有尺寸、不是工具窗口，取面积最大的那个。
pub fn find_main_window(pid: u32) -> Option<HWND> {
    if pid == 0 {
        return None;
    }
    let mut best: Option<(HWND, i64)> = None;
    let mut context = FindContext { pid, best: &mut best };
    unsafe {
        let _ = EnumWindows(Some(enum_main_window), LPARAM(&mut context as *mut _ as isize));
    }
    best.map(|(hwnd, _)| hwnd)
}

struct FindContext<'a> {
    pid: u32,
    best: &'a mut Option<(HWND, i64)>,
}

unsafe extern "system" fn enum_main_window(window: HWND, data: LPARAM) -> BOOL {
    let context = &mut *(data.0 as *mut FindContext);
    let mut pid = 0u32;
    GetWindowThreadProcessId(window, Some(&mut pid));
    if pid != context.pid {
        return TRUE;
    }
    if !IsWindowVisible(window).as_bool() || IsIconic(window).as_bool() {
        return TRUE;
    }
    let ex_style = GetWindowLongPtrW(window, GWL_EXSTYLE) as u32;
    if ex_style & WS_EX_TOOLWINDOW.0 != 0 {
        return TRUE;
    }

    let mut rect = RECT::default();
    if GetWindowRect(window, &mut rect).is_err() {
        return TRUE;
    }
    let area = ((rect.right - rect.left) as i64) * ((rect.bottom - rect.top) as i64);
    if area <= 0 {
        return TRUE;
    }
    let replace = match context.best {
        Some((_, best_area)) => area > *best_area,
        None => true,
    };
    if replace {
        *context.best = Some((window, area));
    }
    TRUE
}

/// 进程的主窗口出现了没有（Unity 启动要几秒，这里轮询等）。
pub fn wait_for_main_window(pid: u32, timeout: std::time::Duration) -> Option<HWND> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if let Some(hwnd) = find_main_window(pid) {
            return Some(hwnd);
        }
        if std::time::Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(std::time::Duration::from_millis(120));
    }
}

/// 桌面（Progman / WorkerW / 桌面图标层）现在是不是前台窗口。
///
/// 输入转发只在这个时候开闸：否则用户在任何程序里动鼠标，都会有一堆合成消息发给壁纸。
pub fn is_desktop_foreground() -> bool {
    unsafe {
        let foreground = GetForegroundWindow();
        if foreground.is_invalid() {
            return false;
        }
        // 桌面图标层（SHELLDLL_DefView）本身也可能是前台窗口
        let view = find_ex(Some(foreground), Some(HWND::default()), w!("SHELLDLL_DefView"));
        if !view.is_invalid() {
            return true;
        }

        let mut buffer = [0u16; 64];
        let length = GetClassNameW(foreground, &mut buffer);
        if length <= 0 {
            return false;
        }
        let class = String::from_utf16_lossy(&buffer[..length as usize]);
        matches!(class.as_str(), "Progman" | "WorkerW")
    }
}

/* ------------------------------------------------- 输入队列 / 鼠标捕获 */

/// 桌面图标列表（`SHELLDLL_DefView` 下的 `SysListView32`）。
///
/// 它是桌面鼠标输入的合法持有者：被壁纸窗口抢走的捕获要还，就该还给它 ——
/// 还给它等于「拖到一半的图标接着拖」，而单纯 `ReleaseCapture` 会把拖动打断。
pub fn icon_listview() -> Option<HWND> {
    unsafe {
        let mut defview: HWND = HWND::default();
        let _ = EnumWindows(
            Some(enum_for_defview),
            LPARAM(&mut defview as *mut HWND as isize),
        );
        if defview.is_invalid() {
            return None;
        }
        let list = find_ex(Some(defview), Some(HWND::default()), w!("SysListView32"));
        (!list.is_invalid()).then_some(list)
    }
}

/// 找「有 `SHELLDLL_DefView` 子窗口」的那个顶层窗口，记下那个子窗口。
unsafe extern "system" fn enum_for_defview(window: HWND, reference: LPARAM) -> BOOL {
    let view = find_ex(Some(window), Some(HWND::default()), w!("SHELLDLL_DefView"));
    if !view.is_invalid() {
        *(reference.0 as *mut HWND) = view;
        return BOOL(0);
    }
    TRUE
}

/// 桌面外壳窗口（Progman 或挂图标层的 WorkerW）。
///
/// 它的线程就是 explorer 的桌面线程 —— 桌面图标列表、`SetCapture` 都归这条输入队列管。
/// 拿不到 `Progman` 时依次退回图标层 WorkerW 与 `GetShellWindow()`。
pub fn desktop_window() -> Option<HWND> {
    unsafe {
        if let Ok(progman) = FindWindowW(w!("Progman"), PCWSTR::null()) {
            if !progman.is_invalid() {
                return Some(progman);
            }
        }
        // Progman 找不到就退到「挂着 SHELLDLL_DefView 的那个窗口」——
        // Win11 24H2 起桌面图标层在 WorkerW 上，它和 Progman 同属一条桌面线程。
        let mut defview: HWND = HWND::default();
        let _ = EnumWindows(
            Some(enum_for_defview),
            LPARAM(&mut defview as *mut HWND as isize),
        );
        if !defview.is_invalid() {
            if let Ok(parent) = GetParent(defview) {
                if !parent.is_invalid() {
                    return Some(parent);
                }
            }
        }
        let shell = GetShellWindow();
        (!shell.is_invalid()).then_some(shell)
    }
}

/// 窗口所在线程 id（0 = 无效窗口）。
pub fn window_thread(hwnd: HWND) -> u32 {
    if hwnd.is_invalid() {
        return 0;
    }
    unsafe { GetWindowThreadProcessId(hwnd, None) }
}

/// 某条输入队列当前拿着的鼠标捕获窗口。
pub fn capture_of_thread(thread: u32) -> HWND {
    if thread == 0 {
        return HWND::default();
    }
    unsafe {
        let mut info = GUITHREADINFO {
            cbSize: std::mem::size_of::<GUITHREADINFO>() as u32,
            ..Default::default()
        };
        if GetGUIThreadInfo(thread, &mut info).is_ok() {
            info.hwndCapture
        } else {
            HWND::default()
        }
    }
}

/// 桌面线程当前拿着的鼠标捕获窗口。
pub fn capture_of_desktop() -> HWND {
    match desktop_window() {
        Some(window) => capture_of_thread(window_thread(window)),
        None => HWND::default(),
    }
}

/// 窗口是否还在。
pub fn is_alive(hwnd: isize) -> bool {
    let hwnd = HWND(hwnd as *mut c_void);
    !hwnd.is_invalid() && unsafe { IsWindow(Some(hwnd)).as_bool() }
}

/// `last_error` 只在调试时用得上，单独留个引用避免死代码告警淹没真正的警告。
#[allow(dead_code)]
pub fn last_win32_error(context: &str) -> String {
    last_error(context)
}

/* ------------------------------------------------------- 壁纸实时缩略图 */

/// 一张窗口缩略图（RGBA，行序自上而下）。
pub struct Thumbnail {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

/// 把窗口画面抓成一张缩略图，给主界面做「当前壁纸预览」。
///
/// 用 `PrintWindow(PW_RENDERFULLCONTENT)` + `StretchBlt` 缩到 `max_width` 以内，
/// 再 `GetDIBits` 取像素。返回 `None` 的三种情况：窗口无效、抓不到、抓到的是**全黑**帧
/// （DirectX 独占渲染的窗口经常如此）—— 界面据此显示「此壁纸无法实时预览」而不是一块黑。
///
/// 注意：**不要在壁纸进程被挂起时调用**，`PrintWindow` 会等目标进程响应而卡住；
/// 调用方（命令层）已经用 `paused` 挡在前面。
pub fn capture_thumbnail(hwnd: HWND, max_width: i32) -> Option<Thumbnail> {
    if hwnd.is_invalid() || max_width <= 0 {
        return None;
    }
    unsafe {
        let mut client = RECT::default();
        if GetClientRect(hwnd, &mut client).is_err() {
            return None;
        }
        let width = client.right - client.left;
        let height = client.bottom - client.top;
        if width <= 0 || height <= 0 {
            return None;
        }
        let scale = (max_width as f32 / width as f32).min(1.0);
        let thumb_width = ((width as f32 * scale).round() as i32).max(1);
        let thumb_height = ((height as f32 * scale).round() as i32).max(1);

        let screen = GetDC(None);
        if screen.is_invalid() {
            return None;
        }
        let full_dc = CreateCompatibleDC(Some(screen));
        let thumb_dc = CreateCompatibleDC(Some(screen));
        let full_bitmap = CreateCompatibleBitmap(screen, width, height);
        let thumb_bitmap = CreateCompatibleBitmap(screen, thumb_width, thumb_height);
        if full_dc.is_invalid() || thumb_dc.is_invalid() || full_bitmap.is_invalid() || thumb_bitmap.is_invalid() {
            let _ = ReleaseDC(None, screen);
            return None;
        }

        let old_full = SelectObject(full_dc, HGDIOBJ(full_bitmap.0));
        let printed = PrintWindow(hwnd, full_dc, PW_RENDERFULLCONTENT).as_bool();

        let old_thumb = SelectObject(thumb_dc, HGDIOBJ(thumb_bitmap.0));
        let _ = SetStretchBltMode(thumb_dc, HALFTONE);
        let stretched = if printed {
            StretchBlt(
                thumb_dc, 0, 0, thumb_width, thumb_height, Some(full_dc), 0, 0, width, height,
                SRCCOPY,
            )
            .as_bool()
        } else {
            false
        };

        let mut result = None;
        if stretched {
            let mut info = BITMAPINFO::default();
            info.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
            info.bmiHeader.biWidth = thumb_width;
            // 负高度 = 自上而下，省得再翻一次行
            info.bmiHeader.biHeight = -thumb_height;
            info.bmiHeader.biPlanes = 1;
            info.bmiHeader.biBitCount = 32;
            info.bmiHeader.biCompression = BI_RGB.0;

            let mut buffer = vec![0u8; (thumb_width * thumb_height * 4) as usize];
            let lines = GetDIBits(
                thumb_dc,
                thumb_bitmap,
                0,
                thumb_height as u32,
                Some(buffer.as_mut_ptr() as *mut std::ffi::c_void),
                &mut info,
                DIB_RGB_COLORS,
            );
            if lines > 0 {
                // BGRA → RGBA。注意 `GetDIBits` 拿到的 32bpp 位图 **alpha 恒为 0**，
                // 直接交给前端画会变成全透明（看起来就是一张白图），必须补满 255。
                let mut darkest: u8 = 0;
                for pixel in buffer.chunks_exact_mut(4) {
                    pixel.swap(0, 2);
                    pixel[3] = 255;
                    darkest = darkest.max(pixel[0]).max(pixel[1]).max(pixel[2]);
                }
                // 全黑说明 PrintWindow 没拿到真实画面（D3D 独占窗口的典型表现）
                if darkest > 8 {
                    result = Some(Thumbnail {
                        width: thumb_width as u32,
                        height: thumb_height as u32,
                        rgba: buffer,
                    });
                }
            }
        }

        SelectObject(thumb_dc, old_thumb);
        SelectObject(full_dc, old_full);
        let _ = DeleteObject(HGDIOBJ(thumb_bitmap.0));
        let _ = DeleteObject(HGDIOBJ(full_bitmap.0));
        let _ = DeleteDC(thumb_dc);
        let _ = DeleteDC(full_dc);
        let _ = ReleaseDC(None, screen);
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn target_rect_of_the_virtual_desktop_covers_every_monitor() {
        let all = target_rect(-1);
        assert!(all.right > all.left);
        assert!(all.bottom > all.top);
        for monitor in monitors() {
            assert!(monitor.x >= all.left && monitor.y >= all.top);
            assert!(monitor.x + monitor.width <= all.right);
        }
    }

    #[test]
    fn monitor_indices_are_contiguous_starting_at_zero() {
        let list = monitors();
        assert!(!list.is_empty(), "至少应该有一台显示器");
        for (position, monitor) in list.iter().enumerate() {
            assert_eq!(monitor.index, position as i32);
        }
    }

    #[test]
    fn a_missing_window_is_not_alive() {
        assert!(!is_alive(0));
    }
}
