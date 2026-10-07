//! 采集会话：把 DLL 的回调桥接到「环形缓冲 → 发射线程 → 前端事件」。
//!
//! 关键设计：
//! * DLL 回调跑在采集线程上，必须极快返回 —— 这里只做一次 `try_lock` + 拷贝，
//!   拿不到锁宁可丢这一块数据，也绝不阻塞音频线程；
//! * 发射线程按固定节奏（`EMIT_INTERVAL_MS`）把累计的采样做 DSP 后通过 Tauri 事件推给前端。

use std::collections::VecDeque;
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use serde::Serialize;
use tauri::{AppHandle, Emitter};

use crate::dsp::{Analyzer, AudioFrame};
use crate::pac::{PacAudioCallback, PacCaptureHandle, PacFormat, PacLibrary};

/// 前端监听的事件名。
pub const AUDIO_EVENT: &str = "pac://audio-frame";
/// 会话结束事件（DLL 侧异常中断 / 手动停止）。
pub const STOPPED_EVENT: &str = "pac://stopped";

/// 事件推送的默认节奏（毫秒）。20ms ≈ 50fps，也是"帧率不限"时的节奏。
const EMIT_INTERVAL_MS: u64 = 20;
/// 推帧间隔的下限 / 上限（毫秒）：最快 250fps，最慢 10fps。
const MIN_EMIT_INTERVAL_MS: u32 = 4;
const MAX_EMIT_INTERVAL_MS: u32 = 100;
/// 环形缓冲上限：约 1 秒 @48kHz 立体声，超了丢最旧的数据。
const MAX_BUFFERED_SAMPLES: usize = 48_000 * 2;

/// 帧率上限（0 = 不限）→ 推帧间隔（毫秒）。
///
/// 界面上的「帧率上限」是同一个数管两头：前端按它限绘制，这里按它限推帧 ——
/// 光限绘制的话，事件里那 700 多个数字照样每秒被解析几十次。
pub fn frame_interval_ms(fps: u32) -> u32 {
    if fps == 0 {
        EMIT_INTERVAL_MS as u32
    } else {
        (1000 / fps.max(1)).clamp(MIN_EMIT_INTERVAL_MS, MAX_EMIT_INTERVAL_MS)
    }
}

/// 跨线程共享的会话计数器。
pub struct SharedCounters {
    pub sample_rate: AtomicU32,
    pub channels: AtomicU32,
    pub total_frames: AtomicU64,
    /// 因为缓冲溢出被丢弃的采样数，用于 UI 提示。
    pub dropped_samples: AtomicU64,
    /// 推帧间隔（毫秒）。会话跑着的时候改帧率也能立刻生效，所以每轮都读一次。
    pub emit_interval_ms: AtomicU32,
}

impl Default for SharedCounters {
    fn default() -> Self {
        Self {
            sample_rate: AtomicU32::new(0),
            channels: AtomicU32::new(0),
            total_frames: AtomicU64::new(0),
            dropped_samples: AtomicU64::new(0),
            // 默认就是原来的 50fps；真正的值在起流时按设置写进来
            emit_interval_ms: AtomicU32::new(EMIT_INTERVAL_MS as u32),
        }
    }
}

/// 传给 DLL 回调的上下文。通过 `Box::into_raw` 固定地址后交给 C 侧。
pub struct CallbackCtx {
    queue: Arc<Mutex<VecDeque<f32>>>,
    counters: Arc<SharedCounters>,
}

/// 活跃的采集会话。
pub struct ActiveCapture {
    pub pid: u32,
    pub process_name: String,
    pub started_at: Instant,
    pub counters: Arc<SharedCounters>,

    lib: Arc<PacLibrary>,
    handle: *mut PacCaptureHandle,
    ctx: *mut CallbackCtx,
    stop_flag: Arc<AtomicBool>,
    emitter: Option<JoinHandle<()>>,
}

// 句柄与上下文都是裸指针，但生命周期由本结构体严格管理（stop 时统一释放）。
unsafe impl Send for ActiveCapture {}

impl ActiveCapture {
    pub fn elapsed(&self) -> Duration {
        self.started_at.elapsed()
    }

    /// 内核是否仍认为采集线程在跑。为 `false` 说明流自己断了（不是我们停的）。
    pub fn is_alive(&self) -> bool {
        self.lib.is_capturing(self.handle).unwrap_or(true)
    }

    /// 采集线程自行结束时的错误码（`0` 表示仍在运行或正常停止）。
    pub fn terminal_error(&self) -> i32 {
        self.lib.capture_error(self.handle).unwrap_or(0)
    }

    /// 停止采集：先让 DLL 停下（此后不会再触发回调），再让发射线程排空退出，最后回收内存。
    ///
    /// `notify` 为 `false` 时不广播 `pac://stopped` —— 自动跟随切换时会先停旧会话，
    /// 那种情况不能让它看起来像"用户点了停止"。
    pub fn stop(mut self, app: &AppHandle, notify: bool) -> StopReport {
        let mut warnings = Vec::new();

        // 1. 停止 DLL 侧采集。该调用会 join 采集线程，返回后不会再有任何回调。
        let stop_code = unsafe { self.lib.stop_capture(self.handle) };
        if stop_code != 0 {
            warnings.push(self.lib.describe(stop_code));
        }

        // 2. 通知发射线程做最后一次排空并退出。
        self.stop_flag.store(true, Ordering::SeqCst);
        if let Some(handle) = self.emitter.take() {
            if handle.join().is_err() {
                warnings.push("发射线程异常退出".to_string());
            }
        }

        // 3. 释放回调上下文的内存（此时 C 侧已不再持有它）。
        if !self.ctx.is_null() {
            unsafe { drop(Box::from_raw(self.ctx)) };
            self.ctx = std::ptr::null_mut();
        }

        let report = StopReport {
            pid: self.pid,
            process_name: self.process_name.clone(),
            total_frames: self.counters.total_frames.load(Ordering::Relaxed),
            sample_rate: self.counters.sample_rate.load(Ordering::Relaxed),
            channels: self.counters.channels.load(Ordering::Relaxed),
            duration_ms: self.elapsed().as_millis() as u64,
            dropped_samples: self.counters.dropped_samples.load(Ordering::Relaxed),
            warnings,
        };

        if notify {
            let _ = app.emit(STOPPED_EVENT, &report);
        }
        report
    }
}

/// 停止后的汇总信息。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StopReport {
    pub pid: u32,
    pub process_name: String,
    pub total_frames: u64,
    pub sample_rate: u32,
    pub channels: u32,
    pub duration_ms: u64,
    pub dropped_samples: u64,
    pub warnings: Vec<String>,
}

/// 启动结果。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StartReport {
    pub pid: u32,
    pub process_name: String,
    pub sample_rate: u32,
    pub channels: u32,
    pub dll_version: u32,
    pub warnings: Vec<String>,
}

/// 以阻塞方式启动一个采集会话。**必须在阻塞线程池里调用**（`pac_start_capture` 最多阻塞 10 秒）。
///
/// `emit_interval_ms` 是推帧间隔，由界面的「帧率上限」换算而来（见 [`frame_interval_ms`]）。
pub fn start_capture(
    app: AppHandle,
    lib: Arc<PacLibrary>,
    pid: u32,
    process_name: String,
    emit_interval_ms: u32,
) -> Result<ActiveCapture, String> {
    let counters = Arc::new(SharedCounters::default());
    counters
        .emit_interval_ms
        .store(emit_interval_ms, Ordering::Relaxed);
    let queue: Arc<Mutex<VecDeque<f32>>> = Arc::new(Mutex::new(VecDeque::with_capacity(MAX_BUFFERED_SAMPLES)));
    let stop_flag = Arc::new(AtomicBool::new(false));

    let ctx = Box::new(CallbackCtx {
        queue: Arc::clone(&queue),
        counters: Arc::clone(&counters),
    });
    let ctx_ptr = Box::into_raw(ctx);

    // SAFETY: 回调指针在会话存续期间不会移动，且我们保证在 stop 之后再释放。
    let (code, handle) = unsafe {
        lib.start_capture(pid, audio_callback as PacAudioCallback, ctx_ptr as *mut c_void)
    };

    if code != 0 || handle.is_null() {
        // 启动失败：回收上下文内存即可，发射线程还没起来
        unsafe { drop(Box::from_raw(ctx_ptr)) };

        let detail = lib.describe(code);
        return Err(format!("启动 PID {pid} 的音频捕获失败：{detail}"));
    }

    // 采集格式不用再等首帧回调 —— 内核把流格式固定成 48 kHz / 立体声 / float32
    // 并如实报出来，所以界面上可以立刻显示真实采样率。
    let format = match lib.capture_format(handle) {
        Ok(format) => format,
        Err(err) => {
            unsafe { lib.stop_capture(handle) };
            unsafe { drop(Box::from_raw(ctx_ptr)) };
            return Err(err);
        }
    };
    counters
        .sample_rate
        .store(format.sample_rate, Ordering::Relaxed);
    counters
        .channels
        .store(format.channels as u32, Ordering::Relaxed);

    // 发射线程：排空缓冲 → 内核 DSP → 事件推送
    let started_at = Instant::now();
    let emitter = spawn_emitter(
        app.clone(),
        Arc::clone(&lib),
        Arc::clone(&queue),
        Arc::clone(&counters),
        Arc::clone(&stop_flag),
        pid,
        started_at,
        format,
    );

    Ok(ActiveCapture {
        pid,
        process_name,
        started_at,
        counters,
        lib,
        handle,
        ctx: ctx_ptr,
        stop_flag,
        emitter: Some(emitter),
    })
}

/// DLL 回调。运行在采集线程上，必须尽快返回且绝不 panic。
unsafe extern "C" fn audio_callback(
    samples: *const f32,
    frames: u32,
    channels: u16,
    sample_rate: u32,
    user_data: *mut c_void,
) {
    if samples.is_null() || user_data.is_null() || frames == 0 || channels == 0 {
        return;
    }

    let ctx = &*(user_data as *const CallbackCtx);
    ctx.counters.sample_rate.store(sample_rate, Ordering::Relaxed);
    ctx.counters
        .channels
        .store(channels as u32, Ordering::Relaxed);
    ctx.counters
        .total_frames
        .fetch_add(frames as u64, Ordering::Relaxed);

    let len = frames as usize * channels as usize;
    let slice = std::slice::from_raw_parts(samples, len);

    // 音频线程上只做 try_lock：拿不到锁就丢掉这一块，绝不阻塞回调。
    if let Ok(mut guard) = ctx.queue.try_lock() {
        guard.extend(slice.iter().copied());
        let overflow = guard.len().saturating_sub(MAX_BUFFERED_SAMPLES);
        for _ in 0..overflow {
            guard.pop_front();
        }
        if overflow > 0 {
            ctx.counters
                .dropped_samples
                .fetch_add(overflow as u64, Ordering::Relaxed);
        }
    } else {
        ctx.counters
            .dropped_samples
            .fetch_add(len as u64, Ordering::Relaxed);
    }
}

#[allow(clippy::too_many_arguments)]
fn spawn_emitter(
    app: AppHandle,
    lib: Arc<PacLibrary>,
    queue: Arc<Mutex<VecDeque<f32>>>,
    counters: Arc<SharedCounters>,
    stop_flag: Arc<AtomicBool>,
    pid: u32,
    started_at: Instant,
    format: PacFormat,
) -> JoinHandle<()> {
    thread::Builder::new()
        .name(format!("pac-emitter-{pid}"))
        .spawn(move || {
            // 分析器（FFT 计划 + 临时缓冲）由内核持有，整场采集复用同一个实例
            let mut analyzer =
                match Analyzer::new(Arc::clone(&lib), format.sample_rate, format.channels) {
                    Ok(analyzer) => analyzer,
                    Err(err) => {
                        let _ = app.emit("pac://error", format!("初始化音频分析器失败：{err}"));
                        return;
                    }
                };

            let sample_rate = format.sample_rate;
            let channels = format.channels.max(1) as usize;
            let mut scratch: Vec<f32> = Vec::with_capacity(65_536);
            // 累积待分析的交错采样（降混与分析都在内核里做）
            let mut pending: Vec<f32> = Vec::with_capacity(65_536);
            let mut last_emit = Instant::now();

            loop {
                // 间隔来自可变的帧率设置：每轮重新读，会话跑着改帧率也能立刻生效
                let interval = emit_interval(&counters);

                // 1. 把缓冲里的数据搬到本地
                let drained: usize = {
                    let mut guard = lock(&queue);
                    let take = guard.len().min(131_072);
                    scratch.clear();
                    scratch.extend(guard.drain(..take));
                    take
                };

                if drained > 0 {
                    pending.extend_from_slice(&scratch);

                    // 只保留最近 2 秒用于可视化，防止慢消费者拖垮内存
                    let cap = sample_rate as usize * channels * 2;
                    if pending.len() > cap {
                        let excess = pending.len() - cap;
                        pending.drain(..excess);
                    }
                }

                // 2. 到点了就推一帧
                if last_emit.elapsed() >= Duration::from_millis(interval as u64) {
                    let frames = pending.len() / channels;
                    match analyzer.analyze(&pending, frames) {
                        Ok(frame) => {
                            // 先把这一帧喂给音频链路（UDP 推给 Unity），再发给界面：
                            // 两条路消费的是同一份 DSP 结果，不重复算。
                            crate::audio_link::publish(&app, pid, sample_rate, &frame);
                            if app
                                .emit(
                                    AUDIO_EVENT,
                                    FrameEnvelope {
                                        pid,
                                        frame,
                                        total_frames: counters
                                            .total_frames
                                            .load(Ordering::Relaxed),
                                        elapsed_ms: started_at.elapsed().as_millis() as u64,
                                    },
                                )
                                .is_err()
                            {
                                break;
                            }
                        }
                        Err(err) => {
                            let _ = app.emit("pac://error", format!("音频分析失败：{err}"));
                            break;
                        }
                    }
                    pending.clear();
                    last_emit = Instant::now();
                }

                // 3. 停止条件：内核已停 + 缓冲排空
                if stop_flag.load(Ordering::SeqCst) && drained == 0 {
                    let frames = pending.len() / channels;
                    if let Ok(frame) = analyzer.analyze(&pending, frames) {
                        crate::audio_link::publish(&app, pid, sample_rate, &frame);
                        let _ = app.emit(
                            AUDIO_EVENT,
                            FrameEnvelope {
                                pid,
                                frame,
                                total_frames: counters.total_frames.load(Ordering::Relaxed),
                                elapsed_ms: started_at.elapsed().as_millis() as u64,
                            },
                        );
                    }
                    break;
                }

                if drained == 0 {
                    thread::sleep(Duration::from_millis(2));
                }
            }
        })
        .expect("无法创建音频发射线程")
}

/// 事件负载：会话信息 + 单帧 DSP 结果。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct FrameEnvelope {
    pid: u32,
    total_frames: u64,
    elapsed_ms: u64,
    #[serde(flatten)]
    frame: AudioFrame,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// 当前会话该用的推帧间隔（毫秒）。写坏了（0 或极端值）也有兜底，不会把发射线程跑飞。
fn emit_interval(counters: &SharedCounters) -> u32 {
    let stored = counters.emit_interval_ms.load(Ordering::Relaxed);
    if stored == 0 {
        EMIT_INTERVAL_MS as u32
    } else {
        stored.clamp(MIN_EMIT_INTERVAL_MS, MAX_EMIT_INTERVAL_MS)
    }
}
