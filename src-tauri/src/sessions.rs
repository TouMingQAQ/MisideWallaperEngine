//! 可采集目标列表 —— 数据全部来自采集内核（`pac_enum_targets`）。
//!
//! 窗口枚举 + WASAPI 会话探测 + SMTC 媒体信息 + 按 PID 合并在内核里完成，
//! 宿主只把内核条目转成前端要的形状（契约 2.4）。

use serde::Serialize;

use crate::pac::{
    PacLibrary, PAC_MEDIA_CHANGING, PAC_MEDIA_CLOSED, PAC_MEDIA_OPENED, PAC_MEDIA_PAUSED,
    PAC_MEDIA_PLAYING, PAC_MEDIA_STOPPED, PAC_MEDIA_UNKNOWN, PAC_SESSION_ACTIVE,
    PAC_SESSION_EXPIRED, PAC_SESSION_INACTIVE, PAC_SESSION_NONE,
};

/// 自动跟随时的最小峰值：低于它的进程不算「正在出声」。
pub const CANDIDATE_MIN_PEAK: f32 = 0.015;

/// 一个可采集的音频目标。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AudioTarget {
    pub pid: u32,
    pub process_name: String,
    pub title: String,
    /// 是否有窗口句柄。
    pub has_window: bool,
    /// 内核判定为「有可见窗口」的进程；`false` = 只在音频会话里（播放器缩在托盘）。
    pub window_visible: bool,
    /// `none` / `active` / `inactive` / `expired`
    pub session_state: String,
    pub session_peak: f32,
    pub media_title: String,
    pub media_artist: String,
    pub media_album: String,
    /// `unknown` / `playing` / `paused` / `stopped` / `closed` / `changing` / `opened`
    pub media_status: String,
}

impl AudioTarget {
    /// 是否正在出声。
    pub fn is_playing(&self) -> bool {
        self.session_peak >= CANDIDATE_MIN_PEAK || self.media_status == "playing"
    }
}

/// 问内核要一份当前目标列表，按「正在出声 → 峰值 → 名称」排序。
pub fn list_targets(lib: &PacLibrary) -> Result<Vec<AudioTarget>, String> {
    let snapshot = lib.enumerate_targets()?;
    let self_name = self_process_name();

    let mut targets: Vec<AudioTarget> = snapshot
        .entries
        .into_iter()
        .filter(|entry| {
            // 自己不该出现在可采集列表里（会把自己的提示音采回来）
            !entry.process_name.eq_ignore_ascii_case(&self_name)
        })
        .map(|entry| AudioTarget {
            pid: entry.pid,
            process_name: entry.process_name,
            title: entry.title,
            has_window: entry.hwnd != 0,
            window_visible: entry.has_window,
            session_state: session_state_name(entry.session_state),
            session_peak: entry.session_peak,
            media_title: entry.media_title,
            media_artist: entry.media_artist,
            media_album: entry.media_album,
            media_status: media_status_name(entry.media_status),
        })
        .collect();

    targets.sort_by(|a, b| {
        b.is_playing()
            .cmp(&a.is_playing())
            .then_with(|| {
                b.session_peak
                    .partial_cmp(&a.session_peak)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .then_with(|| a.process_name.cmp(&b.process_name))
    });
    Ok(targets)
}

/// 挑一个最「响」的目标。
pub fn pick_loudest(targets: &[AudioTarget]) -> Option<AudioTarget> {
    targets
        .iter()
        .filter(|target| target.session_peak >= CANDIDATE_MIN_PEAK)
        .max_by(|a, b| {
            a.session_peak
                .partial_cmp(&b.session_peak)
                .unwrap_or(std::cmp::Ordering::Equal)
        })
        .cloned()
}

/// 按进程名找目标（大小写不敏感，也接受不带扩展名的写法）。
pub fn find_by_name(targets: &[AudioTarget], name: &str) -> Option<AudioTarget> {
    let needle = name.trim().to_lowercase();
    if needle.is_empty() {
        return None;
    }
    targets
        .iter()
        .find(|target| target.process_name.to_lowercase() == needle)
        .or_else(|| {
            targets.iter().find(|target| {
                target
                    .process_name
                    .to_lowercase()
                    .trim_end_matches(".exe")
                    == needle.trim_end_matches(".exe")
            })
        })
        .cloned()
}

/// 本进程的可执行文件名（小写）。
pub fn self_process_name() -> String {
    std::env::current_exe()
        .ok()
        .and_then(|path| {
            path.file_name()
                .map(|name| name.to_string_lossy().to_lowercase())
        })
        .unwrap_or_default()
}

fn session_state_name(state: i32) -> String {
    match state {
        PAC_SESSION_ACTIVE => "active",
        PAC_SESSION_INACTIVE => "inactive",
        PAC_SESSION_EXPIRED => "expired",
        _ => {
            debug_assert_eq!(state, PAC_SESSION_NONE);
            "none"
        }
    }
    .to_string()
}

fn media_status_name(status: i32) -> String {
    match status {
        PAC_MEDIA_PLAYING => "playing",
        PAC_MEDIA_PAUSED => "paused",
        PAC_MEDIA_STOPPED => "stopped",
        PAC_MEDIA_CLOSED => "closed",
        PAC_MEDIA_CHANGING => "changing",
        PAC_MEDIA_OPENED => "opened",
        _ => {
            debug_assert_eq!(status, PAC_MEDIA_UNKNOWN);
            "unknown"
        }
    }
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(name: &str, peak: f32) -> AudioTarget {
        AudioTarget {
            pid: 1,
            process_name: name.to_string(),
            title: String::new(),
            has_window: true,
            window_visible: true,
            session_state: "active".to_string(),
            session_peak: peak,
            media_title: String::new(),
            media_artist: String::new(),
            media_album: String::new(),
            media_status: "unknown".to_string(),
        }
    }

    #[test]
    fn the_loudest_playing_target_wins() {
        let list = vec![
            target("quiet.exe", 0.001),
            target("loud.exe", 0.7),
            target("medium.exe", 0.2),
        ];
        assert_eq!(pick_loudest(&list).unwrap().process_name, "loud.exe");
    }

    #[test]
    fn silence_picks_nothing() {
        let list = vec![target("a.exe", 0.0), target("b.exe", 0.001)];
        assert!(pick_loudest(&list).is_none());
    }

    #[test]
    fn lookup_ignores_case_and_the_exe_suffix() {
        let list = vec![target("CloudMusic.exe", 0.5)];
        assert!(find_by_name(&list, "cloudmusic.exe").is_some());
        assert!(find_by_name(&list, "CloudMusic").is_some());
        assert!(find_by_name(&list, "other.exe").is_none());
        assert!(find_by_name(&list, "  ").is_none());
    }
}
