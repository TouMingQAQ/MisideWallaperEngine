//! 节拍检测：从低频能量里找 onset，并估个 BPM。
//!
//! 用的是 Patin 那套「即时能量 vs 动态阈值」的经典做法，但把窗口改成**按时间**而不是按帧数 ——
//! 推流帧率是可配的（15–240fps），按帧数取窗口会让灵敏度随帧率漂移。
//!
//! 对外只暴露一个 `process`：喂低频能量与当前时间戳，它告诉这一帧是不是节拍、当前 BPM 大概多少。

use std::collections::VecDeque;
use std::time::Duration;

/// 能量历史窗口：1.2 秒足够放下一个 50 BPM 的鼓点间隔。
const HISTORY: Duration = Duration::from_millis(1200);
/// 两次节拍的最小间隔：对应 300 BPM，防止一个鼓点被连打三下。
const MIN_BEAT_GAP: Duration = Duration::from_millis(180);
/// BPM 估计使用的最近节拍数。
const BPM_BEATS: usize = 12;
/// 固定灵敏度。
///
/// 界面上那个「节拍灵敏度」滑杆已经删掉：壁纸端用的是原始端 `UnityAudioBeat`
/// 那套判定（能量 + 下降沿），没有灵敏度这个概念，所以这个旋钮没有任何东西可调。
/// 宿主这边的检测仍在跑（`beat` / `bpm` 字段还会下发），保留一个固定值即可。
const SENSITIVITY: f32 = 1.0;

/// 单帧的检测结果。
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct BeatInfo {
    /// 本帧是否命中节拍（0 或 1）。
    pub beat: f32,
    /// 估计出的 BPM；样本不足时为 0。
    pub bpm: f32,
    /// 本帧的动态阈值，调试用。
    pub threshold: f32,
    /// 本帧参与比较的能量值。
    pub energy: f32,
}

/// 内部保存的一条能量样本（带时间戳，便于按时间裁剪历史）。
#[derive(Clone, Copy)]
struct Sample {
    at_ms: u64,
    energy: f32,
}

pub struct BeatDetector {
    history: VecDeque<Sample>,
    beat_times: VecDeque<u64>,
    last_beat_ms: Option<u64>,
    last_input_ms: u64,
}

impl Default for BeatDetector {
    fn default() -> Self {
        Self::new()
    }
}

impl BeatDetector {
    pub fn new() -> Self {
        Self {
            history: VecDeque::with_capacity(256),
            beat_times: VecDeque::with_capacity(BPM_BEATS),
            last_beat_ms: None,
            last_input_ms: 0,
        }
    }

    /// 清空历史（切换采集源 / 重新起流时调用，避免拿旧能量当基线）。
    pub fn reset(&mut self) {
        self.history.clear();
        self.beat_times.clear();
        self.last_beat_ms = None;
        self.last_input_ms = 0;
    }

    /// 喂一帧低频能量。
    ///
    /// * `energy` —— 低频能量（0–1，建议用频谱前若干柱的平均值）；
    /// * `at_ms` —— 宿主单调时钟毫秒。
    pub fn process(&mut self, energy: f32, at_ms: u64) -> BeatInfo {
        let energy = if energy.is_finite() {
            energy.max(0.0)
        } else {
            0.0
        };
        // 先取出「上一帧的时间」再更新：写反了的话 at_ms - last_input_ms 恒为 0，
        // 下面那个 quiet 判断就永远不成立（起流中断后的第一帧会误报一个节拍）。
        let previous_input_ms = self.last_input_ms;
        self.last_input_ms = at_ms;

        // 按时间裁掉过老的样本
        while let Some(front) = self.history.front() {
            if at_ms.saturating_sub(front.at_ms) > HISTORY.as_millis() as u64 {
                self.history.pop_front();
            } else {
                break;
            }
        }

        // 至少要有一个完整窗口才谈得上"阈值"，否则开头几帧全是假节拍
        let ready = self.history.len() >= 12;
        let (mean, variance) = {
            let count = self.history.len().max(1) as f32;
            let mean = self.history.iter().map(|s| s.energy).sum::<f32>() / count;
            let variance = self
                .history
                .iter()
                .map(|s| {
                    let diff = s.energy - mean;
                    diff * diff
                })
                .sum::<f32>()
                / count;
            (mean, variance)
        };

        // Patin 的经验系数：方差越大阈值越高（音乐越"吵"，越要更强的瞬态才算节拍）
        let coefficient = (-0.002_571_4 * variance + 1.514_285_7).max(1.0);
        // 固定灵敏度：灵敏度反向作用于系数，调高 = 门槛更接近平均值
        let threshold = (mean * (1.0 + (coefficient - 1.0) / SENSITIVITY)).max(0.012);

        let quiet = at_ms.saturating_sub(previous_input_ms) > 400;
        let gap_ok = self
            .last_beat_ms
            .map(|last| at_ms.saturating_sub(last) >= MIN_BEAT_GAP.as_millis() as u64)
            .unwrap_or(true);
        let hit = ready && !quiet && energy > threshold && energy > 0.02 && gap_ok;

        if hit {
            self.last_beat_ms = Some(at_ms);
            self.beat_times.push_back(at_ms);
            while self.beat_times.len() > BPM_BEATS {
                self.beat_times.pop_front();
            }
        }

        self.history.push_back(Sample { at_ms, energy });
        while self.history.len() > 512 {
            self.history.pop_front();
        }

        BeatInfo {
            beat: if hit { 1.0 } else { 0.0 },
            bpm: self.bpm(),
            threshold,
            energy,
        }
    }

    /// 最近节拍间隔的中位数换算出的 BPM（60–200 之外认为估不准，返回 0）。
    pub fn bpm(&self) -> f32 {
        if self.beat_times.len() < 4 {
            return 0.0;
        }
        let mut gaps: Vec<f32> = self
            .beat_times
            .iter()
            .zip(self.beat_times.iter().skip(1))
            .map(|(a, b)| b.saturating_sub(*a) as f32)
            .filter(|gap| *gap >= MIN_BEAT_GAP.as_millis() as f32)
            .collect();
        if gaps.len() < 3 {
            return 0.0;
        }
        gaps.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let median = gaps[gaps.len() / 2];
        if median <= 0.0 {
            return 0.0;
        }
        let bpm = 60_000.0 / median;
        if (60.0..=200.0).contains(&bpm) {
            (bpm * 10.0).round() / 10.0
        } else {
            0.0
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 以 60fps 喂一个「安静 → 鼓点 → 安静」的序列，应当只在鼓点那一帧命中。
    #[test]
    fn a_loud_transient_after_a_quiet_baseline_is_a_beat() {
        let mut detector = BeatDetector::new();
        let mut hits = Vec::new();
        let mut at = 0u64;
        for frame in 0..120u64 {
            // 每 500ms（30 帧）来一次强瞬态，其余帧只有底噪
            let energy = if frame % 30 == 29 { 0.7 } else { 0.05 };
            let info = detector.process(energy, at);
            if info.beat > 0.0 {
                hits.push(at);
            }
            at += 16;
        }
        assert!(
            (3..=4).contains(&hits.len()),
            "expected ~4 beats, got {hits:?}"
        );
        // 120 BPM（500ms 一次）应当能被估出来
        let bpm = detector.bpm();
        assert!((110.0..=130.0).contains(&bpm), "bpm = {bpm}");
    }

    #[test]
    fn a_steady_tone_produces_no_beats() {
        let mut detector = BeatDetector::new();
        let mut hits = 0;
        let mut at = 0u64;
        for _ in 0..180 {
            if detector.process(0.3, at).beat > 0.0 {
                hits += 1;
            }
            at += 16;
        }
        // 恒定能量没有瞬态，最多允许开头阈值尚未稳定时的一次误判
        assert!(hits <= 1, "steady tone produced {hits} beats");
    }

    #[test]
    fn reset_clears_history() {
        let mut detector = BeatDetector::new();
        detector.process(0.5, 0);
        detector.process(0.5, 16);
        detector.reset();
        assert_eq!(detector.bpm(), 0.0);
        assert!(detector.history.is_empty());
    }

    #[test]
    fn a_long_gap_does_not_fake_a_beat() {
        let mut detector = BeatDetector::new();
        let mut at = 0u64;
        for frame in 0..90u64 {
            let energy = if frame % 30 == 29 { 0.7 } else { 0.05 };
            detector.process(energy, at);
            at += 16;
        }

        // 数据中断 2 秒后重新接上：第一帧不该被当成鼓点
        at += 2000;
        assert_eq!(
            detector.process(0.9, at).beat,
            0.0,
            "长时间中断后的第一帧不是节拍"
        );

        // 中断会把能量历史清空，检测器要重新攒够基线（至少 12 帧）才会再报节拍 ——
        // 这正是我们要的行为：宁可有 200ms 不响应，也别在接上的瞬间乱打一下
        let mut resumed = at + 16;
        for _ in 0..20 {
            detector.process(0.05, resumed);
            resumed += 16;
        }
        assert!(
            detector.process(0.9, resumed).beat > 0.0,
            "基线重建后应当能重新报节拍"
        );
    }
}
