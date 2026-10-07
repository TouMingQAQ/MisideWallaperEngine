//! 宿主库入口：模块划分见下。
//!
//! * `pac` / `dsp` / `capture` —— 采集内核（`ProcessAudioCapture.dll`）的动态绑定与音频会话；
//! * `beat` —— 从频段能量里找节拍、估 BPM；
//! * `audio_link` —— 把节奏数据经 UDP 推给 Unity；
//! * `library` —— 壁纸库（zip 导入 / 索引 / 删除）；
//! * `unity` —— Unity exe 进程的启动、暂停、音量、尺寸；
//! * `win` —— Windows 平台细节（WorkerW 挂载、显示器、输入转发、系统状态）；
//! * `prefs` / `commands` / `tray` —— 设置、IPC 命令、系统托盘。

pub mod audio_link;
pub mod beat;
pub mod capture;
pub mod commands;
pub mod dsp;
pub mod library;
pub mod pac;
pub mod prefs;
pub mod sessions;
pub mod tray;
pub mod unity;
pub mod win;

/// 应用版本（与 `tauri.conf.json` / `package.json` 对齐）。
pub const APP_VERSION: &str = env!("CARGO_PKG_VERSION");

pub fn run() {
    commands::run_app();
}
