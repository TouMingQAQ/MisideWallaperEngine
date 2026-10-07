//! Windows 控制面：系统状态（全屏 / 电池 / 前台）、进程音量与静音、进程挂起、
//! 内存占用、开机自启。
//!
//! 这里的每件事都是「宿主能对外部 exe 做的事」：Unity 程序本身没有控制接口，
//! 所以音量走 WASAPI 会话音量、暂停走 `NtSuspendProcess`、尺寸走启动参数。

use std::ffi::c_void;
use std::sync::Mutex;

use serde::Serialize;
use windows::core::{s, w, Interface, PCWSTR};
use windows::Win32::Foundation::{HWND, MAX_PATH};
use windows::Win32::Media::Audio::{
    eConsole, eRender, IAudioSessionControl2, IAudioSessionManager2, IMMDeviceEnumerator,
    ISimpleAudioVolume, MMDeviceEnumerator,
};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CLSCTX_ALL, COINIT_MULTITHREADED,
};
use windows::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
use windows::Win32::System::Power::{GetSystemPowerStatus, SYSTEM_POWER_STATUS};
use windows::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
use windows::Win32::System::Registry::{
    RegCreateKeyExW, RegDeleteValueW, RegOpenKeyExW, RegQueryValueExW, RegSetValueExW,
    HKEY, HKEY_CURRENT_USER, KEY_QUERY_VALUE, KEY_SET_VALUE, REG_OPTION_NON_VOLATILE, REG_SZ,
};
use windows::Win32::System::Threading::{
    OpenProcess, PROCESS_ACCESS_RIGHTS, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SUSPEND_RESUME,
    PROCESS_TERMINATE,
};
use windows::Win32::UI::Shell::{
    SHQueryUserNotificationState, QUNS_BUSY, QUNS_PRESENTATION_MODE, QUNS_RUNNING_D3D_FULL_SCREEN,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetForegroundWindow, GetWindowThreadProcessId, IsIconic, IsWindowVisible,
};

use super::desktop;

/// 开机自启在注册表 `Run` 键下的值名。
pub const AUTOSTART_VALUE: &str = "miside-wallpaper-engine";
const AUTOSTART_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";

/// 一次系统状态采样（`wp://monitor` 事件的负载）。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SystemState {
    /// 有全屏应用 / 演示模式在跑（Lively 用的 `SHQueryUserNotificationState`）。
    pub fullscreen: bool,
    /// 前台有窗口铺满整块显示器（且不是桌面、不是我们自己）。
    pub foreground_fullscreen: bool,
    /// 正在用电池。
    pub on_battery: bool,
    /// 桌面当前是前台（输入转发只在这时候开闸）。
    pub desktop_foreground: bool,
}

/// 采样一次系统状态。
pub fn system_state() -> SystemState {
    SystemState {
        fullscreen: is_fullscreen_running(),
        foreground_fullscreen: is_foreground_fullscreen(),
        on_battery: is_on_battery(),
        desktop_foreground: desktop::is_desktop_foreground(),
    }
}

/// 全屏应用 / 演示模式检测。
///
/// **不能只看 `SHQueryUserNotificationState` 的 `QUNS_BUSY`**：外壳在切换前台窗口
/// （点别的程序、Alt+Tab、开开始菜单）时会短暂地报 BUSY，用户看到的现象就是
/// "一切换到别的程序，壁纸就停一下又自己回来"。所以：
/// * `QUNS_RUNNING_D3D_FULL_SCREEN`（独占全屏的游戏）/ `QUNS_PRESENTATION_MODE` → 直接算；
/// * `QUNS_BUSY` → 再要一条实证：前台**真的**有一个盖满整块显示器的窗口（且不是桌面层/壁纸自己）。
pub fn is_fullscreen_running() -> bool {
    unsafe {
        match SHQueryUserNotificationState() {
            Ok(state) => {
                if state == QUNS_RUNNING_D3D_FULL_SCREEN || state == QUNS_PRESENTATION_MODE {
                    return true;
                }
                if state == QUNS_BUSY {
                    return foreground_covers_monitor();
                }
                false
            }
            // 查不到就当没有：宁可多跑一会儿壁纸，也不要无故暂停
            Err(_) => false,
        }
    }
}

/// 前台窗口是不是铺满了它所在的显示器。
///
/// 桌面层（Progman / WorkerW / 桌面图标）、挂在桌面层里的壁纸窗口、以及宿主自己的窗口都不算 ——
/// 用户在桌面上点来点去、或焦点短暂落到壁纸层，都不该把壁纸暂停掉。
pub fn foreground_covers_monitor() -> bool {
    unsafe {
        let foreground = GetForegroundWindow();
        if foreground.is_invalid() || !IsWindowVisible(foreground).as_bool() || IsIconic(foreground).as_bool() {
            return false;
        }
        if desktop::is_desktop_foreground() || desktop::is_wallpaper_window(foreground) {
            return false;
        }
        // 宿主自己的设置窗口当然也不算"全屏应用"
        let mut pid = 0u32;
        GetWindowThreadProcessId(foreground, Some(&mut pid));
        if pid == std::process::id() {
            return false;
        }
        let Some(monitor) = desktop::monitor_of_window(foreground) else {
            return false;
        };
        let mut rect = windows::Win32::Foundation::RECT::default();
        if windows::Win32::UI::WindowsAndMessaging::GetWindowRect(foreground, &mut rect).is_err() {
            return false;
        }
        let width = rect.right - rect.left;
        let height = rect.bottom - rect.top;
        // 允许 2px 的边框误差
        width >= monitor.width - 2 && height >= monitor.height - 2
    }
}

/// 前台窗口是不是铺满了它所在的显示器（"前台最大化时暂停"这一项用的判定）。
pub fn is_foreground_fullscreen() -> bool {
    foreground_covers_monitor()
}

/// 是否正在用电池（台式机 / 查不到时为 false）。
pub fn is_on_battery() -> bool {
    unsafe {
        let mut status = SYSTEM_POWER_STATUS::default();
        if GetSystemPowerStatus(&mut status).is_err() {
            return false;
        }
        // ACLineStatus: 0 = 电池供电，1 = 接市电，255 = 未知
        status.ACLineStatus == 0
    }
}

/// 进程占用的物理内存（MB，取不到返回 0）。
pub fn process_memory_mb(pid: u32) -> f32 {
    if pid == 0 {
        return 0.0;
    }
    unsafe {
        let Ok(handle) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else {
            return 0.0;
        };
        let mut counters = PROCESS_MEMORY_COUNTERS::default();
        let ok = GetProcessMemoryInfo(
            handle,
            &mut counters,
            std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
        );
        let _ = windows::Win32::Foundation::CloseHandle(handle);
        if ok.is_ok() {
            counters.WorkingSetSize as f32 / 1024.0 / 1024.0
        } else {
            0.0
        }
    }
}

/* -------------------------------------------------------- 进程挂起 / 恢复 */

type NtProcessFn = unsafe extern "system" fn(*mut c_void) -> i32;

/// 记住已经查过的 ntdll 函数指针，避免每次挂起都重新找一遍。
static NT_SUSPEND: Mutex<Option<usize>> = Mutex::new(None);
static NT_RESUME: Mutex<Option<usize>> = Mutex::new(None);

fn nt_process_function(name: &str, cache: &Mutex<Option<usize>>) -> Option<NtProcessFn> {
    let mut guard = cache.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(address) = *guard {
        return Some(unsafe { std::mem::transmute::<usize, NtProcessFn>(address) });
    }
    unsafe {
        let module = GetModuleHandleW(w!("ntdll.dll")).ok()?;
        let name = std::ffi::CString::new(name).ok()?;
        let proc = GetProcAddress(module, windows::core::PCSTR(name.as_ptr() as *const u8))?;
        let address = proc as usize;
        *guard = Some(address);
        Some(std::mem::transmute::<usize, NtProcessFn>(address))
    }
}

/// 挂起整个进程（壁纸的「暂停」）。
pub fn suspend_process(pid: u32) -> Result<(), String> {
    let Some(function) = nt_process_function("NtSuspendProcess", &NT_SUSPEND) else {
        return Err("这个系统上找不到 NtSuspendProcess，无法暂停壁纸".to_string());
    };
    unsafe {
        let handle = OpenProcess(PROCESS_SUSPEND_RESUME, false, pid)
            .map_err(|e| format!("打开进程 {pid} 失败：{e}"))?;
        let status = function(handle.0);
        let _ = windows::Win32::Foundation::CloseHandle(handle);
        if status >= 0 {
            Ok(())
        } else {
            Err(format!("挂起进程 {pid} 失败：NTSTATUS {status:#x}"))
        }
    }
}

/// 恢复整个进程。
pub fn resume_process(pid: u32) -> Result<(), String> {
    let Some(function) = nt_process_function("NtResumeProcess", &NT_RESUME) else {
        return Err("这个系统上找不到 NtResumeProcess，无法恢复壁纸".to_string());
    };
    unsafe {
        let handle = OpenProcess(PROCESS_SUSPEND_RESUME, false, pid)
            .map_err(|e| format!("打开进程 {pid} 失败：{e}"))?;
        let status = function(handle.0);
        let _ = windows::Win32::Foundation::CloseHandle(handle);
        if status >= 0 {
            Ok(())
        } else {
            Err(format!("恢复进程 {pid} 失败：NTSTATUS {status:#x}"))
        }
    }
}

/* ------------------------------------------------------------ 会话音量 */

/// 在默认播放设备的所有音频会话里找这个进程，设置音量 / 静音。
///
/// 返回 `Ok(false)` 表示进程当前没有音频会话（还没出声），调用方可以稍后重试 ——
/// 这不是错误，Unity 壁纸在播放第一段音频之前确实没有会话。
pub fn set_process_volume(pid: u32, volume: Option<f32>, muted: Option<bool>) -> Result<bool, String> {
    if pid == 0 {
        return Ok(false);
    }
    unsafe {
        // 采集线程与命令线程都可能进来，用 MTA：重复初始化返回 S_FALSE / RPC_E_CHANGED_MODE 都无害
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);

        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
                .map_err(|e| format!("创建音频设备枚举器失败：{e}"))?;
        let device = enumerator
            .GetDefaultAudioEndpoint(eRender, eConsole)
            .map_err(|e| format!("获取默认播放设备失败：{e}"))?;
        let manager: IAudioSessionManager2 = device
            .Activate(CLSCTX_ALL, None)
            .map_err(|e| format!("打开音频会话管理器失败：{e}"))?;
        let sessions = manager
            .GetSessionEnumerator()
            .map_err(|e| format!("枚举音频会话失败：{e}"))?;
        let count = sessions.GetCount().map_err(|e| format!("读取会话数失败：{e}"))?;

        let mut touched = false;
        for index in 0..count {
            let Ok(control) = sessions.GetSession(index) else {
                continue;
            };
            let Ok(control2) = control.cast::<IAudioSessionControl2>() else {
                continue;
            };
            if control2.GetProcessId().unwrap_or(0) != pid {
                continue;
            }
            let Ok(volume_control) = control.cast::<ISimpleAudioVolume>() else {
                continue;
            };
            if let Some(level) = volume {
                volume_control
                    .SetMasterVolume(level.clamp(0.0, 1.0), std::ptr::null())
                    .map_err(|e| format!("设置音量失败：{e}"))?;
            }
            if let Some(flag) = muted {
                volume_control
                    .SetMute(flag, std::ptr::null())
                    .map_err(|e| format!("设置静音失败：{e}"))?;
            }
            touched = true;
        }
        Ok(touched)
    }
}

/* -------------------------------------------------------------- 开机自启 */

fn exe_path() -> Option<String> {
    let mut buffer = vec![0u16; MAX_PATH as usize];
    let length = unsafe {
        windows::Win32::System::LibraryLoader::GetModuleFileNameW(
            None,
            &mut buffer,
        )
    };
    if length == 0 {
        return None;
    }
    Some(String::from_utf16_lossy(&buffer[..length as usize]))
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

/// 打开（必要时创建）自启键。
unsafe fn open_run_key(create: bool) -> Result<HKEY, String> {
    let mut key = HKEY::default();
    let path = wide(AUTOSTART_KEY);
    if create {
        let result = RegCreateKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR(path.as_ptr()),
            None,
            PCWSTR::null(),
            REG_OPTION_NON_VOLATILE,
            KEY_SET_VALUE | KEY_QUERY_VALUE,
            None,
            &mut key,
            None,
        );
        if result.is_err() {
            return Err(format!("打开注册表自启键失败：{result:?}"));
        }
    } else {
        let result = RegOpenKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR(path.as_ptr()),
            None,
            KEY_QUERY_VALUE,
            &mut key,
        );
        if result.is_err() {
            return Err("读取注册表自启键失败".to_string());
        }
    }
    Ok(key)
}

/// 打开 / 关闭开机自启，返回设置后的状态。
pub fn set_autostart(enabled: bool) -> Result<bool, String> {
    let Some(exe) = exe_path() else {
        return Err("取不到自身程序路径，无法设置开机自启".to_string());
    };
    unsafe {
        let key = open_run_key(true)?;
        let name = wide(AUTOSTART_VALUE);
        if enabled {
            // 带引号，路径里有空格也不会被当成参数分隔
            let command = format!("\"{exe}\"");
            let value = wide(&command);
            let bytes = std::slice::from_raw_parts(
                value.as_ptr() as *const u8,
                value.len() * std::mem::size_of::<u16>(),
            );
            let result = RegSetValueExW(key, PCWSTR(name.as_ptr()), None, REG_SZ, Some(bytes));
            let _ = windows::Win32::System::Registry::RegCloseKey(key);
            if result.is_err() {
                return Err(format!("写入开机自启失败：{result:?}"));
            }
        } else {
            let result = RegDeleteValueW(key, PCWSTR(name.as_ptr()));
            let _ = windows::Win32::System::Registry::RegCloseKey(key);
            // 值本来就不在，按「已经关掉」处理
            if result.is_err() {
                return Ok(false);
            }
        }
    }
    Ok(enabled)
}

/// 当前是否已经开启开机自启（比对注册表里的路径是不是自己）。
pub fn autostart_enabled() -> bool {
    let Some(exe) = exe_path() else {
        return false;
    };
    unsafe {
        let Ok(key) = open_run_key(false) else {
            return false;
        };
        let name = wide(AUTOSTART_VALUE);
        let mut kind = windows::Win32::System::Registry::REG_VALUE_TYPE::default();
        let mut size: u32 = 0;
        let probe = RegQueryValueExW(
            key,
            PCWSTR(name.as_ptr()),
            None,
            Some(&mut kind),
            None,
            Some(&mut size),
        );
        if probe.is_err() || size == 0 {
            let _ = windows::Win32::System::Registry::RegCloseKey(key);
            return false;
        }
        let mut buffer = vec![0u8; size as usize];
        let read = RegQueryValueExW(
            key,
            PCWSTR(name.as_ptr()),
            None,
            Some(&mut kind),
            Some(buffer.as_mut_ptr()),
            Some(&mut size),
        );
        let _ = windows::Win32::System::Registry::RegCloseKey(key);
        if read.is_err() {
            return false;
        }
        let text = String::from_utf16_lossy(
            &buffer[..size as usize]
                .chunks_exact(2)
                .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
                .take_while(|code| *code != 0)
                .collect::<Vec<u16>>(),
        );
        text.trim_matches('"').eq_ignore_ascii_case(&exe)
    }
}

/// 当前前台窗口（给日志 / 调试用）。
pub fn foreground_window() -> HWND {
    unsafe { GetForegroundWindow() }
}

/// 进程访问权限的辅助函数：确认某个 PID 还活着。
pub fn process_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    unsafe {
        match OpenProcess(PROCESS_ACCESS_RIGHTS(PROCESS_QUERY_LIMITED_INFORMATION.0), false, pid) {
            Ok(handle) => {
                let mut code = 0u32;
                let ok = windows::Win32::System::Threading::GetExitCodeProcess(
                    handle,
                    &mut code,
                )
                .is_ok();
                let _ = windows::Win32::Foundation::CloseHandle(handle);
                ok && code == 259 // STILL_ACTIVE
            }
            Err(_) => false,
        }
    }
}

/// 取某个进程的可执行文件全路径（拿不到返回 `None`）。
///
/// 用来判断"记录在案的那个 PID 现在还是不是当初我们启动的那个壁纸" —— PID 会被系统复用，
/// 只凭 PID 就去杀进程是有风险的。
pub fn process_path(pid: u32) -> Option<String> {
    if pid == 0 {
        return None;
    }
    use windows::core::PWSTR;
    use windows::Win32::System::Threading::{QueryFullProcessImageNameW, PROCESS_NAME_FORMAT};
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut buffer = vec![0u16; 1024];
        let mut size = buffer.len() as u32;
        let ok = QueryFullProcessImageNameW(
            handle,
            PROCESS_NAME_FORMAT(0),
            PWSTR(buffer.as_mut_ptr()),
            &mut size,
        )
        .is_ok();
        let _ = windows::Win32::Foundation::CloseHandle(handle);
        if ok {
            Some(String::from_utf16_lossy(&buffer[..size as usize]))
        } else {
            None
        }
    }
}

/// 按 PID 结束一个进程（没有 `Child` 句柄时用，比如清理上次运行留下的壁纸）。
pub fn terminate_process(pid: u32) -> Result<(), String> {
    if pid == 0 {
        return Ok(());
    }
    unsafe {
        let handle = OpenProcess(PROCESS_TERMINATE, false, pid)
            .map_err(|e| format!("打开进程 {pid} 失败：{e}"))?;
        let result = windows::Win32::System::Threading::TerminateProcess(handle, 1);
        let _ = windows::Win32::Foundation::CloseHandle(handle);
        match result {
            Ok(()) => Ok(()),
            Err(err) => Err(format!("结束进程 {pid} 失败：{err}")),
        }
    }
}

/// 两个路径是不是同一个可执行文件（大小写不敏感，忽略路径分隔符差异）。
pub fn same_executable(left: &str, right: &str) -> bool {
    let clean = |value: &str| value.replace('/', "\\").to_lowercase();
    !left.is_empty() && clean(left) == clean(right)
}

/* ------------------------------------------------------- 进程守卫（Job） */

/// 「宿主没了，壁纸也得跟着走」——把子进程放进一个设了
/// `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` 的 Job Object。
///
/// 这是治「取消挂载后壁纸不消失」的正解：宿主无论以什么方式结束
/// （正常退出、崩溃、被任务管理器强杀），内核都会在 Job 句柄关闭时把子进程一起结束，
/// 不会留下一个还挂在桌面层上、用户点「停止」也摘不掉的孤儿窗口。
pub struct KillOnCloseJob(windows::Win32::Foundation::HANDLE);

// 句柄由本结构体独占，仅用于 AssignProcessToJobObject。
unsafe impl Send for KillOnCloseJob {}

impl KillOnCloseJob {
    /// 创建一个「句柄关闭即杀子进程」的 Job。
    pub fn create() -> Option<Self> {
        use windows::Win32::System::JobObjects::{
            CreateJobObjectW, JobObjectExtendedLimitInformation, SetInformationJobObject,
            JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        };
        unsafe {
            let job = CreateJobObjectW(None, PCWSTR::null()).ok()?;
            let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            let ok = SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                &info as *const _ as *const c_void,
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
            .is_ok();
            if ok {
                Some(Self(job))
            } else {
                let _ = windows::Win32::Foundation::CloseHandle(job);
                None
            }
        }
    }

    /// 把已经启动的进程塞进这个 Job。
    ///
    /// 失败不算致命：老系统上可能不允许嵌套 Job，那就退回"退出时主动杀 + 启动时清理孤儿"。
    pub fn assign(&self, pid: u32) -> Result<(), String> {
        use windows::Win32::System::JobObjects::AssignProcessToJobObject;
        use windows::Win32::System::Threading::{OpenProcess, PROCESS_SET_QUOTA, PROCESS_TERMINATE};
        unsafe {
            let process = OpenProcess(PROCESS_SET_QUOTA | PROCESS_TERMINATE, false, pid)
                .map_err(|e| format!("打开进程 {pid} 失败：{e}"))?;
            let result = AssignProcessToJobObject(self.0, process);
            let _ = windows::Win32::Foundation::CloseHandle(process);
            result.map_err(|e| format!("把进程 {pid} 加入 Job 失败：{e}"))
        }
    }
}

impl Drop for KillOnCloseJob {
    fn drop(&mut self) {
        unsafe {
            let _ = windows::Win32::Foundation::CloseHandle(self.0);
        }
    }
}

/// 让 `s!` 宏在非 Windows 分支也不至于被判定为未使用。
#[allow(dead_code)]
fn _unused() {
    let _ = s!("miside-wallpaper-engine");
}
