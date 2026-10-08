//! Tauri 命令、应用状态与后台扫描线程。
//!
//! 三条常驻线索：
//! * **扫描线程**（1.2s）—— 驱动壁纸进程（找窗口 / 挂 WorkerW / 暂停条件 / 音量 / 输入转发）、
//!   挑音频源（自动跟随或指定进程）并起停采集、刷新托盘；
//! * **音频推流线程**（`audio_link::spawn_pump`）—— 配置包 / 输入包 / 状态广播；
//! * **采集发射线程**（`capture`）—— 帧包与界面可视化事件。

use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager, Runtime, State};
use tauri_plugin_dialog::DialogExt;

use crate::audio_link::{self, AudioStatus};
use crate::capture::{self, ActiveCapture};
use crate::library::{self, WallpaperEntry};
use crate::pac::{self, PacLibrary};
use crate::prefs::{self, Settings};
use crate::sessions::{self, AudioTarget};
use crate::unity::{Mode, RuntimeState, UnityHost};
use crate::win::{control, desktop};

/// 运行状态广播。
pub const STATE_EVENT: &str = "wp://state";
/// 系统状态广播（全屏 / 电池 / 遮挡）。
pub const MONITOR_EVENT: &str = "wp://monitor";
/// 日志广播。
pub const LOG_EVENT: &str = "wp://log";

/// 后台扫描周期。
// 状态检测需要足够快地响应窗口切换；音频采集自身有独立的节流逻辑。
const SCAN_INTERVAL: Duration = Duration::from_millis(250);
/// 起流失败后的退避：别每 1.2 秒重拉一次被独占的设备。
const START_BACKOFF: Duration = Duration::from_secs(10);
/// 内存里保留的日志条数。
const MAX_LOGS: usize = 500;
/// 日志文件上限（超过就轮转一次）。
const MAX_LOG_BYTES: u64 = 2 * 1024 * 1024;

/* ------------------------------------------------------------------ 状态 */

/// 一条日志。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LogLine {
    /// `info` / `warn` / `error`
    pub level: String,
    pub message: String,
    /// Unix 毫秒。
    pub at: u64,
}

/// 系统状态广播负载（契约第 4 节）。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MonitorTick {
    pub attached: bool,
    pub occluded: bool,
    pub fullscreen: bool,
    pub on_battery: bool,
    pub foreground: bool,
    pub paused: bool,
    pub pid: u32,
    pub mode: String,
    pub monitor_index: i32,
}

/// 「关于」页要的信息。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AppInfo {
    pub version: String,
    pub library_dir: String,
    pub default_library_dir: String,
    pub wallpaper_count: usize,
    pub total_bytes: u64,
    pub dll_ok: bool,
    pub dll_version: u32,
    pub dll_path: String,
    pub autostart: bool,
}

/// 音频源的状态机：正在等谁、上次失败是什么时候。
#[derive(Debug, Default)]
struct SourceState {
    /// 上一次起流失败的进程名与时间（用于退避）。
    failed: Option<(String, Instant)>,
}

/// 全局状态。
pub struct AppState {
    settings: Mutex<Settings>,
    host: Mutex<UnityHost>,
    library: Mutex<Option<Arc<PacLibrary>>>,
    dll_error: Mutex<Option<String>>,
    capture: Mutex<Option<ActiveCapture>>,
    source: Mutex<SourceState>,
    logs: Mutex<Vec<LogLine>>,
}

impl Default for AppState {
    fn default() -> Self {
        Self {
            settings: Mutex::new(Settings::default()),
            host: Mutex::new(UnityHost::default()),
            library: Mutex::new(None),
            dll_error: Mutex::new(None),
            capture: Mutex::new(None),
            source: Mutex::new(SourceState::default()),
            logs: Mutex::new(Vec::new()),
        }
    }
}

impl AppState {
    /// 供托盘 / 扫描线程读取的当前设置。
    fn settings(&self) -> Settings {
        lock(&self.settings).clone()
    }

    fn set_settings(&self, settings: Settings) {
        *lock(&self.settings) = settings;
    }

    fn runtime(&self) -> RuntimeState {
        lock(&self.host).state()
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/* ------------------------------------------------------------------ 日志 */

/// 记一条日志：内存（给界面）+ 文件（给排查）+ 事件。
///
/// 对 runtime 泛型是为了能在 `cleanup_orphan` / `shutdown_cleanup` 这类泛型函数里复用。
pub fn log<R: Runtime>(app: &AppHandle<R>, level: &str, message: impl Into<String>) {
    let message = message.into();
    let line = LogLine {
        level: level.to_string(),
        message: message.clone(),
        at: now_ms(),
    };

    let state = app.state::<AppState>();
    {
        let mut logs = lock(&state.logs);
        logs.push(line.clone());
        let overflow = logs.len().saturating_sub(MAX_LOGS);
        if overflow > 0 {
            logs.drain(..overflow);
        }
    }

    let _ = app.emit(LOG_EVENT, &line);

    if let Ok(dir) = app.path().app_log_dir() {
        let _ = std::fs::create_dir_all(&dir);
        let file = dir.join("host.log");
        if std::fs::metadata(&file).map(|meta| meta.len()).unwrap_or(0) > MAX_LOG_BYTES {
            let rotated = dir.join("host.log.1");
            let _ = std::fs::remove_file(&rotated);
            let _ = std::fs::rename(&file, &rotated);
        }
        if let Ok(mut handle) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&file)
        {
            use std::io::Write;
            let _ = writeln!(
                handle,
                "[{}] {:<5} {}",
                library::iso8601(std::time::SystemTime::now()),
                level.to_uppercase(),
                message
            );
        }
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|value| value.as_millis() as u64)
        .unwrap_or(0)
}

/* ------------------------------------------------- 残留壁纸进程的清理 */

/// 「当前正在跑的壁纸进程」的落盘记录。
///
/// 存它是为了解决一个很恶心的情况：**宿主被强杀（任务管理器结束进程 / 崩溃）时，
/// 壁纸子进程不会跟着退**，它的窗口会一直挂在桌面层上 —— 用户在新实例里点「停止」
/// 是摘不掉的，因为那是**上一个实例**的进程。启动时凭这份记录把这个孤儿收掉。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RunningWallpaper {
    pid: u32,
    exe: String,
    started_at_ms: u64,
}

fn running_file<R: Runtime>(app: &AppHandle<R>) -> PathBuf {
    app.path()
        .app_config_dir()
        .unwrap_or_else(|_| std::env::temp_dir())
        .join("running.json")
}

/// 起流成功后登记一笔。
fn remember_running<R: Runtime>(app: &AppHandle<R>, snapshot: &RuntimeState) {
    if snapshot.pid == 0 {
        return;
    }
    let record = RunningWallpaper {
        pid: snapshot.pid,
        exe: snapshot.exe.clone(),
        started_at_ms: snapshot.started_at_ms,
    };
    let Ok(text) = serde_json::to_string_pretty(&record) else {
        return;
    };
    let file = running_file(app);
    if let Some(dir) = file.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(&file, text);
}

/// 壁纸已经正常结束，把登记抹掉。
fn forget_running<R: Runtime>(app: &AppHandle<R>) {
    let _ = std::fs::remove_file(running_file(app));
}

/// 启动时清理上一次运行留下的壁纸进程。
///
/// 只凭 PID 杀进程是有风险的（PID 会被系统复用），所以先核对**这个 PID 现在的可执行文件
/// 就是当初记录的那个 exe**，再动手：先把窗口从桌面层摘下来，再结束进程。
fn cleanup_orphan<R: Runtime>(app: &AppHandle<R>) {
    let file = running_file(app);
    let Ok(text) = std::fs::read_to_string(&file) else {
        return;
    };
    let _ = std::fs::remove_file(&file);
    let Ok(record) = serde_json::from_str::<RunningWallpaper>(&text) else {
        return;
    };
    if record.pid == 0 || !control::process_alive(record.pid) {
        return;
    }
    let still_ours = control::process_path(record.pid)
        .map(|path| control::same_executable(&path, &record.exe))
        .unwrap_or(false);
    if !still_ours {
        return;
    }

    if let Some(hwnd) = desktop::find_main_window(record.pid) {
        let _ = desktop::detach(hwnd);
    }
    let _ = control::resume_process(record.pid);
    match control::terminate_process(record.pid) {
        Ok(()) => log(
            app,
            "warn",
            format!(
                "已清理上次运行残留的壁纸进程（PID {}，{}）",
                record.pid, record.exe
            ),
        ),
        Err(err) => log(app, "error", format!("清理残留壁纸失败：{err}")),
    }
    std::thread::sleep(Duration::from_millis(200));
}

/// 退出前把壁纸收干净：结束进程 + 从桌面层摘窗口 + 清掉登记。
///
/// 每一步都留一条日志：退出路径上的代码一旦中途没跑完，光看"壁纸还在"是查不出来的
/// （实测踩过 —— 只看到入口那条日志，后面的都没了）。
pub fn shutdown_cleanup(app: &AppHandle) {
    let state = app.state::<AppState>();
    let pid = {
        let mut host = lock(&state.host);
        let pid = host.state().pid;
        let tracked = host.has_child();
        log(
            app,
            "info",
            format!("退出清理 · 开始：pid={pid} 有子进程句柄={tracked}"),
        );
        host.stop();
        log(app, "info", "退出清理 · host.stop() 已返回");
        pid
    };
    forget_running(app);
    log(
        app,
        "info",
        format!(
            "退出清理 · 登记已清除：pid={pid} 进程仍存活={}",
            control::process_alive(pid)
        ),
    );
}

/* --------------------------------------------------------- 采集内核加载 */

fn ensure_library(app: &AppHandle, state: &AppState) -> Result<Arc<PacLibrary>, String> {
    if let Some(library) = lock(&state.library).as_ref() {
        return Ok(Arc::clone(library));
    }
    let resource_dir = app.path().resource_dir().ok();
    let candidates = pac::candidate_paths(resource_dir.as_deref());

    let mut last_error = "未找到 ProcessAudioCapture.dll".to_string();
    for path in &candidates {
        match PacLibrary::load(path) {
            Ok(library) => {
                let library = Arc::new(library);
                *lock(&state.library) = Some(Arc::clone(&library));
                *lock(&state.dll_error) = None;
                return Ok(library);
            }
            Err(err) => last_error = err,
        }
    }

    *lock(&state.dll_error) = Some(last_error.clone());
    Err(format!(
        "{last_error}（请把 ProcessAudioCapture.dll 放到程序目录）"
    ))
}

/* ------------------------------------------------------------ 采集起停 */

/// 停掉当前采集会话。
fn stop_active(app: &AppHandle, state: &AppState, notify: bool) {
    let active = { lock(&state.capture).take() };
    if let Some(active) = active {
        let report = active.stop(app, notify);
        log(
            app,
            "info",
            format!(
                "停止采集 {}：{} 帧 / {} ms",
                report.process_name, report.total_frames, report.duration_ms
            ),
        );
    }
}

/// 起一个采集会话（阻塞，最长约 10 秒 —— 只在扫描线程或阻塞线程里调）。
fn start_active(
    app: &AppHandle,
    state: &AppState,
    target: &AudioTarget,
    settings: &Settings,
) -> Result<(), String> {
    stop_active(app, state, false);
    let library = ensure_library(app, state)?;
    let interval = capture::frame_interval_ms(settings.audio_frame_rate);
    let active = capture::start_capture(
        app.clone(),
        Arc::clone(&library),
        target.pid,
        target.process_name.clone(),
        interval,
    )?;

    log(
        app,
        "info",
        format!(
            "开始采集 {}（PID {}）· {} Hz / {} 声道",
            target.process_name,
            active.pid,
            active.counters.sample_rate.load(std::sync::atomic::Ordering::Relaxed),
            active.counters.channels.load(std::sync::atomic::Ordering::Relaxed),
        ),
    );
    *lock(&state.capture) = Some(active);
    audio_link::reset_source();
    Ok(())
}

/* ------------------------------------------------------------ 音频源同步 */

/// 每一轮扫描：决定采谁、要不要切换，并把结果写回音频状态。
fn sync_audio(app: &AppHandle, state: &AppState, settings: &Settings) {
    // 设置随时可能变（音源、端口、增益……），每次进来先同步一遍
    audio_link::configure(settings);

    let library = match ensure_library(app, state) {
        Ok(library) => library,
        Err(err) => {
            stop_active(app, state, false);
            audio_link::update_session(
                &settings.audio_source,
                "",
                0,
                false,
                "error",
                &err,
                false,
                0,
            );
            return;
        }
    };
    let dll_version = library.version();
    let dll_ok = library.has_extras();

    // 关掉音频 / 明确不采：直接停
    if !settings.audio_enabled || settings.audio_source == "off" {
        stop_active(app, state, false);
        audio_link::update_session(
            &settings.audio_source,
            "",
            0,
            false,
            "idle",
            if settings.audio_enabled {
                "音频源设为「关闭」"
            } else {
                "音频节奏已关闭"
            },
            dll_ok,
            dll_version,
        );
        return;
    }

    if !dll_ok {
        stop_active(app, state, false);
        audio_link::update_session(
            &settings.audio_source,
            "",
            0,
            false,
            "error",
            &format!("采集内核版本 {dll_version} 过旧，需要 v3 及以上"),
            false,
            dll_version,
        );
        return;
    }

    let targets = match sessions::list_targets(&library) {
        Ok(targets) => targets,
        Err(err) => {
            audio_link::update_session(
                &settings.audio_source,
                "",
                0,
                false,
                "error",
                &format!("枚举音频目标失败：{err}"),
                dll_ok,
                dll_version,
            );
            return;
        }
    };

    // 该采谁
    let mut message = String::new();
    let desired: Option<AudioTarget> = match settings.audio_source.as_str() {
        "process" => {
            if settings.audio_process.trim().is_empty() {
                message = "还没有选音频源".to_string();
                None
            } else {
                match sessions::find_by_name(&targets, &settings.audio_process) {
                    Some(target) => Some(target),
                    None => {
                        message = format!("等待 {} 出现…", settings.audio_process);
                        None
                    }
                }
            }
        }
        _ => match sessions::pick_loudest(&targets) {
            Some(target) => Some(target),
            None => {
                message = "等待有程序出声…".to_string();
                None
            }
        },
    };

    let current_pid = lock(&state.capture).as_ref().map(|active| active.pid);
    let current_dead = lock(&state.capture)
        .as_ref()
        .map(|active| !active.is_alive())
        .unwrap_or(false);

    match desired {
        Some(target) => {
            let same = current_pid == Some(target.pid) && !current_dead;
            if !same {
                // 退避：上一次为同一个进程起流失败过就先等一会儿
                let blocked = {
                    let source = lock(&state.source);
                    source
                        .failed
                        .as_ref()
                        .map(|(name, at)| {
                            name.eq_ignore_ascii_case(&target.process_name)
                                && at.elapsed() < START_BACKOFF
                        })
                        .unwrap_or(false)
                };
                if !blocked {
                    match start_active(app, state, &target, settings) {
                        Ok(()) => lock(&state.source).failed = None,
                        Err(err) => {
                            log(app, "error", format!("采集 {} 失败：{err}", target.process_name));
                            lock(&state.source).failed =
                                Some((target.process_name.clone(), Instant::now()));
                        }
                    }
                } else {
                    message = format!("{} 占用中，稍后重试…", target.process_name);
                }
            }
        }
        None => {
            if current_pid.is_some() {
                stop_active(app, state, false);
            }
        }
    }

    // 写回状态
    let (pid, name) = lock(&state.capture)
        .as_ref()
        .map(|active| (active.pid, active.process_name.clone()))
        .unwrap_or((0, String::new()));
    let running = pid != 0;
    if running {
        message = format!("正在采集 {name}");
    } else if message.is_empty() {
        message = "等待有程序出声…".to_string();
    }
    audio_link::update_session(
        &settings.audio_source,
        &name,
        pid,
        running,
        if running { "capturing" } else { "waiting" },
        &message,
        dll_ok,
        dll_version,
    );
}

/* ------------------------------------------------------------ 扫描线程 */

fn spawn_monitor(app: AppHandle) {
    std::thread::Builder::new()
        .name("wp-monitor".to_string())
        .spawn(move || loop {
            std::thread::sleep(SCAN_INTERVAL);
            scan_once(&app);
        })
        .expect("无法创建后台扫描线程");
}

fn scan_once(app: &AppHandle) {
    let state = app.state::<AppState>();
    let settings = state.settings();
    let system = control::system_state();

    // 1. 壁纸进程
    let (snapshot, auto_pause_note) = {
        let mut host = lock(&state.host);
        let was_auto_paused = host.state().auto_paused;
        host.tick(&settings, &system);
        let snapshot = host.state();
        // 自动暂停过去只改状态不写日志，"壁纸为什么自己停了"就只能靠猜 —— 补上原因
        let note = if snapshot.auto_paused != was_auto_paused {
            if snapshot.auto_paused {
                Some(format!("自动暂停壁纸：{}", host.pause_reason()))
            } else {
                Some("自动暂停已解除，壁纸继续".to_string())
            }
        } else {
            None
        };
        (snapshot, note)
    };
    if let Some(note) = auto_pause_note {
        log(app, "info", note);
    }
    audio_link::set_paused(snapshot.paused);
    audio_link::set_input_publishing(snapshot.input_forwarding && system.desktop_foreground);

    let _ = app.emit(STATE_EVENT, &snapshot);
    let _ = app.emit(
        MONITOR_EVENT,
        MonitorTick {
            attached: snapshot.attached,
            occluded: snapshot.occluded,
            fullscreen: system.fullscreen,
            on_battery: system.on_battery,
            foreground: system.foreground_fullscreen,
            paused: snapshot.paused,
            pid: snapshot.pid,
            mode: snapshot.mode.clone(),
            monitor_index: snapshot.monitor_index,
        },
    );

    // 2. 音频源
    sync_audio(app, &state, &settings);

    // 3. 托盘
    crate::tray::sync(app);
}

/* ------------------------------------------------------------------ 命令 */

#[tauri::command]
fn get_settings(state: State<'_, AppState>) -> Settings {
    state.settings()
}

/// 保存设置。
///
/// 前端送的是整份对象（后端 `normalized` 会夹一遍范围）。这里额外处理两件有副作用的事：
/// * 渲染倍率 / 显示器变了 —— 运行中的壁纸要重启才能生效；
/// * 开机自启变了 —— 同步注册表。
#[tauri::command]
fn save_settings(app: AppHandle, state: State<'_, AppState>, settings: Settings) -> Result<Settings, String> {
    let previous = state.settings();
    let next = settings.normalized();
    prefs::store(&app, &next)?;
    state.set_settings(next.clone());
    audio_link::configure(&next);
    prefs::broadcast(&app, &next);

    // 自定义参数变了就立刻补发一次配置包：主界面「点一次头」这类按钮靠它即时生效，
    // 否则最坏要等满一个 1 秒周期，点下去像是没反应。
    if previous.custom_params != next.custom_params {
        audio_link::flush_config();
    }

    if previous.auto_start != next.auto_start {
        match control::set_autostart(next.auto_start) {
            Ok(value) => log(
                &app,
                "info",
                format!("开机自启已{}", if value { "开启" } else { "关闭" }),
            ),
            Err(err) => log(&app, "warn", err),
        }
    }

    // 这几个是**启动参数**，改完必须重启壁纸进程才生效（渲染倍率虽然走配置包，
    // 但和分辨率一起改的时候顺手重启一次更符合直觉）
    let needs_restart = previous.render_scale != next.render_scale
        || previous.monitor_index != next.monitor_index
        || previous.graphics_api != next.graphics_api;
    if needs_restart {
        let mode = lock(&state.host).mode();
        if mode == Mode::Desktop {
            if let Some(entry) = current_entry(&app, &state) {
                let result = {
                    let mut host = lock(&state.host);
                    host.reload(&next, &entry)
                };
                match result {
                    Ok(snapshot) => {
                        let _ = app.emit(STATE_EVENT, &snapshot);
                        log(
                            &app,
                            "info",
                            format!(
                                "分辨率 / 显示器 / 图形 API 变了，已重启壁纸进程（图形 API = {}）",
                                next.graphics_api
                            ),
                        );
                    }
                    Err(err) => log(&app, "error", format!("重启壁纸失败：{err}")),
                }
            }
        }
    }

    Ok(next)
}

fn current_entry(app: &AppHandle, state: &AppState) -> Option<WallpaperEntry> {
    let settings = state.settings();
    if settings.active_wallpaper.is_empty() {
        return None;
    }
    library::find(app, &settings.active_wallpaper).ok()
}

#[tauri::command]
fn app_info(app: AppHandle, state: State<'_, AppState>) -> AppInfo {
    let settings = state.settings();
    let (count, total) = library::stats(&app);
    let default_dir = app
        .path()
        .app_config_dir()
        .unwrap_or_else(|_| std::env::temp_dir())
        .join("wallpapers");

    let loaded = lock(&state.library).clone();
    let (dll_ok, dll_version, dll_path) = match loaded {
        Some(library) => (
            library.has_extras(),
            library.version(),
            library.path.display().to_string(),
        ),
        None => (false, 0, String::new()),
    };

    AppInfo {
        version: crate::APP_VERSION.to_string(),
        library_dir: settings.library_root(&app).display().to_string(),
        default_library_dir: default_dir.display().to_string(),
        wallpaper_count: count,
        total_bytes: total,
        dll_ok,
        dll_version,
        dll_path,
        autostart: control::autostart_enabled(),
    }
}

#[tauri::command]
fn list_monitors() -> Vec<desktop::MonitorInfo> {
    desktop::monitors()
}

/* ------------------------------------------------------------- 壁纸库 */

#[tauri::command]
fn library_list(app: AppHandle, state: State<'_, AppState>) -> Vec<WallpaperEntry> {
    library::list(&app, &state.settings())
}

#[tauri::command(async)]
fn pick_zip_path(app: AppHandle) -> Option<String> {
    app.dialog()
        .file()
        .set_title("选择壁纸压缩包")
        .add_filter("壁纸压缩包", &["zip"])
        .blocking_pick_file()
        .and_then(|file| file.into_path().ok())
        .map(|path| path.display().to_string())
}

#[tauri::command(async)]
fn pick_library_dir(app: AppHandle) -> Option<String> {
    app.dialog()
        .file()
        .set_title("选择壁纸库目录")
        .blocking_pick_folder()
        .and_then(|file| file.into_path().ok())
        .map(|path| path.display().to_string())
}

#[tauri::command(async)]
fn import_zip(
    app: AppHandle,
    zip_path: String,
    name: Option<String>,
) -> Result<WallpaperEntry, String> {
    let state = app.state::<AppState>();
    let settings = state.settings();
    log(&app, "info", format!("开始导入：{zip_path}"));
    match library::import_zip(&app, &settings, &zip_path, name) {
        Ok(entry) => {
            log(
                &app,
                "info",
                format!(
                    "导入完成：{}（{} 个文件，{:.1} MB）",
                    entry.name,
                    entry.file_count,
                    entry.size_bytes as f64 / 1024.0 / 1024.0
                ),
            );
            Ok(entry)
        }
        Err(err) => {
            log(&app, "error", format!("导入失败：{err}"));
            Err(err)
        }
    }
}

#[tauri::command]
fn remove_wallpaper(
    app: AppHandle,
    state: State<'_, AppState>,
    id: String,
) -> Result<Vec<WallpaperEntry>, String> {
    let mut settings = state.settings();
    // 正在跑的壁纸先停掉，否则文件被占用删不干净
    if settings.active_wallpaper == id {
        let mut host = lock(&state.host);
        if host.mode() != Mode::Stopped {
            host.stop();
        }
        drop(host);
        forget_running(&app);
    }
    let result = library::remove(&app, &settings, &id);
    if result.is_ok() {
        // 删掉的正好是「当前壁纸」时把这个记忆也清掉：
        // 否则下次启动会拿一个已经不存在的 id 去自动应用，白报一次警告
        if settings.active_wallpaper == id {
            settings.active_wallpaper = String::new();
            let _ = prefs::store(&app, &settings);
            state.set_settings(settings.clone());
            prefs::broadcast(&app, &settings);
        }
        log(&app, "info", format!("已删除壁纸 {id}"));
    } else if let Err(err) = &result {
        log(&app, "error", format!("删除失败：{err}"));
    }
    result
}

#[tauri::command]
fn rename_wallpaper(
    app: AppHandle,
    state: State<'_, AppState>,
    id: String,
    name: String,
) -> Result<WallpaperEntry, String> {
    library::rename(&app, &state.settings(), &id, &name)
}

#[tauri::command]
fn reveal_wallpaper(app: AppHandle, id: String) -> Result<(), String> {
    let entry = library::find(&app, &id)?;
    let dir = if entry.dir.is_empty() {
        return Err("这个壁纸没有目录".to_string());
    } else {
        entry.dir
    };
    std::process::Command::new("explorer")
        .arg(dir)
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("打开资源管理器失败：{e}"))
}

/* --------------------------------------------------------------- 运行 */

/// 启动壁纸进程（`preview` = 普通窗口预览，`desktop` = 挂到桌面）。
fn launch(app: &AppHandle, state: &AppState, id: &str, mode: Mode) -> Result<RuntimeState, String> {
    let settings = state.settings();
    let entry = library::find(app, id)?;
    if !entry.is_runnable() {
        return Err(format!("壁纸「{}」的文件不完整，重新导入一次", entry.name));
    }
    let snapshot = {
        let mut host = lock(&state.host);
        host.start(&settings, &entry, mode)?
    };
    // 登记：万一宿主之后被强杀，下次启动能把这个进程收掉
    remember_running(app, &snapshot);

    // 记成当前壁纸（下次启动自动接着用）
    let mut next = state.settings();
    if next.active_wallpaper != entry.id {
        next.active_wallpaper = entry.id.clone();
        if let Err(err) = prefs::store(app, &next) {
            log(app, "warn", err);
        }
        state.set_settings(next.clone());
        prefs::broadcast(app, &next);
    }

    audio_link::reset_source();
    let _ = app.emit(STATE_EVENT, &snapshot);

    // 「渲染倍率」只在 D3D12 下有效（Unity 的动态分辨率在 Windows 独立平台仅支持 D3D12）。
    // 用户选了 D3D11 或自动却开了倍率时明确说一声，免得以为设置坏了。
    if settings.render_scale < 1.0 && settings.graphics_api != "d3d12" {
        log(
            app,
            "warn",
            "「渲染倍率」需要 DirectX 12：当前图形 API 不是 D3D12，这一项不会有效果（可在设置里把图形 API 选成 D3D12）。",
        );
    }

    log(
        app,
        "info",
        format!(
            "已{}：{}（PID {}）",
            if mode == Mode::Desktop {
                "应用到桌面"
            } else {
                "打开预览"
            },
            entry.name,
            snapshot.pid
        ),
    );
    Ok(snapshot)
}

#[tauri::command(async)]
fn preview_wallpaper(app: AppHandle, id: String) -> Result<RuntimeState, String> {
    let state = app.state::<AppState>();
    launch(&app, &state, &id, Mode::Preview)
}

#[tauri::command(async)]
fn apply_wallpaper(app: AppHandle, id: String) -> Result<RuntimeState, String> {
    let state = app.state::<AppState>();
    launch(&app, &state, &id, Mode::Desktop)
}

#[tauri::command]
fn stop_wallpaper(app: AppHandle, state: State<'_, AppState>) -> RuntimeState {
    {
        let mut host = lock(&state.host);
        host.stop();
    }
    forget_running(&app);
    let snapshot = state.runtime();
    let _ = app.emit(STATE_EVENT, &snapshot);
    log(&app, "info", "已停止壁纸");
    snapshot
}

#[tauri::command(async)]
fn reload_wallpaper(app: AppHandle) -> Result<RuntimeState, String> {
    let state = app.state::<AppState>();
    let settings = state.settings();
    let entry = library::find(&app, &settings.active_wallpaper)?;
    let snapshot = {
        let mut host = lock(&state.host);
        host.reload(&settings, &entry)?
    };
    audio_link::reset_source();
    let _ = app.emit(STATE_EVENT, &snapshot);
    log(&app, "info", format!("已重载壁纸：{}", entry.name));
    Ok(snapshot)
}

#[tauri::command]
fn toggle_pause(app: AppHandle, state: State<'_, AppState>) -> Result<RuntimeState, String> {
    let snapshot = {
        let mut host = lock(&state.host);
        host.toggle_pause()?
    };
    let _ = app.emit(STATE_EVENT, &snapshot);
    log(
        &app,
        "info",
        if snapshot.paused {
            "壁纸已暂停（进程挂起）"
        } else {
            "壁纸已继续"
        },
    );
    Ok(snapshot)
}

#[tauri::command]
fn set_volume(app: AppHandle, state: State<'_, AppState>, volume: f32) -> RuntimeState {
    let mut settings = state.settings();
    settings.volume = volume.clamp(0.0, 1.0);
    let _ = prefs::store(&app, &settings);
    state.set_settings(settings.clone());
    prefs::broadcast(&app, &settings);

    // 立刻生效一次，不等下一轮扫描
    let snapshot = state.runtime();
    if snapshot.pid != 0 {
        if let Err(err) = control::set_process_volume(snapshot.pid, Some(settings.volume), None) {
            log(&app, "warn", err);
        }
    }
    let mut snapshot = state.runtime();
    snapshot.volume = settings.volume;
    snapshot
}

#[tauri::command]
fn set_muted(app: AppHandle, state: State<'_, AppState>, muted: bool) -> RuntimeState {
    let mut settings = state.settings();
    settings.muted = muted;
    let _ = prefs::store(&app, &settings);
    state.set_settings(settings.clone());
    prefs::broadcast(&app, &settings);

    let snapshot = state.runtime();
    if snapshot.pid != 0 {
        if let Err(err) = control::set_process_volume(snapshot.pid, None, Some(muted)) {
            log(&app, "warn", err);
        }
    }
    let mut snapshot = state.runtime();
    snapshot.muted = muted;
    snapshot
}

#[tauri::command]
fn wallpaper_state(state: State<'_, AppState>) -> RuntimeState {
    state.runtime()
}

/// 「当前壁纸预览」用的缩略图宽度上限（再大对界面也没意义，IPC 却要搬更多字节）。
const THUMBNAIL_WIDTH: i32 = 720;

/// 主界面里那张实时壁纸预览图。
///
/// 返回裸二进制：前 8 字节 = `width u32 LE` + `height u32 LE`，其后是 RGBA 像素（自上而下）。
/// **空响应**表示「当前没有运行壁纸 / 已暂停 / 抓不到画面」——
/// 用 `PrintWindow` 抓 DirectX 独占渲染的窗口时经常只能拿到全黑，这种情况按抓不到处理，
/// 界面显示占位而不是一块黑屏。
#[tauri::command(async)]
fn wallpaper_thumbnail(state: State<'_, AppState>) -> tauri::ipc::Response {
    let (hwnd, paused) = {
        let host = lock(&state.host);
        (host.hwnd(), host.state().paused)
    };
    // 被挂起的进程不会响应 PrintWindow，硬抓会把宿主线程一起拖住
    if paused {
        return tauri::ipc::Response::new(Vec::new());
    }
    let Some(hwnd) = hwnd else {
        return tauri::ipc::Response::new(Vec::new());
    };

    match desktop::capture_thumbnail(hwnd, THUMBNAIL_WIDTH) {
        Some(thumb) => {
            let mut payload = Vec::with_capacity(thumb.rgba.len() + 8);
            payload.extend_from_slice(&thumb.width.to_le_bytes());
            payload.extend_from_slice(&thumb.height.to_le_bytes());
            payload.extend_from_slice(&thumb.rgba);
            tauri::ipc::Response::new(payload)
        }
        None => tauri::ipc::Response::new(Vec::new()),
    }
}

/* ------------------------------------------- 给托盘 / 内部线程用的包装 */

/// 当前设置（托盘菜单要用）。
pub fn settings_of(app: &AppHandle) -> Settings {
    app.state::<AppState>().settings()
}

/// 直接覆盖内存里的设置（托盘改完设置后写回）。
pub fn set_settings_of(app: &AppHandle, settings: Settings) {
    app.state::<AppState>().set_settings(settings);
}

/// 当前运行状态。
pub fn runtime_of(app: &AppHandle) -> RuntimeState {
    app.state::<AppState>().runtime()
}

/// 暂停 / 继续（阻塞版本，托盘用）。
pub fn toggle_pause_blocking(app: &AppHandle) -> Result<RuntimeState, String> {
    let state = app.state::<AppState>();
    let snapshot = {
        let mut host = lock(&state.host);
        host.toggle_pause()?
    };
    let _ = app.emit(STATE_EVENT, &snapshot);
    Ok(snapshot)
}

/// 停止壁纸（阻塞版本，托盘用）。
pub fn stop_blocking(app: &AppHandle) {
    let state = app.state::<AppState>();
    {
        let mut host = lock(&state.host);
        host.stop();
    }
    forget_running(app);
    let _ = app.emit(STATE_EVENT, &state.runtime());
    log(app, "info", "托盘：已停止壁纸");
}

/// 按 id 应用到桌面（阻塞版本，托盘用）。
pub fn apply_blocking(app: &AppHandle, id: &str) -> Result<RuntimeState, String> {
    let state = app.state::<AppState>();
    launch(app, &state, id, Mode::Desktop)
}

/* --------------------------------------------------------------- 音频 */

#[tauri::command(async)]
fn list_audio_targets(app: AppHandle) -> Result<Vec<AudioTarget>, String> {
    let state = app.state::<AppState>();
    let library = ensure_library(&app, &state)?;
    sessions::list_targets(&library)
}

fn audio_status_now() -> AudioStatus {
    let mut status = audio_link::status();
    status.packets_sent = audio_link::packets_sent();
    status
}

#[tauri::command]
fn audio_status() -> AudioStatus {
    audio_status_now()
}

/// 壁纸端上报的参数能力快照（契约 2.8）。
///
/// 界面打开设置页 / 收到 `wp://param-report` 时来取。`ageMs` 为 `None` 表示
/// 从未收到过上报 —— 要么壁纸端没跑，要么没实现上报，要么上报端口没通。
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct ParamReportSnapshot {
    client: String,
    params: Vec<audio_link::ParamReport>,
    age_ms: Option<u64>,
}

#[tauri::command]
fn param_report() -> ParamReportSnapshot {
    let report = audio_link::param_report().unwrap_or_default();
    ParamReportSnapshot {
        client: report.client,
        params: report.params,
        age_ms: audio_link::report_age_ms(),
    }
}

/// 指定音频源（空串 = 回到自动跟随最响的进程）。
#[tauri::command]
fn set_audio_target(app: AppHandle, state: State<'_, AppState>, process_name: String) -> AudioStatus {
    let mut settings = state.settings();
    let trimmed = process_name.trim().to_lowercase();
    if trimmed.is_empty() {
        settings.audio_source = "auto".to_string();
        settings.audio_process = String::new();
    } else {
        settings.audio_source = "process".to_string();
        settings.audio_process = trimmed.clone();
    }
    let settings = settings.normalized();
    let _ = prefs::store(&app, &settings);
    state.set_settings(settings.clone());
    audio_link::configure(&settings);
    prefs::broadcast(&app, &settings);
    audio_link::reset_source();
    log(
        &app,
        "info",
        if trimmed.is_empty() {
            "音频源：自动跟随最响的进程".to_string()
        } else {
            format!("音频源：{trimmed}")
        },
    );
    audio_status_now()
}

#[tauri::command]
fn set_audio_enabled(app: AppHandle, state: State<'_, AppState>, enabled: bool) -> AudioStatus {
    let mut settings = state.settings();
    settings.audio_enabled = enabled;
    let _ = prefs::store(&app, &settings);
    state.set_settings(settings.clone());
    audio_link::configure(&settings);
    prefs::broadcast(&app, &settings);
    log(
        &app,
        "info",
        if enabled {
            "音频节奏推送已开启"
        } else {
            "音频节奏推送已关闭"
        },
    );
    audio_status_now()
}

/* --------------------------------------------------------------- 系统 */

#[tauri::command]
fn set_autostart(app: AppHandle, state: State<'_, AppState>, enabled: bool) -> Result<bool, String> {
    let value = control::set_autostart(enabled)?;
    let mut settings = state.settings();
    settings.auto_start = value;
    let _ = prefs::store(&app, &settings);
    state.set_settings(settings.clone());
    prefs::broadcast(&app, &settings);
    log(
        &app,
        "info",
        format!("开机自启已{}", if value { "开启" } else { "关闭" }),
    );
    Ok(value)
}

#[tauri::command]
fn open_log_dir(app: AppHandle) -> Result<(), String> {
    let dir: PathBuf = app
        .path()
        .app_log_dir()
        .map_err(|e| format!("取不到日志目录：{e}"))?;
    std::fs::create_dir_all(&dir).map_err(|e| format!("创建日志目录失败：{e}"))?;
    std::process::Command::new("explorer")
        .arg(&dir)
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("打开日志目录失败：{e}"))
}

/* ------------------------------------------------------------------ 入口 */

/// 构建并运行应用。
pub fn run_app() {
    let app = tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.unminimize();
                let _ = window.show();
                let _ = window.set_focus();
            }
        }))
        .plugin(tauri_plugin_dialog::init())
        .manage(AppState::default())
        .invoke_handler(tauri::generate_handler![
            get_settings,
            save_settings,
            app_info,
            list_monitors,
            library_list,
            pick_zip_path,
            pick_library_dir,
            import_zip,
            remove_wallpaper,
            rename_wallpaper,
            reveal_wallpaper,
            preview_wallpaper,
            apply_wallpaper,
            stop_wallpaper,
            reload_wallpaper,
            toggle_pause,
            set_volume,
            set_muted,
            wallpaper_state,
            wallpaper_thumbnail,
            list_audio_targets,
            audio_status,
            param_report,
            set_audio_target,
            set_audio_enabled,
            set_autostart,
            open_log_dir,
        ])
        .setup(|app| {
            let handle = app.handle().clone();
            let settings = prefs::load(&handle);
            app.state::<AppState>().set_settings(settings.clone());

            // 音频链路与推流线程
            audio_link::configure(&settings);
            audio_link::spawn_pump(handle.clone());
            // 反向通道：接收壁纸端上报的参数能力（服装列表等）
            audio_link::spawn_report_listener(handle.clone());

            // 托盘
            if let Err(err) = crate::tray::setup(&handle) {
                log(&handle, "warn", format!("托盘创建失败：{err}"));
            }

            // 后台扫描
            spawn_monitor(handle.clone());

            log(
                &handle,
                "info",
                format!(
                    "miside-wallpaper-engine v{} 启动（Wallpaper Engine 式设置 + 音频节奏推流）",
                    crate::APP_VERSION
                ),
            );

            // 上次运行如果被强杀（任务管理器结束进程 / 崩溃），壁纸子进程会残留并一直挂在
            // 桌面层上 —— 先把它收掉，否则这一轮点「停止」永远摘不掉那层壁纸
            cleanup_orphan(&handle);

            // 开机自启：以设置文件为准，自己纠正注册表（用户手工删过也能恢复）
            let registry = control::autostart_enabled();
            if registry != settings.auto_start {
                if let Err(err) = control::set_autostart(settings.auto_start) {
                    log(&handle, "warn", err);
                }
            }

            // 自动应用上次的壁纸：放到后台线程，别拖慢启动
            if settings.auto_apply && !settings.active_wallpaper.is_empty() {
                let handle = handle.clone();
                let id = settings.active_wallpaper.clone();
                std::thread::spawn(move || {
                    std::thread::sleep(Duration::from_millis(800));
                    let state = handle.state::<AppState>();
                    if let Err(err) = launch(&handle, &state, &id, Mode::Desktop) {
                        log(&handle, "warn", format!("自动应用上次的壁纸失败：{err}"));
                    }
                });
            }

            Ok(())
        })
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                let app = window.app_handle();
                let close_to_tray = app.state::<AppState>().settings().close_to_tray;
                if close_to_tray {
                    // 点 × 只是收进托盘：壁纸和音频采集继续跑
                    api.prevent_close();
                    let _ = window.hide();
                    crate::tray::sync(app);
                } else {
                    // 这次是真的要退出：在窗口关掉之前先把壁纸收了，
                    // 别指望退出事件一定会到（实测"关窗口退出"这条路径上不一定触发）
                    cleanup_once(app, "CloseRequested");
                }
            }
        })
        .build(tauri::generate_context!())
        .expect("构建 Tauri 应用失败");

    app.run(|handle, event| {
        // 退出前把壁纸收干净：结束进程 + 从桌面层摘窗口 + 清登记。
        // 少了这一步，用户"退出程序"之后桌面上会留下一层摘不掉的壁纸
        // （新实例还能靠 running.json 兜底，但用户不该看到那一层）。
        //
        // `ExitRequested` 与 `Exit` 都挂上：不同退出路径（关掉最后一个窗口 / 托盘"退出" /
        // 系统关机）触发的事件不完全一样，实测只挂 `Exit` 时"关窗口退出"这条路径不会执行到，
        // 所以两个都挂，并靠 `cleanup_once` 保证只做一次。
        match event {
            tauri::RunEvent::ExitRequested { .. } => cleanup_once(handle, "ExitRequested"),
            tauri::RunEvent::Exit => cleanup_once(handle, "Exit"),
            _ => {}
        }
    });
}

/// 退出清理只做一次（两个退出事件可能都到）。
fn cleanup_once(app: &AppHandle, reason: &str) {
    use std::sync::atomic::{AtomicBool, Ordering};
    static CLEANED: AtomicBool = AtomicBool::new(false);
    if CLEANED.swap(true, Ordering::SeqCst) {
        return;
    }
    let running = app.state::<AppState>().runtime();
    log(
        app,
        "info",
        format!(
            "退出清理（{reason}）：壁纸 PID {}",
            if running.pid == 0 {
                "无".to_string()
            } else {
                running.pid.to_string()
            }
        ),
    );
    shutdown_cleanup(app);
}
