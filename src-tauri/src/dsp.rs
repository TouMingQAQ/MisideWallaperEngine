//! 可视化 DSP —— 计算全部在采集内核里完成。
//!
//! 内核的 `pac_analyzer_process` 一次做完「降混单声道 → 峰谷包络 → Hann 窗 FFT
//! → 对数频谱 → 电平表」，宿主不再自带 FFT，也就不再需要 rustfft。
//! 这里只负责创建/释放分析器、复用输出缓冲，并整理成前端要的形状。

use std::sync::Arc;

use serde::Serialize;

use crate::pac::{PacAnalyzer, PacLibrary, PAC_SPECTRUM_BINS, PAC_WAVE_BUCKETS};

/// 每帧输出 256 组 (min,max) → 512 个 f32。
pub const WAVE_BUCKETS: usize = PAC_WAVE_BUCKETS;
/// 频谱柱数量。
pub const SPECTRUM_BINS: usize = PAC_SPECTRUM_BINS;

/// 送往前端的一帧可视化数据。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AudioFrame {
    /// `[min0, max0, min1, max1, ...]`，长度 2 * WAVE_BUCKETS。
    pub waveform: Vec<f32>,
    /// 归一化到 0..1 的对数频谱幅度，长度 SPECTRUM_BINS。
    pub spectrum: Vec<f32>,
    /// 均方根电平（线性 0..1）。
    pub rms: f32,
    /// 峰值电平（线性 0..1）。
    pub peak: f32,
    /// 单帧对应的时间跨度（毫秒）。
    pub window_ms: f32,
}

/// 内核 DSP 分析器的 RAII 包装。
///
/// 一次采集会话创建一个并一直复用：内核在分析器里持有 FFT 计划与临时缓冲，
/// 每帧重建会白白重算一遍 FFT 计划。
pub struct Analyzer {
    lib: Arc<PacLibrary>,
    handle: *mut PacAnalyzer,
    waveform: Vec<f32>,
    spectrum: Vec<f32>,
    sample_rate: u32,
    channels: u16,
}

// 句柄由本结构体独占，且只在持有它的线程上使用。
unsafe impl Send for Analyzer {}

impl Analyzer {
    /// 按采集流的格式创建分析器。
    pub fn new(lib: Arc<PacLibrary>, sample_rate: u32, channels: u16) -> Result<Self, String> {
        let handle = lib.analyzer_create(sample_rate, channels)?;
        Ok(Self {
            lib,
            handle,
            waveform: vec![0.0; WAVE_BUCKETS * 2],
            spectrum: vec![0.0; SPECTRUM_BINS],
            sample_rate,
            channels: channels.max(1),
        })
    }

    /// 分析一段**交错**采样，返回可以直接发给前端的一帧。
    pub fn analyze(&mut self, interleaved: &[f32], frames: usize) -> Result<AudioFrame, String> {
        let usable = frames.min(interleaved.len() / self.channels as usize);
        let (rms, peak, waveform_len, spectrum_len) = self.lib.analyzer_process(
            self.handle,
            interleaved,
            usable,
            &mut self.waveform,
            &mut self.spectrum,
        )?;

        let window_ms = if self.sample_rate == 0 {
            0.0
        } else {
            usable as f32 / self.sample_rate as f32 * 1000.0
        };

        Ok(AudioFrame {
            waveform: self.waveform[..waveform_len.min(self.waveform.len())].to_vec(),
            spectrum: self.spectrum[..spectrum_len.min(self.spectrum.len())].to_vec(),
            rms,
            peak,
            window_ms,
        })
    }
}

impl Drop for Analyzer {
    fn drop(&mut self) {
        self.lib.analyzer_destroy(self.handle);
    }
}
