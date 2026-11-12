//! 崩溃 / 卡死捕捉。
//!
//! 这里补的是「运行一会儿以后崩了，但日志里什么都没有」这一类问题：
//!
//! * **panic 钩子** —— Rust 侧任何线程 panic（后台扫描线程、托盘、命令线程）都会写进
//!   `logs\crash.log`，并附上线程名、位置和回溯；GUI 版本没有控制台，默认的 panic 输出
//!   会直接丢掉，所以必须自己落盘；
//! * **原生异常过滤（Windows）** —— 访问违例这类不是 panic 的崩溃走
//!   `SetUnhandledExceptionFilter`，记录异常码 / 出错模块 / 偏移，并写一份 minidump，
//!   下次用户只要把 `logs` 目录发过来就能定位；
//! * **看门狗** —— 扫描线程每 tick 打一次心跳，主线程（界面）由看门狗投递心跳。
//!   任何一方停摆（典型：卡在跨进程窗口消息里，Windows 会记 AppHangXProc）都会写日志；
//! * **壁纸子进程退出** —— 退出码（例如 `0xC0000005` 访问违例）、运行时长、峰值内存，
//!   并顺手把 Windows 应用日志里的崩溃/挂起记录、Unity 自己的 `Player.log` 尾巴摘下来；
//! * **会话标记** —— 正常退出会删掉 `session.json`，下次启动若发现它还在，说明上次是
//!   崩溃 / 被强杀，直接写一条 warning。
//!
//! 所有写盘都走同一个 [`write_host_log`]，与 `commands::log` 共用 `host.log` 的轮转规则，
//! 保证「界面看到的」和「文件里的」是同一份内容。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tauri::{AppHandle, Manager, Runtime};

/// host.log / crash.log 的单文件上限，超过就轮转一次。
const MAX_LOG_BYTES: u64 = 2 * 1024 * 1024;
/// 内存里保留的操作轨迹条数（用于崩溃现场回放）。
const MAX_TRACE: usize = 256;

/// 日志目录（启动时由 [`init`] 写入）。
static LOG_DIR: OnceLock<PathBuf> = OnceLock::new();
/// 串行化所有文件写入，避免多线程交错写坏一行。
static FILE_LOCK: Mutex<()> = Mutex::new(());
/// 「最近做了什么」的环形轨迹：崩溃 / 卡死时一起打出来。
static TRACE: Mutex<std::collections::VecDeque<String>> = Mutex::new(std::collections::VecDeque::new());
/// 扫描线程最后一次心跳（Unix 毫秒）。
static SCAN_BEAT: AtomicI64 = AtomicI64::new(0);
/// 主线程最后一次执行看门狗投递任务的时间（Unix 毫秒）。
static MAIN_BEAT: AtomicI64 = AtomicI64::new(0);
/// 前端（WebView）最后一次心跳（Unix 毫秒）。
static UI_BEAT: AtomicI64 = AtomicI64::new(0);
/// 前端是否至少发过一次心跳（没有界面的场景不该被误判成「渲染进程崩了」）。
static UI_SEEN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
/// 当前扫描线程停在哪一步。
static STAGE: Mutex<String> = Mutex::new(String::new());

/* ------------------------------------------------------------- 基础工具 */

pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|value| value.as_millis() as i64)
        .unwrap_or(0)
}
/// 与 `library::iso8601` 保持一致的本地时间格式。
fn stamp() -> String {
    crate::library::iso8601(SystemTime::now())
}

/// 给文件名用的紧凑时间戳：20261103-114502。
fn stamp_compact() -> String {
    let text = stamp();
    let digits: String = text
        .chars()
        .filter(|c| c.is_ascii_digit())
        .collect::<String>();
    // iso8601 输出形如 2026-11-03T11:45:02Z -> 20261103114502
    if digits.len() >= 14 {
        format!("{}-{}", &digits[0..8], &digits[8..14])
    } else {
        now_ms().to_string()
    }
}

/// 日志目录。`init` 之前调用会退回 `%LOCALAPPDATA%` 下的默认位置。
pub fn default_log_dir() -> PathBuf {
    std::env::var_os("LOCALAPPDATA")
        .map(|base| {
            PathBuf::from(base)
                .join("com.toumingqaq.miside-wallpaper-engine")
                .join("logs")
        })
        .unwrap_or_else(|| std::env::temp_dir().join("miside-wallpaper-engine-logs"))
}

/// 启动时登记日志目录，并重置各类心跳。必须是 `setup` 里最早做的事之一。
pub fn init<R: Runtime>(app: &AppHandle<R>) {
    let dir = app
        .path()
        .app_log_dir()
        .unwrap_or_else(|_| default_log_dir());
    let _ = std::fs::create_dir_all(&dir);
    let _ = LOG_DIR.set(dir);
    let now = now_ms();
    SCAN_BEAT.store(now, Ordering::Relaxed);
    MAIN_BEAT.store(now, Ordering::Relaxed);
    UI_BEAT.store(now, Ordering::Relaxed);
}

/// 日志目录。即使 `init` 还没跑（例如启动早期就 panic），也退回默认位置，保证日志不丢。
pub fn log_dir() -> Option<&'static Path> {
    Some(LOG_DIR.get_or_init(default_log_dir).as_path())
}

/* --------------------------------------------------------------- 写日志 */

/// 往 `host.log` 追加一行（带轮转）。`commands::log` 也走这里，保证格式一致。
pub fn write_host_log(level: &str, message: &str) {
    let Some(dir) = log_dir() else {
        return;
    };
    let _guard = FILE_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    write_rotated(dir, "host.log", &format!("[{}] {:<5} {}", stamp(), level.to_uppercase(), message));
}

/// 往 `crash.log` 追加一段（崩溃 / 卡死 / 进程异常退出专用）。
pub fn write_crash(kind: &str, body: &str) {
    let Some(dir) = log_dir() else {
        return;
    };
    let _guard = FILE_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    write_rotated(
        dir,
        "crash.log",
        &format!("===== {} · {} =====\n{}\n", stamp(), kind, body.trim_end()),
    );
}

/// 写一份独立的现场文件 `crash-<时间>-<名字>.txt`，返回路径（导出诊断时会一起打包）。
pub fn write_incident(name: &str, body: &str) -> Option<PathBuf> {
    let dir = log_dir()?;
    let _ = std::fs::create_dir_all(dir);
    let safe: String = name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '-' })
        .collect();
    let file = dir.join(format!("crash-{}-{}.txt", stamp_compact(), safe));
    let text = format!(
        "miside-wallpaper-engine v{}\n{} · {}\n\n{}\n",
        crate::APP_VERSION,
        stamp(),
        name,
        body.trim_end()
    );
    match std::fs::write(&file, text) {
        Ok(()) => Some(file),
        Err(_) => None,
    }
}

fn write_rotated(dir: &Path, name: &str, line: &str) {
    let _ = std::fs::create_dir_all(dir);
    let file = dir.join(name);
    if std::fs::metadata(&file).map(|meta| meta.len()).unwrap_or(0) > MAX_LOG_BYTES {
        let rotated = dir.join(format!("{name}.1"));
        let _ = std::fs::remove_file(&rotated);
        let _ = std::fs::rename(&file, &rotated);
    }
    if let Ok(mut handle) = std::fs::OpenOptions::new().create(true).append(true).open(&file) {
        use std::io::Write;
        let _ = writeln!(handle, "{line}");
    }
}

/* --------------------------------------------------------------- 轨迹 */

/// 记一条操作轨迹（崩溃现场回放用）。开销很小，扫描线程里可以放心调。
pub fn note(stage: impl Into<String>) {
    let mut trace = TRACE.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    trace.push_back(format!("[{}] {}", stamp(), stage.into()));
    while trace.len() > MAX_TRACE {
        trace.pop_front();
    }
}

/// 当前扫描阶段（看门狗判断「卡在哪」用）。
pub fn set_stage(stage: impl Into<String>) {
    let stage = stage.into();
    *STAGE.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = stage.clone();
    note(format!("scan · {stage}"));
}

pub fn current_stage() -> String {
    STAGE
        .lock()
        .map(|guard| guard.clone())
        .unwrap_or_else(|poisoned| poisoned.into_inner().clone())
}

fn trace_section() -> String {
    let trace = TRACE.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let mut text = String::from("--- 最近操作轨迹 ---\n");
    if trace.is_empty() {
        text.push_str("（无）\n");
    } else {
        for line in trace.iter() {
            text.push_str(line);
            text.push('\n');
        }
    }
    text
}

/* ------------------------------------------------------------- panic 钩子 */

fn panic_payload(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(text) = payload.downcast_ref::<&str>() {
        (*text).to_string()
    } else if let Some(text) = payload.downcast_ref::<String>() {
        text.clone()
    } else {
        "（无法读取 panic 信息）".to_string()
    }
}

/// 安装 panic 钩子。越早越好（在 `tauri::Builder::default()` 之前）。
pub fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let thread = std::thread::current();
        let thread_name = thread.name().unwrap_or("<未命名线程>").to_string();
        let location = info
            .location()
            .map(|loc| format!("{}:{}:{}", loc.file(), loc.line(), loc.column()))
            .unwrap_or_else(|| "<未知位置>".to_string());
        let message = panic_payload(info.payload());
        let backtrace = std::backtrace::Backtrace::force_capture();

        let body = format!(
            "线程: {thread_name}\n位置: {location}\n信息: {message}\n\n--- 回溯 ---\n{backtrace}\n{trace}",
            trace = trace_section()
        );
        write_host_log(
            "error",
            &format!("【崩溃】线程 {thread_name} panic：{message}（{location}）"),
        );
        write_crash("panic", &body);
        let _ = write_incident("panic", &body);

        // 保留默认行为：有控制台时仍然打印 stderr
        previous(info);
    }));
}

/* --------------------------------------------------- 原生异常 + minidump */

#[repr(C)]
struct ExceptionRecord {
    code: u32,
    flags: u32,
    record: *mut ExceptionRecord,
    address: *mut core::ffi::c_void,
    number_parameters: u32,
    information: [usize; 15],
}

#[repr(C)]
struct ExceptionPointers {
    record: *mut ExceptionRecord,
    context: *mut core::ffi::c_void,
}

#[repr(C)]
struct MinidumpExceptionInfo {
    thread_id: u32,
    exception_pointers: *mut ExceptionPointers,
    client_pointers: i32,
}

type FilterFn = unsafe extern "system" fn(*const ExceptionPointers) -> i32;
type MiniDumpWriteDumpFn = unsafe extern "system" fn(
    *mut core::ffi::c_void,
    u32,
    *mut core::ffi::c_void,
    u32,
    *const MinidumpExceptionInfo,
    *const core::ffi::c_void,
    *const core::ffi::c_void,
) -> i32;

#[link(name = "kernel32")]
extern "system" {
    fn SetUnhandledExceptionFilter(filter: Option<FilterFn>) -> *mut core::ffi::c_void;
}

const EXCEPTION_EXECUTE_HANDLER: i32 = 1;
/// MiniDumpWithThreadInfo：线程名/寄存器等，够定位，体积也可控。
const MINIDUMP_WITH_THREAD_INFO: u32 = 0x0000_1000;

/// 安装 Windows 未处理异常过滤器：访问违例 / 栈溢出这类「直接把进程带走」的崩溃，
/// 会在这里留下异常码、出错模块和一份 minidump。
pub fn install_exception_filter() {
    unsafe {
        // 提前把 dbghelp 载入，崩溃时就不用再 LoadLibrary（那时堆栈已经很脆弱）
        let _ = windows::Win32::System::LibraryLoader::LoadLibraryW(windows::core::w!("dbghelp.dll"));
        SetUnhandledExceptionFilter(Some(unhandled_exception_filter));
    }
}

unsafe extern "system" fn unhandled_exception_filter(pointers: *const ExceptionPointers) -> i32 {
    let mut body = String::new();
    let mut address: *mut core::ffi::c_void = std::ptr::null_mut();
    let mut code = 0u32;

    if !pointers.is_null() {
        let record = (*pointers).record;
        if !record.is_null() {
            code = (*record).code;
            address = (*record).address;
        }
    }
    body.push_str(&format!("异常代码: {:#010X}（{}）\n", code, describe_ntstatus(code)));
    body.push_str(&format!("异常地址: {address:p}\n"));
    body.push_str(&format!("出错模块: {}\n", module_for_address(address)));
    body.push_str(&format!("线程: {}\n", std::thread::current().name().unwrap_or("<未知>")));
    body.push_str(&format!(
        "\n--- 当前线程回溯 ---\n{}\n",
        std::backtrace::Backtrace::force_capture()
    ));
    body.push_str(&trace_section());

    write_host_log(
        "error",
        &format!("【崩溃】原生异常 {:#010X}（{}）@ {address:p}", code, describe_ntstatus(code)),
    );

    let dump = write_minidump(pointers);
    if let Some(path) = &dump {
        body.push_str(&format!("\nminidump: {}\n", path.display()));
    }
    write_crash("原生异常", &body);
    let _ = write_incident("native-exception", &body);

    // 已经尽力留档了，安静退出，不要弹 WER 对话框
    EXCEPTION_EXECUTE_HANDLER
}

unsafe fn write_minidump(pointers: *const ExceptionPointers) -> Option<PathBuf> {
    let dir = log_dir()?;
    let _ = std::fs::create_dir_all(dir);
    let path = dir.join(format!("crash-{}.dmp", stamp_compact()));

    use std::os::windows::io::AsRawHandle;
    let file = std::fs::File::create(&path).ok()?;
    let file_handle = file.as_raw_handle() as *mut core::ffi::c_void;

    let module =
        windows::Win32::System::LibraryLoader::LoadLibraryW(windows::core::w!("dbghelp.dll")).ok()?;
    let address = windows::Win32::System::LibraryLoader::GetProcAddress(
        module,
        windows::core::s!("MiniDumpWriteDump"),
    )?;
    let write_dump: MiniDumpWriteDumpFn = std::mem::transmute(address);
    let mut info = MinidumpExceptionInfo {
        thread_id: windows::Win32::System::Threading::GetCurrentThreadId(),
        exception_pointers: pointers as *mut ExceptionPointers,
        client_pointers: 0,
    };
    let ok = write_dump(
        windows::Win32::System::Threading::GetCurrentProcess().0,
        windows::Win32::System::Threading::GetCurrentThreadId(),
        file_handle,
        MINIDUMP_WITH_THREAD_INFO,
        &mut info,
        std::ptr::null(),
        std::ptr::null(),
    );
    // file 在这里仍然存活，保证句柄有效
    drop(file);
    if ok != 0 {
        Some(path)
    } else {
        let _ = std::fs::remove_file(&path);
        None
    }
}

/// 地址落在哪个模块里（用于回答「崩在显卡驱动还是 Unity」）。
fn module_for_address(address: *mut core::ffi::c_void) -> String {
    if address.is_null() {
        return "未知".to_string();
    }
    use windows::core::PCWSTR;
    use windows::Win32::System::LibraryLoader::{
        GetModuleFileNameW, GetModuleHandleExW, GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS,
        GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
    };
    unsafe {
        let mut module = windows::Win32::Foundation::HMODULE::default();
        let flags = GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT;
        if GetModuleHandleExW(
            flags,
            PCWSTR(address as *const u16),
            &mut module,
        )
        .is_err()
        {
            return "未知（不在任何已加载模块中）".to_string();
        }
        let mut buffer = vec![0u16; 1024];
        let length = GetModuleFileNameW(Some(module), &mut buffer);
        if length == 0 {
            return "未知".to_string();
        }
        let path = String::from_utf16_lossy(&buffer[..length as usize]);
        let base = module.0 as usize;
        format!("{path}（基址 {base:#x}，偏移 +{:#x}）", (address as usize).wrapping_sub(base))
    }
}

/* --------------------------------------------------------------- 看门狗 */

/// 扫描线程每 tick 调一次。
pub fn beat() {
    SCAN_BEAT.store(now_ms(), Ordering::Relaxed);
}

/// 前端（WebView）心跳。
pub fn ui_beat() {
    UI_BEAT.store(now_ms(), Ordering::Relaxed);
    UI_SEEN.store(true, Ordering::Relaxed);
}

/// 主线程看门狗：扫描线程停摆、界面线程卡住、WebView 心跳中断都会记一条。
///
/// 为什么主线程要这样探：Windows 的「应用无响应」是按界面线程算的。真卡在
/// `SendMessage` / `PrintWindow` 这种跨进程调用上时，界面线程自己已经没法写日志了，
/// 只有看门狗这条独立线程还活着 —— 由它把「卡了多久」落盘。
pub fn spawn_watchdog<R: Runtime>(app: AppHandle<R>) {
    let handle = app.clone();
    let spawned = std::thread::Builder::new()
        .name("wp-watchdog".to_string())
        .spawn(move || {
            let mut last_scan_warn = 0i64;
            let mut last_main_warn = 0i64;
            let mut last_ui_warn = 0i64;
            let mut scan_stalled_since: Option<i64> = None;
            let mut main_stalled_since: Option<i64> = None;
            loop {
                std::thread::sleep(Duration::from_secs(2));

                // 1) 给主线程投递一个心跳任务；任务执行了 MAIN_BEAT 才会前进
                let main_probe = handle.clone();
                let _ = main_probe.run_on_main_thread(|| {
                    MAIN_BEAT.store(now_ms(), Ordering::Relaxed);
                });

                // 2) 扫描线程心跳
                let scan_age = now_ms() - SCAN_BEAT.load(Ordering::Relaxed);
                if scan_age > 5_000 {
                    if scan_stalled_since.is_none() {
                        scan_stalled_since = Some(now_ms());
                        last_scan_warn = now_ms();
                        crate::commands::log(
                            &handle,
                            "warn",
                            format!(
                                "扫描线程已 {:.1} 秒没有心跳（可能卡在：{}）",
                                scan_age as f64 / 1000.0,
                                current_stage()
                            ),
                        );
                    } else if now_ms() - last_scan_warn > 30_000 {
                        last_scan_warn = now_ms();
                        crate::commands::log(
                            &handle,
                            "warn",
                            format!(
                                "扫描线程仍无心跳（已 {:.1} 秒，卡在：{}）",
                                scan_age as f64 / 1000.0,
                                current_stage()
                            ),
                        );
                    }
                } else if let Some(since) = scan_stalled_since.take() {
                    crate::commands::log(
                        &handle,
                        "info",
                        format!("扫描线程已恢复（停摆约 {:.1} 秒）", (now_ms() - since) as f64 / 1000.0),
                    );
                }

                // 3) 主线程（界面）心跳
                let main_age = now_ms() - MAIN_BEAT.load(Ordering::Relaxed);
                if main_age > 8_000 {
                    if main_stalled_since.is_none() {
                        main_stalled_since = Some(now_ms());
                        last_main_warn = now_ms();
                        crate::commands::log(
                            &handle,
                            "warn",
                            format!(
                                "界面主线程已 {:.1} 秒无响应（可能卡在跨进程窗口消息，见 crash.log）",
                                main_age as f64 / 1000.0
                            ),
                        );
                        write_crash(
                            "界面线程卡住",
                            &format!(
                                "主线程超过 {:.1} 秒没有执行事件循环任务。\n{}",
                                main_age as f64 / 1000.0,
                                trace_section()
                            ),
                        );
                    } else if now_ms() - last_main_warn > 30_000 {
                        last_main_warn = now_ms();
                        crate::commands::log(
                            &handle,
                            "warn",
                            format!("界面主线程仍无响应（已 {:.1} 秒）", main_age as f64 / 1000.0),
                        );
                    }
                } else if let Some(since) = main_stalled_since.take() {
                    crate::commands::log(
                        &handle,
                        "info",
                        format!("界面主线程已恢复（卡住约 {:.1} 秒）", (now_ms() - since) as f64 / 1000.0),
                    );
                }

                // 4) WebView 心跳（前端每 3 秒一次）。
                // 阈值放到 90 秒：窗口收进托盘 / 最小化时 Chromium 会把定时器节流到
                // 每分钟一次，不能按 15 秒判死。只有真发过心跳的界面才参与判断。
                let ui_age = now_ms() - UI_BEAT.load(Ordering::Relaxed);
                if UI_SEEN.load(Ordering::Relaxed)
                    && ui_age > 90_000
                    && now_ms() - last_ui_warn > 60_000
                {
                    last_ui_warn = now_ms();
                    crate::commands::log(
                        &handle,
                        "warn",
                        format!(
                            "界面（WebView）心跳中断已 {:.1} 秒，渲染进程可能已崩溃或卡死",
                            ui_age as f64 / 1000.0
                        ),
                    );
                }
            }
        });

    if spawned.is_err() {
        eprintln!("[miside-wallpaper-engine] 无法创建看门狗线程");
    }
}

/* ------------------------------------------------------- 退出码 / 异常码 */

/// 把 Windows 退出码翻译成人话。Unity 崩溃最常见的是 `0xC0000005`。
pub fn describe_exit_code(code: Option<i32>) -> String {
    match code {
        None => "未知（进程句柄已失效，读不到退出码）".to_string(),
        Some(0) => "0x00000000 正常退出".to_string(),
        Some(code) => {
            let raw = code as u32;
            let name = match raw {
                0xC000_0005 => "ACCESS_VIOLATION 访问冲突（读了非法内存，常见于显卡驱动 / Unity 渲染崩溃）",
                0xC000_0006 => "IN_PAGE_ERROR（分页文件读写失败）",
                0xC000_0008 => "INVALID_HANDLE 无效句柄",
                0xC000_0017 => "NO_MEMORY 内存不足",
                0xC000_001D => "ILLEGAL_INSTRUCTION 非法指令",
                0xC000_0022 => "ACCESS_DENIED 访问被拒绝",
                0xC000_0094 => "INTEGER_DIVIDE_BY_ZERO 整数除零",
                0xC000_0096 => "PRIVILEGED_INSTRUCTION 特权指令",
                0xC000_00FD => "STACK_OVERFLOW 栈溢出",
                0xC000_0135 => "DLL_NOT_FOUND 找不到 DLL",
                0xC000_0139 => "ENTRYPOINT_NOT_FOUND DLL 入口点缺失",
                0xC000_013A => "被 Ctrl+C / 关闭窗口终止",
                0xC000_0142 => "DLL_INIT_FAILED DLL 初始化失败",
                0xC000_0374 => "HEAP_CORRUPTION 堆损坏",
                0xC000_0409 => "STACK_BUFFER_OVERRUN / fail-fast（安全检查失败）",
                0x4001_0004 => "被调试器终止（TerminateProcess）",
                0x8000_0003 => "断点（BREAKPOINT）",
                0xE06D_7363 => "C++ 未捕获异常（throw 到顶层）",
                0xFFFF_FFFF => "被强制结束（TerminateProcess(-1)）",
                0x0000_0001 => "通用错误（应用主动退出 / 未捕获异常）",
                0x0000_0002 => "文件未找到",
                0x0000_0003 => "路径未找到",
                _ => "未知含义",
            };
            format!("0x{raw:08X} {name}")
        }
    }
}

/// NTSTATUS / SEH 异常码的短名。
pub fn describe_ntstatus(code: u32) -> &'static str {
    match code {
        0xC000_0005 => "ACCESS_VIOLATION 访问冲突",
        0xC000_0006 => "IN_PAGE_ERROR 页错误",
        0xC000_0008 => "INVALID_HANDLE 无效句柄",
        0xC000_0017 => "NO_MEMORY 内存不足",
        0xC000_001D => "ILLEGAL_INSTRUCTION 非法指令",
        0xC000_008C => "ARRAY_BOUNDS_EXCEEDED 数组越界",
        0xC000_0094 => "INTEGER_DIVIDE_BY_ZERO 整数除零",
        0xC000_00FD => "STACK_OVERFLOW 栈溢出",
        0xC000_0374 => "HEAP_CORRUPTION 堆损坏",
        0xC000_0409 => "STACK_BUFFER_OVERRUN fail-fast",
        0xE06D_7363 => "C++ 未捕获异常",
        0x8000_0003 => "BREAKPOINT 断点",
        _ => "未知异常",
    }
}

/* --------------------------------------------- Windows 事件日志 / Unity 日志 */

/// 查最近 N 分钟 Windows「应用程序」日志里与这些 exe 有关的崩溃 / 挂起事件。
///
/// 用 `wevtutil` 而不是直接上 `wevtapi`：只要一次字符串输出，省一大堆 unsafe COM
/// 调用；只在壁纸进程真的退出时调一次，几百毫秒的代价可以接受。
pub fn recent_application_crashes(exe_names: &[&str], minutes: u32) -> String {
    let names: Vec<String> = exe_names
        .iter()
        .filter(|name| !name.is_empty())
        .map(|name| name.to_ascii_lowercase())
        .collect();
    if names.is_empty() {
        return String::new();
    }

    let query = format!(
        "*[System[(EventID=1000 or EventID=1001 or EventID=1002) and TimeCreated[timediff(@SystemTime) <= {}]]]",
        minutes as u64 * 60_000
    );

    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let output = std::process::Command::new("wevtutil")
        .args([
            "qe",
            "Application",
            &format!("/q:{query}"),
            "/f:text",
            "/rd:true",
            "/c:40",
        ])
        .creation_flags(CREATE_NO_WINDOW)
        .output();

    let Ok(output) = output else {
        return String::new();
    };
    let text = decode_console(&output.stdout).replace("\r\n", "\n");

    let mut blocks = Vec::new();
    for block in text.split("\n\n") {
        let lower = block.to_ascii_lowercase();
        if names.iter().any(|name| lower.contains(name)) {
            let trimmed = block.trim();
            if !trimmed.is_empty() {
                blocks.push(trimmed.to_string());
            }
            if blocks.len() >= 8 {
                break;
            }
        }
    }
    blocks.join("\n\n")
}

/// `wevtutil` 的文本输出走控制台代码页（中文系统是 GBK），UTF-8 解不开就按 GBK 兜底。
fn decode_console(bytes: &[u8]) -> String {
    match std::str::from_utf8(bytes) {
        Ok(text) => text.to_string(),
        Err(_) => {
            let (decoded, _, _) = encoding_rs::GBK.decode(bytes);
            decoded.into_owned()
        }
    }
}

/// 收集 Unity 播放器的崩溃现场：`Player.log` 尾部 + 崩溃目录里的文件清单。
///
/// Unity 把日志放在 `%USERPROFILE%\AppData\LocalLow\<公司>\<产品>\Player.log`，
/// 公司和产品名来自 exe 同级 `<exe>_Data\app.info` 的前两行，所以能精确定位；
/// 找不到时再按「最近修改过的 Player.log」兜底。
pub fn unity_crash_artifacts(exe_path: &str) -> String {
    let exe = Path::new(exe_path);
    let mut out = String::new();

    if let Some((company, product)) = unity_app_info(exe) {
        out.push_str(&format!("Unity 产品: {company} / {product}\n"));
        let player_log = local_low().join(&company).join(&product).join("Player.log");
        match read_tail(&player_log, 512 * 1024) {
            Some(tail) if !tail.trim().is_empty() => {
                out.push_str(&format!(
                    "\n--- Player.log 尾部（{}） ---\n{}\n",
                    player_log.display(),
                    tail
                ));
            }
            _ => out.push_str(&format!("\n（没有找到 Player.log：{}）\n", player_log.display())),
        }
        out.push_str(&list_recent_files(
            &local_temp().join(&company).join(&product).join("Crashes"),
        ));
    } else {
        out.push_str("（没有读到 Unity 的 app.info，改用最近修改的 Player.log）\n");
        if let Some(path) = newest_player_log() {
            out.push_str(&format!("候选 Player.log: {}\n", path.display()));
            if let Some(tail) = read_tail(&path, 512 * 1024) {
                out.push_str(&tail);
                out.push('\n');
            }
        }
    }

    // 壁纸目录里也可能有 Unity 自己写的崩溃日志
    if let Some(dir) = exe.parent() {
        out.push_str(&list_recent_files(dir));
    }
    out
}

fn unity_app_info(exe: &Path) -> Option<(String, String)> {
    let stem = exe.file_stem()?.to_string_lossy().to_string();
    let info = exe.parent()?.join(format!("{stem}_Data")).join("app.info");
    let text = std::fs::read_to_string(&info).ok()?;
    let mut lines = text.lines().map(|line| line.trim().to_string());
    let company = lines.next().filter(|value| !value.is_empty())?;
    let product = lines.next().filter(|value| !value.is_empty())?;
    Some((company, product))
}

fn local_low() -> PathBuf {
    std::env::var_os("USERPROFILE")
        .map(|profile| PathBuf::from(profile).join("AppData").join("LocalLow"))
        .unwrap_or_else(|| std::env::temp_dir())
}

fn local_temp() -> PathBuf {
    std::env::temp_dir()
}

fn newest_player_log() -> Option<PathBuf> {
    let root = local_low();
    let mut best: Option<(SystemTime, PathBuf)> = None;
    let companies = std::fs::read_dir(&root).ok()?;
    for company in companies.flatten().take(64) {
        let company_path = company.path();
        if !company_path.is_dir() {
            continue;
        }
        let products = match std::fs::read_dir(&company_path) {
            Ok(products) => products,
            Err(_) => continue,
        };
        for product in products.flatten().take(64) {
            let log = product.path().join("Player.log");
            let modified = std::fs::metadata(&log).and_then(|meta| meta.modified()).ok();
            if let Some(modified) = modified {
                if best.as_ref().map(|(at, _)| modified > *at).unwrap_or(true) {
                    best = Some((modified, log));
                }
            }
        }
    }
    best.map(|(_, path)| path)
}

/// 列出目录里最近修改过的小文件（崩溃转储 / error.log 之类），最多 12 个。
fn list_recent_files(dir: &Path) -> String {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return String::new();
    };
    let mut files: Vec<(SystemTime, String, u64)> = Vec::new();
    for entry in entries.flatten().take(128) {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        if meta.len() > 512 * 1024 * 1024 {
            continue;
        }
        let modified = meta.modified().unwrap_or(UNIX_EPOCH);
        let name = path
            .file_name()
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_default();
        files.push((modified, name, meta.len()));
    }
    if files.is_empty() {
        return String::new();
    }
    files.sort_by(|a, b| b.0.cmp(&a.0));
    let mut out = format!("\n--- 目录文件（{}） ---\n", dir.display());
    for (modified, name, size) in files.into_iter().take(12) {
        out.push_str(&format!(
            "{}  {}  {} 字节\n",
            crate::library::iso8601(modified),
            name,
            size
        ));
    }
    out
}

/// 从文件末尾读最多 `max_bytes`，返回最后 160 行（够看崩溃前发生了什么）。
fn read_tail(path: &Path, max_bytes: u64) -> Option<String> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file = std::fs::File::open(path).ok()?;
    let length = file.metadata().ok()?.len();
    if length > max_bytes {
        file.seek(SeekFrom::End(-(max_bytes as i64))).ok()?;
    }
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).ok()?;
    let text = String::from_utf8_lossy(&bytes).into_owned();
    let lines: Vec<&str> = text.lines().collect();
    let start = lines.len().saturating_sub(160);
    Some(lines[start..].join("\n"))
}

/* --------------------------------------------------------------- 会话标记 */

/// `session.json`：正常退出时删除；下次启动还在说明上次是崩溃 / 被强杀。
pub fn session_file<R: Runtime>(app: &AppHandle<R>) -> PathBuf {
    app.path()
        .app_config_dir()
        .unwrap_or_else(|_| std::env::temp_dir())
        .join("session.json")
}

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct SessionMark {
    pid: u32,
    version: String,
    started_at_ms: u64,
}

pub fn mark_session_start<R: Runtime>(app: &AppHandle<R>) {
    let file = session_file(app);
    if let Some(dir) = file.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let mark = SessionMark {
        pid: std::process::id(),
        version: crate::APP_VERSION.to_string(),
        started_at_ms: now_ms() as u64,
    };
    if let Ok(text) = serde_json::to_string_pretty(&mark) {
        let _ = std::fs::write(&file, text);
    }
}

/// 读取并删除上次留下的会话标记；返回一段可读描述（说明上次没正常退出）。
pub fn take_previous_session<R: Runtime>(app: &AppHandle<R>) -> Option<String> {
    let file = session_file(app);
    let text = std::fs::read_to_string(&file).ok()?;
    let _ = std::fs::remove_file(&file);
    let mark: SessionMark = serde_json::from_str(&text).ok()?;
    let started = UNIX_EPOCH + Duration::from_millis(mark.started_at_ms);
    Some(format!(
        "上次运行没有正常退出（疑似崩溃或被强制结束）：PID {}，版本 {}，启动于 {}",
        mark.pid,
        mark.version,
        crate::library::iso8601(started)
    ))
}

pub fn clear_session<R: Runtime>(app: &AppHandle<R>) {
    let _ = std::fs::remove_file(session_file(app));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_codes_are_translated_to_something_readable() {
        assert!(describe_exit_code(Some(0)).contains("正常退出"));
        // 0xC0000005 是 Unity 崩溃最典型的退出码
        assert!(describe_exit_code(Some(0xC000_0005u32 as i32)).contains("ACCESS_VIOLATION"));
        assert!(describe_exit_code(Some(0xC000_0409u32 as i32)).contains("fail-fast"));
        assert!(describe_exit_code(None).contains("未知"));
    }

    #[test]
    fn native_exception_codes_are_named() {
        assert!(describe_ntstatus(0xC000_0005).contains("ACCESS_VIOLATION"));
        assert!(describe_ntstatus(0xC000_00FD).contains("STACK_OVERFLOW"));
    }

    #[test]
    fn console_output_falls_back_to_gbk() {
        let (bytes, _, _) = encoding_rs::GBK.encode("错误应用程序名称");
        assert_eq!(decode_console(&bytes), "错误应用程序名称");
        assert_eq!(decode_console(b"plain ascii"), "plain ascii");
    }

    #[test]
    fn the_trace_remembers_the_current_stage() {
        set_stage("单元测试阶段");
        assert_eq!(current_stage(), "单元测试阶段");
        assert!(trace_section().contains("单元测试阶段"));
    }

    #[test]
    fn rotated_writer_creates_and_rotates_the_file() {
        let dir = std::env::temp_dir().join(format!(
            "miside-crash-log-test-{}-{}",
            std::process::id(),
            now_ms()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        write_rotated(&dir, "probe.log", "first line");
        let text = std::fs::read_to_string(dir.join("probe.log")).unwrap();
        assert!(text.contains("first line"));

        // 超过上限后再写一次，应当轮转出 probe.log.1
        write_rotated(&dir, "probe.log", &"x".repeat(MAX_LOG_BYTES as usize + 8));
        write_rotated(&dir, "probe.log", "after rotation");
        assert!(dir.join("probe.log.1").exists());
        let fresh = std::fs::read_to_string(dir.join("probe.log")).unwrap();
        assert_eq!(fresh.trim(), "after rotation");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
