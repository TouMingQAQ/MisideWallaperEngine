//! `ProcessAudioCapture.dll`（采集内核）的动态绑定层。
//!
//! 内核当前导出 15 个 C ABI 符号，分三组：
//!
//! ```c
//! /* 采集 */
//! int32_t  pac_start_capture(uint32_t pid, pac_audio_callback cb, void *user, pac_capture_handle **out);
//! int32_t  pac_stop_capture(pac_capture_handle *handle);
//! int32_t  pac_capture_format(const pac_capture_handle *handle, pac_format_t *out);
//! int32_t  pac_is_capturing(const pac_capture_handle *handle);
//! int32_t  pac_capture_error(const pac_capture_handle *handle);
//! /* 目标枚举 */
//! int32_t  pac_enum_targets(pac_target_list_t **out);
//! size_t   pac_target_count(const pac_target_list_t *list);
//! const pac_target_t *pac_target_at(const pac_target_list_t *list, size_t index);
//! const char *pac_target_list_sessions_error(const pac_target_list_t *list);
//! void     pac_free_target_list(pac_target_list_t *list);
//! /* DSP */
//! int32_t  pac_analyzer_create(uint32_t rate, uint16_t channels, pac_analyzer_t **out);
//! void     pac_analyzer_destroy(pac_analyzer_t *analyzer);
//! int32_t  pac_analyzer_process(pac_analyzer_t *, const float *samples, uint32_t frames,
//!                               float *wave, size_t wave_cap, float *spec, size_t spec_cap,
//!                               pac_frame_t *out);
//! /* 杂项 */
//! uint32_t pac_version(void);
//! const char *pac_strerror(int32_t code);
//! ```
//!
//! 「窗口枚举 / 音频会话 / 媒体信息 / DSP」原本在宿主侧实现，现已下沉到内核，
//! 所以这一层要按 v3 ABI 解析；万一被人换成旧的 v2 DLL，[`PacLibrary::extras`]
//! 会给出可读的提示而不是崩溃。
//!
//! 注意点（来自内核头文件）：
//! * `pac_start_capture` 会**阻塞**直到流激活成功或 10 秒超时；
//! * 回调运行在采集线程上，`samples` 仅在本次调用期间有效，必须立刻拷贝；
//! * 回调内**不得**再调用 start/stop；
//! * 一个 handle 只能 `stop` 一次，stop 之后句柄被消费；
//! * `pac_enum_targets` 返回的列表必须用 `pac_free_target_list` 释放，
//!   里面的字符串归列表所有。

use std::ffi::{c_char, c_void, CStr, CString};
use std::path::{Path, PathBuf};

use libloading::Library;

/// 不透明句柄，与 C 头文件中的 `pac_capture_handle` 对应。
#[repr(C)]
pub struct PacCaptureHandle {
    _private: [u8; 0],
}

/// 不透明目标列表，与 `pac_target_list_t` 对应。
#[repr(C)]
pub struct PacTargetList {
    _private: [u8; 0],
}

/// 不透明分析器，与 `pac_analyzer_t` 对应。
#[repr(C)]
pub struct PacAnalyzer {
    _private: [u8; 0],
}

/// 一个可采集目标，与 `pac_target_t` 逐字段对应。
///
/// 字符串指针归列表所有，列表释放后即失效；本模块会立刻把它们复制成 Rust 字符串。
#[repr(C)]
struct PacTarget {
    pid: u32,
    hwnd: u64,
    title: *const c_char,
    process_name: *const c_char,
    process_path: *const c_char,
    has_session: i32,
    session_state: i32,
    session_peak: f32,
    has_window: i32,
    media_title: *const c_char,
    media_artist: *const c_char,
    media_album: *const c_char,
    media_status: i32,
}

/// 采集流格式，与 `pac_format_t` 对应。
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct PacFormat {
    pub sample_rate: u32,
    pub channels: u16,
    pub bits_per_sample: u16,
    pub is_float: i32,
}

/// 一帧分析结果的形状，与 `pac_frame_t` 对应。
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
struct PacFrame {
    rms: f32,
    peak: f32,
    waveform_len: usize,
    spectrum_len: usize,
}

/// 采集回调类型。`samples` 为 `frames * channels` 个交错 32-bit float 采样。
pub type PacAudioCallback = unsafe extern "C" fn(
    samples: *const f32,
    frames: u32,
    channels: u16,
    sample_rate: u32,
    user_data: *mut c_void,
);

pub const PAC_OK: i32 = 0;

/// 每帧包络的 `(min, max)` 组数，对应 `PAC_WAVE_BUCKETS`。
pub const PAC_WAVE_BUCKETS: usize = 256;
/// 每帧频谱柱数，对应 `PAC_SPECTRUM_BINS`。
pub const PAC_SPECTRUM_BINS: usize = 128;

/// 目标没有任何音频会话。
pub const PAC_SESSION_NONE: i32 = 0;
/// 目标此刻正在渲染音频。
pub const PAC_SESSION_ACTIVE: i32 = 1;
/// 目标有会话但没在渲染。
pub const PAC_SESSION_INACTIVE: i32 = 2;
/// 目标的会话正在销毁。
pub const PAC_SESSION_EXPIRED: i32 = 3;

/// 媒体播放状态未知 / 没有媒体会话。
pub const PAC_MEDIA_UNKNOWN: i32 = 0;
pub const PAC_MEDIA_PLAYING: i32 = 1;
pub const PAC_MEDIA_PAUSED: i32 = 2;
pub const PAC_MEDIA_STOPPED: i32 = 3;
pub const PAC_MEDIA_CLOSED: i32 = 4;
pub const PAC_MEDIA_CHANGING: i32 = 5;
pub const PAC_MEDIA_OPENED: i32 = 6;

type StartFn = unsafe extern "C" fn(
    pid: u32,
    callback: PacAudioCallback,
    user_data: *mut c_void,
    out_handle: *mut *mut PacCaptureHandle,
) -> i32;
type StopFn = unsafe extern "C" fn(handle: *mut PacCaptureHandle) -> i32;
type VersionFn = unsafe extern "C" fn() -> u32;
type StrErrorFn = unsafe extern "C" fn(code: i32) -> *const c_char;

type CaptureFormatFn =
    unsafe extern "C" fn(handle: *const PacCaptureHandle, out_format: *mut PacFormat) -> i32;
type IsCapturingFn = unsafe extern "C" fn(handle: *const PacCaptureHandle) -> i32;
type CaptureErrorFn = unsafe extern "C" fn(handle: *const PacCaptureHandle) -> i32;

type EnumTargetsFn = unsafe extern "C" fn(out_list: *mut *mut PacTargetList) -> i32;
type TargetCountFn = unsafe extern "C" fn(list: *const PacTargetList) -> usize;
type TargetAtFn =
    unsafe extern "C" fn(list: *const PacTargetList, index: usize) -> *const PacTarget;
type TargetListErrorFn = unsafe extern "C" fn(list: *const PacTargetList) -> *const c_char;
type FreeTargetListFn = unsafe extern "C" fn(list: *mut PacTargetList);

type AnalyzerCreateFn =
    unsafe extern "C" fn(sample_rate: u32, channels: u16, out: *mut *mut PacAnalyzer) -> i32;
type AnalyzerDestroyFn = unsafe extern "C" fn(analyzer: *mut PacAnalyzer);
type AnalyzerProcessFn = unsafe extern "C" fn(
    analyzer: *mut PacAnalyzer,
    samples: *const f32,
    frames: u32,
    waveform: *mut f32,
    waveform_capacity: usize,
    spectrum: *mut f32,
    spectrum_capacity: usize,
    out_frame: *mut PacFrame,
) -> i32;

/// v3 才有的导出符号；缺任何一个都说明手上这份 DLL 太旧。
struct Extras {
    capture_format: CaptureFormatFn,
    is_capturing: IsCapturingFn,
    capture_error: CaptureErrorFn,
    enum_targets: EnumTargetsFn,
    target_count: TargetCountFn,
    target_at: TargetAtFn,
    target_list_error: TargetListErrorFn,
    free_target_list: FreeTargetListFn,
    analyzer_create: AnalyzerCreateFn,
    analyzer_destroy: AnalyzerDestroyFn,
    analyzer_process: AnalyzerProcessFn,
}

/// 把 C 列表展开成 Rust 拥有的快照条目。
#[derive(Debug, Clone, Default)]
pub struct TargetEntry {
    pub pid: u32,
    pub hwnd: u64,
    pub title: String,
    pub process_name: String,
    pub process_path: String,
    pub has_session: bool,
    pub session_state: i32,
    pub session_peak: f32,
    pub has_window: bool,
    pub media_title: String,
    pub media_artist: String,
    pub media_album: String,
    pub media_status: i32,
}

/// 一次目标枚举的结果。
#[derive(Debug, Clone, Default)]
pub struct TargetSnapshot {
    pub entries: Vec<TargetEntry>,
    /// 音频会话列表读取失败的原因；成功时为空字符串。
    pub sessions_error: String,
}

/// 已加载的采集内核及其导出符号。
pub struct PacLibrary {
    /// `Library` 必须在整个使用期间存活，否则符号指针悬空。
    _lib: Library,
    start: StartFn,
    stop: StopFn,
    version: Option<VersionFn>,
    strerror: Option<StrErrorFn>,
    extras: Option<Extras>,
    /// DLL 所在路径，便于 UI 展示。
    pub path: PathBuf,
}

impl PacLibrary {
    /// 从指定路径加载 DLL 并解析导出符号。
    pub fn load(path: &Path) -> Result<Self, String> {
        // SAFETY: 我们只从该动态库解析约定的 C ABI 符号，并在库存活期间使用这些指针。
        unsafe {
            let lib = Library::new(path)
                .map_err(|e| format!("加载动态库失败 `{}`：{e}", path.display()))?;

            let start = *lib
                .get::<StartFn>(b"pac_start_capture\0")
                .map_err(|e| format!("未找到导出符号 `pac_start_capture`：{e}"))?;
            let stop = *lib
                .get::<StopFn>(b"pac_stop_capture\0")
                .map_err(|e| format!("未找到导出符号 `pac_stop_capture`：{e}"))?;
            let version = lib.get::<VersionFn>(b"pac_version\0").ok().map(|s| *s);
            let strerror = lib.get::<StrErrorFn>(b"pac_strerror\0").ok().map(|s| *s);

            // v3 新增符号：整套一起取，缺一个就当 DLL 太旧。
            let extras = (|| {
                Some(Extras {
                    capture_format: *lib.get(b"pac_capture_format\0").ok()?,
                    is_capturing: *lib.get(b"pac_is_capturing\0").ok()?,
                    capture_error: *lib.get(b"pac_capture_error\0").ok()?,
                    enum_targets: *lib.get(b"pac_enum_targets\0").ok()?,
                    target_count: *lib.get(b"pac_target_count\0").ok()?,
                    target_at: *lib.get(b"pac_target_at\0").ok()?,
                    target_list_error: *lib.get(b"pac_target_list_sessions_error\0").ok()?,
                    free_target_list: *lib.get(b"pac_free_target_list\0").ok()?,
                    analyzer_create: *lib.get(b"pac_analyzer_create\0").ok()?,
                    analyzer_destroy: *lib.get(b"pac_analyzer_destroy\0").ok()?,
                    analyzer_process: *lib.get(b"pac_analyzer_process\0").ok()?,
                })
            })();

            Ok(Self {
                _lib: lib,
                start,
                stop,
                version,
                strerror,
                extras,
                path: path.to_path_buf(),
            })
        }
    }

    /// 调用 `pac_start_capture`。**该调用会阻塞最多 10 秒**，请放在阻塞线程池里执行。
    ///
    /// 返回 `(返回码, 句柄)`。
    pub unsafe fn start_capture(
        &self,
        pid: u32,
        callback: PacAudioCallback,
        user_data: *mut c_void,
    ) -> (i32, *mut PacCaptureHandle) {
        let mut handle: *mut PacCaptureHandle = std::ptr::null_mut();
        let code = (self.start)(pid, callback, user_data, &mut handle);
        (code, handle)
    }

    /// 调用 `pac_stop_capture`。句柄被消费，之后不可复用。
    pub unsafe fn stop_capture(&self, handle: *mut PacCaptureHandle) -> i32 {
        (self.stop)(handle)
    }

    /// `pac_version()`；若 DLL 未导出则返回 0。
    pub fn version(&self) -> u32 {
        self.version.map(|f| unsafe { f() }).unwrap_or(0)
    }

    /// 这份 DLL 是否具备 v3 能力（目标枚举 / DSP / 会话元数据）。
    pub fn has_extras(&self) -> bool {
        self.extras.is_some()
    }

    /// 采集流的格式。进程回环固定 48 kHz / 立体声 / 32-bit float，
    /// 所以起采集后立刻就能拿到，不必等首帧回调。
    pub fn capture_format(&self, handle: *const PacCaptureHandle) -> Result<PacFormat, String> {
        let extras = self.extras()?;
        let mut format = PacFormat::default();
        let code = unsafe { (extras.capture_format)(handle, &mut format) };
        if code != PAC_OK {
            return Err(self.describe(code));
        }
        Ok(format)
    }

    /// 采集线程是否仍在运行。
    pub fn is_capturing(&self, handle: *const PacCaptureHandle) -> Result<bool, String> {
        let extras = self.extras()?;
        Ok(unsafe { (extras.is_capturing)(handle) } != 0)
    }

    /// 采集线程自行结束时的错误码（`PAC_OK` 表示仍在运行或正常停止）。
    pub fn capture_error(&self, handle: *const PacCaptureHandle) -> Result<i32, String> {
        let extras = self.extras()?;
        Ok(unsafe { (extras.capture_error)(handle) })
    }

    /// 枚举当前可采集的目标。
    pub fn enumerate_targets(&self) -> Result<TargetSnapshot, String> {
        let extras = self.extras()?;

        let mut list: *mut PacTargetList = std::ptr::null_mut();
        let code = unsafe { (extras.enum_targets)(&mut list) };
        if code != PAC_OK {
            return Err(format!("枚举可采集目标失败：{}", self.describe(code)));
        }
        if list.is_null() {
            return Err("枚举可采集目标失败：内核返回了空列表指针".to_string());
        }

        // SAFETY: 列表在下面的 scope 结束前一直有效；字符串在释放前完成拷贝。
        let snapshot = unsafe {
            let count = (extras.target_count)(list);
            let mut entries = Vec::with_capacity(count);
            for index in 0..count {
                let item = (extras.target_at)(list, index);
                if item.is_null() {
                    continue;
                }
                entries.push(copy_target(&*item));
            }

            let error_ptr = (extras.target_list_error)(list);
            let sessions_error = if error_ptr.is_null() {
                String::new()
            } else {
                CStr::from_ptr(error_ptr).to_string_lossy().into_owned()
            };

            TargetSnapshot { entries, sessions_error }
        };

        unsafe { (extras.free_target_list)(list) };
        Ok(snapshot)
    }

    /// 创建内核提供的 DSP 分析器。
    ///
    /// 返回的裸指针需要用 [`PacLibrary::analyzer_destroy`] 释放；
    /// 宿主侧请使用 `dsp::Analyzer` 这层 RAII 包装。
    pub fn analyzer_create(
        &self,
        sample_rate: u32,
        channels: u16,
    ) -> Result<*mut PacAnalyzer, String> {
        let extras = self.extras()?;
        let mut analyzer: *mut PacAnalyzer = std::ptr::null_mut();
        let code = unsafe { (extras.analyzer_create)(sample_rate, channels, &mut analyzer) };
        if code != PAC_OK || analyzer.is_null() {
            return Err(format!("创建分析器失败：{}", self.describe(code)));
        }
        Ok(analyzer)
    }

    /// 释放 [`PacLibrary::analyzer_create`] 返回的分析器。
    pub fn analyzer_destroy(&self, analyzer: *mut PacAnalyzer) {
        if let Some(extras) = self.extras.as_ref() {
            unsafe { (extras.analyzer_destroy)(analyzer) };
        }
    }

    /// 分析一段交错采样，结果写入调用方提供的缓冲。
    ///
    /// 返回 `(电平, 写入的波形长度, 写入的频谱长度)`。
    pub fn analyzer_process(
        &self,
        analyzer: *mut PacAnalyzer,
        interleaved: &[f32],
        frames: usize,
        waveform: &mut [f32],
        spectrum: &mut [f32],
    ) -> Result<(f32, f32, usize, usize), String> {
        let extras = self.extras()?;

        let mut frame = PacFrame::default();
        let code = unsafe {
            (extras.analyzer_process)(
                analyzer,
                interleaved.as_ptr(),
                frames as u32,
                waveform.as_mut_ptr(),
                waveform.len(),
                spectrum.as_mut_ptr(),
                spectrum.len(),
                &mut frame,
            )
        };
        if code != PAC_OK {
            return Err(format!("分析音频帧失败：{}", self.describe(code)));
        }

        Ok((frame.rms, frame.peak, frame.waveform_len, frame.spectrum_len))
    }

    fn extras(&self) -> Result<&Extras, String> {
        self.extras.as_ref().ok_or_else(|| {
            format!(
                "采集内核版本过旧（v{}）：目标枚举与 DSP 需要 v3 及以上，请替换 ProcessAudioCapture.dll",
                self.version()
            )
        })
    }

    /// `pac_strerror()`；若 DLL 未导出则回退到内置的中文描述。
    pub fn strerror(&self, code: i32) -> String {
        if let Some(f) = self.strerror {
            let ptr = unsafe { f(code) };
            if !ptr.is_null() {
                let text = unsafe { CStr::from_ptr(ptr) }.to_string_lossy().into_owned();
                if !text.is_empty() {
                    return format!("{text} ({code})");
                }
            }
        }
        format!("{} ({code})", builtin_strerror(code))
    }

    /// 把返回码转换为可读文本（不依赖 DLL）。
    pub fn describe(&self, code: i32) -> String {
        if code == PAC_OK {
            return "成功".to_string();
        }
        self.strerror(code)
    }
}

/// 把一个 C 目标条目整份拷贝到 Rust 内存，避免依赖列表的生命周期。
fn copy_target(item: &PacTarget) -> TargetEntry {
    TargetEntry {
        pid: item.pid,
        hwnd: item.hwnd,
        title: cstr_to_string(item.title),
        process_name: cstr_to_string(item.process_name),
        process_path: cstr_to_string(item.process_path),
        has_session: item.has_session != 0,
        session_state: item.session_state,
        session_peak: item.session_peak,
        has_window: item.has_window != 0,
        media_title: cstr_to_string(item.media_title),
        media_artist: cstr_to_string(item.media_artist),
        media_album: cstr_to_string(item.media_album),
        media_status: item.media_status,
    }
}

fn cstr_to_string(pointer: *const c_char) -> String {
    if pointer.is_null() {
        return String::new();
    }
    // SAFETY: 内核保证列表里的每个字符串都是以 NUL 结尾的有效 C 字符串。
    unsafe { CStr::from_ptr(pointer) }.to_string_lossy().into_owned()
}

/// `pac_strerror` 的等价内置实现，用于 DLL 缺失该符号时的兜底。
pub fn builtin_strerror(code: i32) -> &'static str {
    match code {
        0 => "PAC_OK：成功",
        -1 => "PAC_E_INVALID_ARGUMENT：参数无效（pid 为 0 / 回调为空 / 句柄为空）",
        -2 => "PAC_E_UNSUPPORTED_PLATFORM：系统不支持进程回环，需 Windows 10 2004 (build 19041) 及以上",
        -3 => "PAC_E_ACTIVATION_FAILED：激活失败，目标进程可能已退出、静音或受保护",
        -4 => "PAC_E_UNSUPPORTED_FORMAT：回环流不是 32-bit IEEE float PCM",
        -5 => "PAC_E_TIMEOUT：音频客户端未在 10 秒内完成激活",
        -6 => "PAC_E_INTERNAL：无法创建 Win32/COM 资源，或采集异常中断",
        _ => "未知返回码",
    }
}

/// DLL 可能的文件扩展名（官方 release 提供 `-x64` / `-x86` 后缀版本）。
const DLL_NAMES: [&str; 2] = ["ProcessAudioCapture.dll", "ProcessAudioCapture-x64.dll"];

/// 按优先级生成候选路径，覆盖 dev 与打包后的多种运行形态。
pub fn candidate_paths(resource_dir: Option<&Path>) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = Vec::new();

    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            dirs.push(dir.to_path_buf());
            dirs.push(dir.join("binaries"));
            dirs.push(dir.join("resources"));
            dirs.push(dir.join("resources").join("binaries"));
        }
    }

    if let Ok(cwd) = std::env::current_dir() {
        dirs.push(cwd.clone());
        dirs.push(cwd.join("binaries"));
        dirs.push(cwd.join("src-tauri").join("binaries"));
        dirs.push(cwd.join("src-tauri"));
    }

    if let Some(dir) = resource_dir {
        dirs.push(dir.to_path_buf());
        dirs.push(dir.join("binaries"));
    }

    // 开发期兜底：编译时记录的工程目录（`cargo run` / `tauri dev` 下一定存在）。
    dirs.push(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("binaries"));

    let mut out = Vec::new();
    for dir in dirs {
        for name in DLL_NAMES {
            let candidate = dir.join(name);
            if candidate.is_file() && !out.contains(&candidate) {
                out.push(candidate);
            }
        }
    }
    out
}

/// 便捷函数：把 Rust `&str` 转成 C 字符串（当前未使用，保留给后续扩展）。
#[allow(dead_code)]
pub fn to_cstring(value: &str) -> Result<CString, String> {
    CString::new(value).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use std::ffi::c_void;
    use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
    use std::time::{Duration, Instant};

    use super::*;

    /// 回调里累计的探针数据。
    #[derive(Default)]
    struct Probe {
        frames: AtomicU64,
        sample_rate: AtomicU32,
        channels: AtomicU32,
        peak_bits: AtomicU32,
    }

    unsafe extern "C" fn probe_callback(
        samples: *const f32,
        frames: u32,
        channels: u16,
        sample_rate: u32,
        user_data: *mut c_void,
    ) {
        if samples.is_null() || user_data.is_null() || frames == 0 || channels == 0 {
            return;
        }
        let probe = &*(user_data as *const Probe);
        probe.frames.fetch_add(frames as u64, Ordering::Relaxed);
        probe.sample_rate.store(sample_rate, Ordering::Relaxed);
        probe.channels.store(channels as u32, Ordering::Relaxed);

        let slice = std::slice::from_raw_parts(samples, frames as usize * channels as usize);
        let mut peak = f32::from_bits(probe.peak_bits.load(Ordering::Relaxed));
        for &value in slice {
            let abs = if value.is_finite() { value.abs() } else { 0.0 };
            if abs > peak {
                peak = abs;
            }
        }
        probe.peak_bits.store(peak.to_bits(), Ordering::Relaxed);
    }

    fn load() -> PacLibrary {
        let candidates = candidate_paths(None);
        println!("候选路径：{candidates:#?}");
        assert!(
            !candidates.is_empty(),
            "没找到 ProcessAudioCapture.dll，请确认 src-tauri/binaries/ 下有该文件"
        );
        PacLibrary::load(&candidates[0]).expect("加载 DLL 失败")
    }

    /// 优先挑一个确实在出声的进程，没有就退回本进程。
    ///
    /// 可以用环境变量 `PAC_TEST_PID=<pid>` 指定目标，方便手动排查。
    fn pick_target_pid(lib: &PacLibrary) -> (u32, String) {
        if let Ok(raw) = std::env::var("PAC_TEST_PID") {
            if let Ok(pid) = raw.trim().parse::<u32>() {
                return (pid, format!("env PAC_TEST_PID={pid}"));
            }
        }
        if let Ok(snapshot) = lib.enumerate_targets() {
            if let Some(entry) = snapshot
                .entries
                .iter()
                .find(|entry| entry.session_state == 1 && entry.session_peak >= 0.015)
            {
                return (entry.pid, entry.process_name.clone());
            }
        }
        (
            std::process::id(),
            format!("pid:{} (无活跃音频会话，使用本进程)", std::process::id()),
        )
    }

    #[test]
    fn finds_and_loads_bundled_dll() {
        let lib = load();
        println!("已加载：{}", lib.path.display());
        println!("pac_version() = {}", lib.version());
        println!("具备 v3 能力：{}", lib.has_extras());
        assert!(lib.version() >= 3, "随程序分发的是 v3 内核，版本号不对");
        assert!(lib.has_extras(), "缺少 v3 导出符号");

        for code in [0, -1, -2, -3, -4, -5, -6, 12345] {
            println!("describe({code}) = {}", lib.describe(code));
        }
    }

    /// 目标枚举：内核应当把窗口、音频会话与媒体信息合并好交回来。
    #[test]
    fn enumerates_targets_with_media() {
        let lib = load();
        let snapshot = lib.enumerate_targets().expect("枚举失败");
        println!("条目数：{}", snapshot.entries.len());
        if !snapshot.sessions_error.is_empty() {
            println!("音频会话读取失败：{}", snapshot.sessions_error);
        }

        for entry in snapshot.entries.iter().take(12) {
            println!(
                "pid={:<7} window={:<5} session={} peak={:.3} {:<26} {}",
                entry.pid,
                entry.has_window,
                entry.session_state,
                entry.session_peak,
                entry.process_name,
                entry.title
            );
            if !entry.media_title.is_empty() {
                println!("      media: {} / {}", entry.media_title, entry.media_artist);
            }
        }

        assert!(!snapshot.entries.is_empty(), "一个目标都没枚举到");

        // 这里只断言**恒成立的不变量**。早先这条测试断言"必须至少有一条音频会话"，
        // 于是用户没在放音乐时它就会红 —— 那是环境，不是缺陷（会话枚举为空的合法情况
        // 只有两种：真的没人出声，或者内核报了 sessions_error）。
        for entry in &snapshot.entries {
            if entry.has_session {
                assert_ne!(
                    entry.session_state, 0,
                    "带会话的条目不该是 none：{}",
                    entry.process_name
                );
                assert!(entry.session_peak >= 0.0);
            } else {
                assert_eq!(entry.session_peak, 0.0, "没有会话就不该有峰值");
            }
            if !entry.has_window {
                assert_eq!(entry.hwnd, 0, "没有窗口的条目 hwnd 必须是 0");
            }
        }

        if snapshot.entries.iter().all(|entry| !entry.has_session) {
            println!(
                "当前没有任何进程在出声（会话枚举为空），只校验了结构不变量{}",
                if snapshot.sessions_error.is_empty() {
                    String::new()
                } else {
                    format!("；内核同时报告：{}", snapshot.sessions_error)
                }
            );
        }
    }

    /// 内核自带的 DSP：同样的输入应当得到和宿主旧实现一致的量级。
    #[test]
    fn kernel_analyzer_produces_a_frame() {
        let lib = load();
        let analyzer = lib.analyzer_create(48_000, 2).expect("创建分析器失败");

        let frames = 960;
        let mut interleaved = Vec::with_capacity(frames * 2);
        for index in 0..frames {
            let value = (std::f32::consts::TAU * 440.0 * index as f32 / 48_000.0).sin() * 0.5;
            interleaved.push(value);
            interleaved.push(value);
        }

        let mut waveform = vec![0.0f32; PAC_WAVE_BUCKETS * 2];
        let mut spectrum = vec![0.0f32; PAC_SPECTRUM_BINS];
        let (rms, peak, waveform_len, spectrum_len) = lib
            .analyzer_process(analyzer, &interleaved, frames, &mut waveform, &mut spectrum)
            .expect("分析失败");

        println!("rms={rms:.4} peak={peak:.4} waveform_len={waveform_len} spectrum_len={spectrum_len}");
        lib.analyzer_destroy(analyzer);

        assert!((rms - 0.3535).abs() < 0.02, "rms 偏离预期：{rms}");
        assert!((peak - 0.5).abs() < 0.01, "peak 偏离预期：{peak}");
        assert_eq!(waveform_len, PAC_WAVE_BUCKETS * 2);
        assert_eq!(spectrum_len, PAC_SPECTRUM_BINS);
    }

    /// 端到端冒烟测试：真的调一次 pac_start_capture / pac_stop_capture，
    /// 并顺带验证格式回报与「采集线程还活着吗」这两个新接口。
    #[test]
    fn captures_audio_through_the_dll() {
        let lib = load();
        let (pid, name) = pick_target_pid(&lib);
        println!("目标进程：{name} (pid={pid})");

        let probe = Probe::default();
        let probe_ptr = &probe as *const Probe as *mut c_void;

        // SAFETY: probe 在整个采集期间都活在栈上；回调只做原子累加，不会 panic。
        let (code, handle) = unsafe { lib.start_capture(pid, probe_callback, probe_ptr) };
        println!("pac_start_capture -> {code}（{}）", lib.describe(code));

        if code != PAC_OK || handle.is_null() {
            // -3 = 目标进程没有在渲染音频（例如本进程压根不播声音），属于可接受结果
            assert_eq!(code, -3, "非预期的返回码：{}", lib.describe(code));
            println!("跳过：目标进程当前没有渲染音频");
            return;
        }

        let format = lib.capture_format(handle).expect("格式回报失败");
        println!(
            "pac_capture_format -> {} Hz / {} 声道 / {} bit / float={}",
            format.sample_rate, format.channels, format.bits_per_sample, format.is_float
        );
        assert_eq!(format.sample_rate, 48_000);
        assert_eq!(format.channels, 2);
        assert!(lib.is_capturing(handle).unwrap_or(false), "采集线程应当还在跑");
        assert_eq!(lib.capture_error(handle).unwrap_or(-99), PAC_OK);

        let deadline = Instant::now() + Duration::from_secs(3);
        while probe.frames.load(Ordering::Relaxed) == 0 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        // 再多采 1.2 秒，让峰值和格式有代表性（目标进程需真的在出声）
        std::thread::sleep(Duration::from_millis(1200));

        let stop_code = unsafe { lib.stop_capture(handle) };
        let frames = probe.frames.load(Ordering::Relaxed);
        let peak = f32::from_bits(probe.peak_bits.load(Ordering::Relaxed));

        println!(
            "收到 {frames} 帧 · {} Hz · {} 声道 · 峰值 {peak:.4}",
            probe.sample_rate.load(Ordering::Relaxed),
            probe.channels.load(Ordering::Relaxed),
        );
        println!("pac_stop_capture -> {stop_code}");

        assert_eq!(stop_code, PAC_OK, "stop 返回异常");
        assert!(frames > 0, "激活成功但一个采样都没收到");
        assert!(peak <= 1.0, "峰值超出 [-1, 1]：{peak}");
    }
}
