//! Unity exe 的宿主：启动参数、进程生命周期、暂停 / 音量 / 尺寸。
//!
//! Unity 程序没有控制接口，所以：
//! * 尺寸靠启动参数（`-screen-width/-screen-height/-screen-fullscreen 0/-popupwindow`），
//!   运行中改「渲染倍率」只能重载；
//! * 帧率靠给 Unity 发配置包，由它在运行时 `Application.targetFrameRate`；
//! * 音量 / 静音走 WASAPI 会话音量（见 `win::control`）；
//! * 暂停 / 继续走进程挂起（`NtSuspendProcess`）。
//!
//! 挂到 WorkerW 的动作**不在这里等待窗口**：`start` 立刻返回，窗口由后台扫描线程
//! 发现后挂载 —— 免得界面按钮卡住十几秒。

use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::Serialize;
use windows::Win32::Foundation::HWND;

use crate::library::WallpaperEntry;
use crate::prefs::Settings;
use crate::win::{control, desktop};

/// 不带控制台窗口启动子进程（GUI 子系统本来就没有，控制台子系统的构建也不会闪一个黑框）。
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// 音量重试的最小间隔：Unity 起流前没有音频会话，别每 tick 都去枚举一遍 WASAPI。
const VOLUME_RETRY: Duration = Duration::from_millis(2000);

/// 运行模式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Stopped,
    Preview,
    Desktop,
}

impl Mode {
    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Stopped => "stopped",
            Mode::Preview => "preview",
            Mode::Desktop => "desktop",
        }
    }
}

/// 运行状态（契约 2.2）。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeState {
    pub mode: String,
    pub pid: u32,
    pub hwnd: i64,
    pub wallpaper_id: String,
    pub exe: String,
    pub attached: bool,
    pub paused: bool,
    pub user_paused: bool,
    pub auto_paused: bool,
    pub muted: bool,
    pub volume: f32,
    pub monitor_index: i32,
    pub started_at_ms: u64,
    pub memory_mb: f32,
    pub occluded: bool,
    pub input_forwarding: bool,
    pub last_error: String,
}

impl Default for RuntimeState {
    fn default() -> Self {
        Self {
            mode: Mode::Stopped.as_str().to_string(),
            pid: 0,
            hwnd: 0,
            wallpaper_id: String::new(),
            exe: String::new(),
            attached: false,
            paused: false,
            user_paused: false,
            auto_paused: false,
            muted: false,
            volume: 1.0,
            monitor_index: -1,
            started_at_ms: 0,
            memory_mb: 0.0,
            occluded: false,
            input_forwarding: false,
            last_error: String::new(),
        }
    }
}

/// 当前跑着的壁纸进程 + 它的状态。
pub struct UnityHost {
    child: Option<Child>,
    mode: Mode,
    state: RuntimeState,
    /// 桌面模式下铺满的区域（屏幕坐标）。
    rect: (i32, i32, i32, i32),
    /// 最近一次向 WASAPI 下发过的音量 / 静音，用来避免每 tick 重复设置。
    applied_volume: Option<f32>,
    applied_muted: Option<bool>,
    last_volume_attempt: Option<Instant>,
    /// 预览窗口是否已经摆好位置（摆过一次就不再动它）。
    positioned: bool,
    /// 「宿主没了，壁纸也得跟着走」的 Job（见 `win::control::KillOnCloseJob`）。
    job: Option<control::KillOnCloseJob>,
    /// 自动暂停条件的去抖（见 [`PauseDebounce`]）。
    pause_debounce: PauseDebounce,
    /// 当前自动暂停的原因（没暂停时为空串），日志与界面都用它回答"为什么停了"。
    pause_reason: String,
}

/// 自动暂停条件的去抖：条件要**持续**满足 `hold` 才真的暂停。
///
/// 为什么需要去抖：`SHQueryUserNotificationState` 这类系统信号本身会抖 —— 切换前台窗口
/// （点别的程序、Alt+Tab、开开始菜单）的瞬间，外壳会短暂报 `QUNS_BUSY`。条件一抖就挂起进程，
/// 用户看到的就是"一切换到别的程序，壁纸就停一下，一秒后又自己回来"。
/// 真正玩全屏游戏时条件会一直成立，所以 3 秒的等待完全不影响该有的效果。
struct PauseDebounce {
    since: Option<Instant>,
    hold: Duration,
}

impl PauseDebounce {
    fn new(hold: Duration) -> Self {
        Self { since: None, hold }
    }

    /// 喂入当前条件，返回"现在可以暂停了吗"。
    fn update(&mut self, condition: bool, now: Instant) -> bool {
        if !condition {
            self.since = None;
            return false;
        }
        let since = *self.since.get_or_insert(now);
        now.duration_since(since) >= self.hold
    }

    fn reset(&mut self) {
        self.since = None;
    }
}

/// 自动暂停前，条件需要持续成立多久。
const PAUSE_HOLD: Duration = Duration::from_secs(3);

impl Default for UnityHost {
    fn default() -> Self {
        Self {
            child: None,
            mode: Mode::Stopped,
            state: RuntimeState::default(),
            rect: (0, 0, 0, 0),
            applied_volume: None,
            applied_muted: None,
            last_volume_attempt: None,
            positioned: false,
            job: None,
            pause_debounce: PauseDebounce::new(PAUSE_HOLD),
            pause_reason: String::new(),
        }
    }
}

impl UnityHost {
    pub fn state(&self) -> RuntimeState {
        self.state.clone()
    }

    /// 当前自动暂停的原因（没自动暂停时为空串）——"壁纸为什么停了"的答案。
    pub fn pause_reason(&self) -> &str {
        &self.pause_reason
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }

    pub fn hwnd(&self) -> Option<HWND> {
        if self.state.hwnd == 0 {
            None
        } else {
            Some(HWND(self.state.hwnd as *mut std::ffi::c_void))
        }
    }

    /// 是否还握着子进程句柄（退出清理的日志用来看"到底有没有进程要收"）。
    pub fn has_child(&self) -> bool {
        self.child.is_some()
    }

    /// 启动壁纸进程。`Mode::Desktop` 只负责起进程，挂载交给后台扫描线程。
    pub fn start(
        &mut self,
        settings: &Settings,
        entry: &WallpaperEntry,
        mode: Mode,
    ) -> Result<RuntimeState, String> {
        self.stop();

        if !entry.is_runnable() {
            return Err(format!("壁纸「{}」的主程序不见了，重新导入一次", entry.name));
        }
        let exe = PathBuf::from(&entry.exe);
        let work_dir = exe
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));

        let rect = match mode {
            Mode::Desktop => desktop::target_rect(settings.monitor_index),
            _ => desktop::target_rect(0),
        };
        let args = build_args(settings, mode, rect);
        let mut command = Command::new(&exe);
        command
            .args(&args)
            // Unity 靠工作目录找 `*_Data`，不在 exe 同级启动会直接黑屏退出
            .current_dir(&work_dir)
            .creation_flags(CREATE_NO_WINDOW);

        let child = command
            .spawn()
            .map_err(|e| format!("启动 {} 失败：{e}", exe.display()))?;

        let pid = child.id();

        // 把子进程放进「句柄关闭即结束」的 Job：宿主之后无论是正常退出、崩溃还是被
        // 任务管理器强杀，内核都会把壁纸一起带走，不会留下摘不掉的孤儿窗口。
        // Job 只建一次（一个宿主的所有壁纸都归它管）。
        if self.job.is_none() {
            self.job = control::KillOnCloseJob::create();
        }
        if let Some(job) = self.job.as_ref() {
            if let Err(err) = job.assign(pid) {
                // 老系统可能不允许嵌套 Job：不致命，还有退出清理与启动清理兜底
                eprintln!("[miside-wallpaper-engine] {err}");
            }
        }

        self.mode = mode;
        self.child = Some(child);
        self.rect = (rect.left, rect.top, rect.right - rect.left, rect.bottom - rect.top);
        self.applied_volume = None;
        self.applied_muted = None;
        self.positioned = false;
        // 新一次起流从干净状态开始：上一个壁纸留下的暂停条件不该顺延过来
        self.pause_debounce.reset();
        self.pause_reason.clear();
        self.state = RuntimeState {
            mode: mode.as_str().to_string(),
            pid,
            hwnd: 0,
            wallpaper_id: entry.id.clone(),
            exe: entry.exe.clone(),
            attached: false,
            paused: false,
            user_paused: false,
            auto_paused: false,
            muted: settings.muted,
            volume: settings.volume,
            monitor_index: settings.monitor_index,
            started_at_ms: now_ms(),
            memory_mb: 0.0,
            occluded: false,
            input_forwarding: false,
            last_error: String::new(),
        };
        Ok(self.state())
    }

    /// 结束壁纸进程，并确保它的窗口**一定**从桌面层摘下来。
    ///
    /// 这里要防的是「停止后壁纸还在桌面上」：只要窗口还挂在 WorkerW 上，进程又没能杀掉，
    /// 用户就会看到一层摘不掉的壁纸。所以：
    /// * 记录的 `hwnd` 可能还是 0（进程刚起来就被停），那就按 PID 再找一次主窗口；
    /// * `kill` 之后確認进程真的没了，还在就用 PID 强杀一次。
    pub fn stop(&mut self) {
        let pid = self.state.pid;
        let hwnd = self.hwnd().or_else(|| {
            if pid == 0 {
                None
            } else {
                desktop::find_main_window(pid)
            }
        });

        // 先停输入转发：它要把被壁纸窗口抢走的鼠标捕获还给桌面，而这件事必须在
        // 窗口被摘下来 / 进程被杀掉**之前**做（窗口没了就无从判断捕获在谁手里）。
        crate::win::input::clear_target();

        if let Some(hwnd) = hwnd {
            let _ = desktop::detach(hwnd);
        }

        if let Some(mut child) = self.child.take() {
            // 挂起状态下的进程也要能杀掉：先尽力恢复
            let _ = control::resume_process(child.id());
            let _ = child.kill();
            let _ = child.wait();
        }

        // `Child::kill` 失败（进程被别的句柄挡住、刚变成僵尸等）时兜底强杀
        if pid != 0 && control::process_alive(pid) {
            let _ = control::terminate_process(pid);
        }

        self.mode = Mode::Stopped;
        self.state = RuntimeState::default();
        self.applied_volume = None;
        self.applied_muted = None;
        self.positioned = false;
        self.pause_debounce.reset();
        self.pause_reason.clear();
    }

    /// 重启当前壁纸（改渲染倍率、换显示器、进程卡死都靠它）。
    pub fn reload(
        &mut self,
        settings: &Settings,
        entry: &WallpaperEntry,
    ) -> Result<RuntimeState, String> {
        let mode = if self.mode == Mode::Stopped {
            Mode::Desktop
        } else {
            self.mode
        };
        self.start(settings, entry, mode)
    }

    /// 用户手动暂停 / 继续。
    pub fn toggle_pause(&mut self) -> Result<RuntimeState, String> {
        if self.mode == Mode::Stopped {
            return Ok(self.state.clone());
        }
        self.state.user_paused = !self.state.user_paused;
        self.apply_pause()?;
        Ok(self.state.clone())
    }

    fn apply_pause(&mut self) -> Result<(), String> {
        let should_pause = self.state.user_paused || self.state.auto_paused;
        if should_pause == self.state.paused {
            return Ok(());
        }
        if should_pause {
            control::suspend_process(self.state.pid)?;
        } else {
            control::resume_process(self.state.pid)?;
        }
        self.state.paused = should_pause;
        Ok(())
    }

    /* --------------------------------------------------------- 后台同步 */

    /// 后台扫描线程每 tick 调一次：找窗口、挂载、同步几何、应用暂停条件与音量。
    pub fn tick(&mut self, settings: &Settings, system: &control::SystemState) {
        if self.mode == Mode::Stopped || self.state.pid == 0 {
            return;
        }

        // 1. 进程还在吗
        if !control::process_alive(self.state.pid) {
            let was_desktop = self.mode == Mode::Desktop;
            self.stop();
            self.state.last_error = if was_desktop {
                "壁纸进程自己退出了".to_string()
            } else {
                "预览窗口已关闭".to_string()
            };
            return;
        }

        // 2. 窗口出现没有（Unity 启动要几秒）
        if self.state.hwnd == 0 {
            if let Some(hwnd) = desktop::find_main_window(self.state.pid) {
                self.state.hwnd = hwnd.0 as i64;
            }
        }

        // 3. 桌面模式：挂上 WorkerW；掉了就重挂（explorer 重启、Win+D 之类）
        if self.mode == Mode::Desktop {
            if let Some(hwnd) = self.hwnd() {
                if !desktop::is_attached(hwnd) {
                    match desktop::attach(hwnd, settings.monitor_index) {
                        Ok(()) => {
                            self.state.attached = true;
                            self.state.last_error.clear();
                            let rect = desktop::target_rect(settings.monitor_index);
                            self.rect = (
                                rect.left,
                                rect.top,
                                rect.right - rect.left,
                                rect.bottom - rect.top,
                            );
                        }
                        Err(err) => self.state.last_error = err,
                    }
                } else {
                    self.state.attached = true;
                    // 旧版本挂上去的窗口可能没有「不可激活」标记（那时候还没有这个设计），
                    // 补一次，否则它仍能当前台窗口、把桌面图标顶成非激活状态
                    desktop::enforce_no_activate(hwnd);
                    // 显示器设置变了就按新几何摆一次（尺寸永远是整块屏，渲染倍率不在这里）
                    let rect = desktop::target_rect(settings.monitor_index);
                    let wanted = (rect.left, rect.top, rect.right - rect.left, rect.bottom - rect.top);
                    if wanted != self.rect {
                        self.rect = wanted;
                        let _ = desktop::move_window(hwnd, rect);
                    }
                }
            }
        } else if let Some(hwnd) = self.hwnd() {
            // 预览模式：只在第一次发现窗口时摆到屏幕中间。
            // 每轮都摆会把用户拖走的预览窗口一次次拽回原位。
            if !self.positioned {
                let rect = preview_rect(settings);
                if desktop::move_window(hwnd, rect).is_ok() {
                    self.rect = (rect.left, rect.top, rect.right - rect.left, rect.bottom - rect.top);
                    self.positioned = true;
                }
            }
        }

        // 4. 暂停条件：先弄清"为什么"，日志里要能直接看出是谁按的暂停
        let mut reason = "";
        if settings.pause_on_fullscreen && system.fullscreen {
            reason = "检测到全屏应用";
        } else if settings.pause_on_battery && system.on_battery {
            reason = "正在使用电池";
        } else if settings.pause_when_foreground && system.foreground_fullscreen {
            reason = "前台窗口铺满了屏幕";
        }

        // 去抖：条件要持续成立才挂起，避免切窗口时外壳报忙把壁纸停一下
        let auto = self.pause_debounce.update(!reason.is_empty(), Instant::now());
        if auto {
            self.pause_reason = reason.to_string();
        } else {
            self.pause_reason.clear();
        }
        self.state.auto_paused = auto && !self.state.user_paused;
        self.state.occluded = system.fullscreen;
        let _ = self.apply_pause();

        // 5. 音量 / 静音（会话还没出现就过两秒再试）
        let volume_changed = self.applied_volume != Some(settings.volume);
        let muted_changed = self.applied_muted != Some(settings.muted);
        let retry_due = self
            .last_volume_attempt
            .map(|at| at.elapsed() >= VOLUME_RETRY)
            .unwrap_or(true);
        if (volume_changed || muted_changed) && retry_due {
            self.last_volume_attempt = Some(Instant::now());
            let volume = volume_changed.then_some(settings.volume);
            let muted = muted_changed.then_some(settings.muted);
            match control::set_process_volume(self.state.pid, volume, muted) {
                Ok(true) => {
                    self.applied_volume = Some(settings.volume);
                    self.applied_muted = Some(settings.muted);
                }
                Ok(false) => {
                    // 还没有音频会话：状态先按用户设置显示，下一次 tick 再试
                }
                Err(err) => self.state.last_error = err,
            }
        }
        self.state.volume = settings.volume;
        self.state.muted = settings.muted;

        // 6. 输入转发
        let forwarding = settings.input_forward
            && !settings.input_locked
            && self.mode == Mode::Desktop;
        if forwarding {
            if let Some(hwnd) = self.hwnd() {
                let _ = crate::win::input::set_target(hwnd.0 as isize, true, true);
            }
        } else if crate::win::input::is_active() {
            crate::win::input::clear_target();
        }
        self.state.input_forwarding = forwarding;

        // 7. 内存占用（每 tick 采一次，界面上能看到它吃多少）
        self.state.memory_mb = control::process_memory_mb(self.state.pid);
    }
}

/// 拼启动参数。
///
/// Unity 独立播放器认这些命令行参数（`-screen-width/-screen-height/-screen-fullscreen`）。
/// **窗口尺寸永远是整块显示器**：渲染倍率不走这里 —— 把窗口改小只会让壁纸缩在屏幕左上角，
/// 正确的做法是窗口照旧铺满、由 Unity 端用 `ScalableBufferManager.ResizeBuffers` 降内部分辨率，
/// 所以倍率是通过配置包下发的（见 `audio_link::config_json` 的 `renderScale`）。
pub fn build_args(settings: &Settings, mode: Mode, rect: windows::Win32::Foundation::RECT) -> Vec<String> {
    let mut width = (rect.right - rect.left).max(320);
    let mut height = (rect.bottom - rect.top).max(240);
    if mode != Mode::Desktop {
        let preview = preview_rect(settings);
        width = preview.right - preview.left;
        height = preview.bottom - preview.top;
    }

    let mut args = vec![
        "-screen-fullscreen".to_string(),
        "0".to_string(),
        "-screen-width".to_string(),
        width.max(320).to_string(),
        "-screen-height".to_string(),
        height.max(240).to_string(),
    ];
    if mode == Mode::Desktop {
        // 无边框窗口：挂到壁纸层之后不该有任何装饰
        args.push("-popupwindow".to_string());
    }

    // 图形 API 由用户选（默认让 Unity 自己挑）。
    // D3D12 的意义在于：Unity 的动态分辨率（ScalableBufferManager）在 Windows 独立平台上
    // 只支持 D3D12，也就是壁纸设置里的「渲染倍率」只有在 D3D12 下才会真的生效。
    match settings.graphics_api.as_str() {
        "d3d11" => args.push("-force-d3d11".to_string()),
        "d3d12" => args.push("-force-d3d12".to_string()),
        _ => {}
    }

    // 用户附加参数放在最后：想覆盖上面的选择（比如自己写 -force-d3d11）时以它为准
    args.extend(
        settings
            .extra_args
            .split_whitespace()
            .filter(|part| !part.is_empty())
            .map(|part| part.to_string()),
    );
    args
}

/// 预览窗口的屏幕矩形：主显示器居中。
pub fn preview_rect(settings: &Settings) -> windows::Win32::Foundation::RECT {
    let primary = desktop::monitors()
        .into_iter()
        .find(|monitor| monitor.primary)
        .or_else(|| desktop::monitors().into_iter().next());
    let (screen_w, screen_h, origin_x, origin_y) = primary
        .map(|monitor| (monitor.width, monitor.height, monitor.x, monitor.y))
        .unwrap_or((1920, 1080, 0, 0));

    let width = settings.preview_width.min(screen_w as u32) as i32;
    let height = settings.preview_height.min(screen_h as u32) as i32;
    let left = origin_x + (screen_w - width) / 2;
    let top = origin_y + (screen_h - height) / 2;
    windows::Win32::Foundation::RECT {
        left,
        top,
        right: left + width,
        bottom: top + height,
    }
}

/// 按倍率缩放一个屏幕矩形（围绕它的左上角）。
///
/// 目前只用于「预览窗口在超大虚拟桌面上的兜底」，壁纸路径**不再**用它 —— 见 `build_args`。
#[allow(dead_code)]
pub fn scaled_rect(
    rect: windows::Win32::Foundation::RECT,
    scale: f32,
) -> windows::Win32::Foundation::RECT {
    let width = ((rect.right - rect.left) as f32 * scale).round() as i32;
    let height = ((rect.bottom - rect.top) as f32 * scale).round() as i32;
    windows::Win32::Foundation::RECT {
        left: rect.left,
        top: rect.top,
        right: rect.left + width.max(2),
        bottom: rect.top + height.max(2),
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|value| value.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(width: i32, height: i32) -> windows::Win32::Foundation::RECT {
        windows::Win32::Foundation::RECT {
            left: 0,
            top: 0,
            right: width,
            bottom: height,
        }
    }

    #[test]
    fn desktop_args_always_fill_the_whole_monitor() {
        let settings = Settings {
            // 渲染倍率不该再影响窗口尺寸：窗口铺满整块屏，降分辨率由 Unity 侧做
            render_scale: 0.5,
            extra_args: "--foo".to_string(),
            ..Default::default()
        };
        let args = build_args(&settings, Mode::Desktop, rect(1920, 1080));
        assert!(args.contains(&"-popupwindow".to_string()));
        let width_at = args.iter().position(|a| a == "-screen-width").expect("width");
        assert_eq!(args[width_at + 1], "1920");
        let height_at = args.iter().position(|a| a == "-screen-height").expect("height");
        assert_eq!(args[height_at + 1], "1080");
        // 用户附加参数原样跟在后面
        assert!(args.contains(&"--foo".to_string()));
    }

    #[test]
    fn graphics_api_switches_the_force_flag() {
        // auto：不插手 Unity 的默认选择
        let auto = build_args(&Settings::default(), Mode::Desktop, rect(1920, 1080));
        assert!(!auto.iter().any(|a| a.starts_with("-force-d3d")));

        let d11 = build_args(
            &Settings { graphics_api: "d3d11".into(), ..Default::default() },
            Mode::Desktop,
            rect(1920, 1080),
        );
        assert!(d11.contains(&"-force-d3d11".to_string()));

        let d12 = build_args(
            &Settings { graphics_api: "d3d12".into(), ..Default::default() },
            Mode::Desktop,
            rect(1920, 1080),
        );
        assert!(d12.contains(&"-force-d3d12".to_string()));
    }

    #[test]
    fn user_extra_args_come_last_so_they_can_override() {
        let settings = Settings {
            graphics_api: "d3d12".into(),
            extra_args: "-force-d3d11".into(),
            ..Default::default()
        };
        let args = build_args(&settings, Mode::Desktop, rect(1920, 1080));
        let ours = args.iter().position(|a| a == "-force-d3d12").expect("ours");
        let theirs = args.iter().position(|a| a == "-force-d3d11").expect("theirs");
        assert!(theirs > ours, "用户自己写的参数应当排在后面（后写的生效）");
    }

    #[test]
    fn preview_args_use_the_preview_size_and_stay_windowed() {        let settings = Settings {
            preview_width: 800,
            preview_height: 450,
            ..Default::default()
        };
        let args = build_args(&settings, Mode::Preview, rect(1920, 1080));
        assert!(!args.contains(&"-popupwindow".to_string()));
        let width_at = args.iter().position(|a| a == "-screen-width").expect("width");
        assert_eq!(args[width_at + 1], "800");
        assert_eq!(args[0], "-screen-fullscreen");
        assert_eq!(args[1], "0");
    }

    #[test]
    fn a_degenerate_rect_still_produces_a_usable_window() {
        let settings = Settings::default();
        let args = build_args(&settings, Mode::Desktop, rect(0, 0));
        let width_at = args.iter().position(|a| a == "-screen-width").expect("width");
        assert!(args[width_at + 1].parse::<i32>().unwrap() >= 320);
    }

    #[test]
    fn stopping_resets_state() {
        let mut host = UnityHost::default();
        host.state.pid = 4242;
        host.mode = Mode::Desktop;
        host.stop();
        assert_eq!(host.mode(), Mode::Stopped);
        assert_eq!(host.state().pid, 0);
        assert_eq!(host.state().mode, "stopped");
        assert!(host.hwnd().is_none());
    }

    #[test]
    fn a_condition_flicker_does_not_pause_the_wallpaper() {
        // 这就是"切到别的程序壁纸停一下"的复现：外壳报忙 1 秒后又不忙了
        let mut debounce = PauseDebounce::new(PAUSE_HOLD);
        let start = Instant::now();
        assert!(!debounce.update(true, start), "刚满足条件不该立刻暂停");
        assert!(!debounce.update(true, start + Duration::from_millis(900)));
        assert!(!debounce.update(false, start + Duration::from_millis(1000)), "条件没了就该解除");
        // 抖动过去后即使再满足，也要从头计时
        assert!(!debounce.update(true, start + Duration::from_millis(1100)));
        assert!(!debounce.update(true, start + Duration::from_millis(2000)));
    }

    #[test]
    fn a_sustained_condition_pauses_and_a_cleared_one_resumes() {
        let mut debounce = PauseDebounce::new(PAUSE_HOLD);
        let start = Instant::now();
        assert!(!debounce.update(true, start));
        assert!(!debounce.update(true, start + Duration::from_secs(2)));
        assert!(debounce.update(true, start + Duration::from_secs(3)), "持续 3 秒后应当暂停");
        assert!(debounce.update(true, start + Duration::from_secs(10)), "条件还在就保持暂停");
        assert!(!debounce.update(false, start + Duration::from_secs(11)), "条件消失立刻恢复");
    }

    #[test]
    fn stopping_clears_the_pause_reason() {
        let mut host = UnityHost::default();
        host.pause_reason = "检测到全屏应用".to_string();
        host.stop();
        assert!(host.pause_reason().is_empty());
    }

    #[test]
    fn scaling_keeps_the_origin() {
        let scaled = scaled_rect(
            windows::Win32::Foundation::RECT {
                left: 100,
                top: 50,
                right: 2020,
                bottom: 1130,
            },
            0.5,
        );
        assert_eq!((scaled.left, scaled.top), (100, 50));
        assert_eq!(scaled.right - scaled.left, 960);
        assert_eq!(scaled.bottom - scaled.top, 540);
    }
}
