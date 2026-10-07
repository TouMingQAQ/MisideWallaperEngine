//! 把音频节奏数据推给 Unity。
//!
//! 一条通道：
//! * **UDP**（默认 `127.0.0.1:47810`）—— Unity 端零原生插件，丢一个 C# 脚本就能收。
//!
//! 三类包（协议见 `docs/protocol.md`）：
//! * **帧包**（type 1，定长 440 字节）：频谱、波形、响度、节拍、BPM；
//! * **配置包**（type 2，JSON）：目标帧率、音量、是否暂停、语言、屏幕尺寸、自定义参数；
//! * **输入包**（type 3，JSON）：鼠标在壁纸窗口客户区里的位置与按键。
//!
//! 以及一条**反向**通道：
//! * **上报包**（type 4，JSON，Unity → 宿主）：壁纸端把「参数能力」报回来
//!   （可用服装列表、当前值……），控制端界面据此给出可选项。见 `spawn_report_listener`。
//!
//! 帧包由采集发射线程驱动（`publish`），配置 / 输入 / 状态广播由一个 50ms 的常驻线程驱动
//! （`pump`）—— 输入要 60Hz，靠 1.2s 的后台扫描线程是不行的。

use std::net::{IpAddr, SocketAddr, ToSocketAddrs, UdpSocket};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use serde::Serialize;
use tauri::{AppHandle, Emitter, Runtime};

use crate::beat::BeatDetector;
use crate::dsp::AudioFrame;
use crate::prefs::Settings;

/// 帧包魔数。
pub const MAGIC: [u8; 4] = *b"MWAV";
/// 协议版本。
pub const VERSION: u8 = 1;
/// 帧包。
pub const TYPE_FRAME: u8 = 1;
/// 配置包（JSON）。
pub const TYPE_CONFIG: u8 = 2;
/// 输入包（JSON）。
pub const TYPE_INPUT: u8 = 3;
/// 上报包（JSON，**Unity → 宿主**）：壁纸端的参数能力回报。
pub const TYPE_REPORT: u8 = 4;

/// 频谱柱数量。
pub const SPECTRUM_BINS: usize = 128;
/// 波形组数（每组 min/max 两个 i8）。
pub const WAVE_PAIRS: usize = 128;

/// 帧包长度：8 字节头 + 4(seq) + 8(ts) + 4(sr) + 8 个 f32 + 128 + 256 = 440。
pub const FRAME_PACKET_LEN: usize = 440;

/// 给界面画图的事件（与推给 Unity 的帧包解耦：界面不需要 240fps）。
pub const FRAME_EVENT: &str = "wp://audio-frame";
/// 音频状态广播事件。
pub const STATUS_EVENT: &str = "wp://audio-status";
/// 壁纸端上报的参数能力事件（界面据此刷新可选项下拉）。
pub const REPORT_EVENT: &str = "wp://param-report";

/// 推流线程的节奏。
const PUMP_INTERVAL: Duration = Duration::from_millis(10);
/// 配置包的重发间隔。
const CONFIG_INTERVAL: Duration = Duration::from_millis(1000);
/// 输入包的间隔（60Hz）。
const INPUT_INTERVAL: Duration = Duration::from_millis(16);
/// 界面可视化事件的间隔（30fps）。
const EVENT_INTERVAL: Duration = Duration::from_millis(33);

/// 音频状态（契约 2.3）。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AudioStatus {
    pub enabled: bool,
    pub source: String,
    pub process_name: String,
    pub pid: u32,
    pub running: bool,
    pub udp_host: String,
    pub udp_port: u16,
    pub packets_sent: u64,
    /// `idle` / `waiting` / `capturing` / `error`
    pub state: String,
    pub message: String,
    pub dll_ok: bool,
    pub dll_version: u32,
}

impl Default for AudioStatus {
    fn default() -> Self {
        Self {
            enabled: true,
            source: "auto".to_string(),
            process_name: String::new(),
            pid: 0,
            running: false,
            udp_host: "127.0.0.1".to_string(),
            udp_port: 47810,
            packets_sent: 0,
            state: "idle".to_string(),
            message: String::new(),
            dll_ok: false,
            dll_version: 0,
        }
    }
}

/// 界面可视化负载（契约 2.6）。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FramePayload {
    pub pid: u32,
    pub rms: f32,
    pub peak: f32,
    pub level: f32,
    pub beat: f32,
    pub bpm: f32,
    pub bass: f32,
    pub mid: f32,
    pub treble: f32,
    pub spectrum: Vec<u8>,
    /// 128 组交错 `(min, max)`，取值 -127–127（与推给 Unity 的帧包同一份数据）。
    pub waveform: Vec<i8>,
}

/// 壁纸端回报的一条「参数能力」（契约 2.8）。
///
/// 壁纸端是唯一知道 `costume` 到底有哪些可选值的一方，所以由它上报；
/// 宿主这边只做展示与回填，不硬编码任何业务枚举。
#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ParamReport {
    /// 参数键（与 `CustomParam::key` 对应）。
    pub key: String,
    /// 人类可读的参数名（可空，界面回落到 key）。
    pub label: String,
    /// 该参数当前在壁纸端生效的值。
    pub value: String,
    /// 可选值列表；为空表示自由填写（没有枚举可选）。
    pub options: Vec<ParamOption>,
    /// 壁纸端是否接受改这个参数（例如场景里没有换装组件时为 false）。
    pub writable: bool,
}

/// 「参数能力」里的一个可选项。
#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ParamOption {
    pub value: String,
    pub label: String,
}

/// 壁纸端上报的整份快照。
#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReportPayload {
    /// 壁纸端的标识（脚本版本号，便于排查"对面是哪一版"）。
    pub client: String,
    pub params: Vec<ParamReport>,
}

/// 常驻的音频链路。
pub struct AudioLink {
    socket: Option<UdpSocket>,
    target: Option<SocketAddr>,
    enabled: bool,
    gain: f32,
    net_interval: Duration,
    settings: Settings,
    beat: BeatDetector,
    level: f32,
    seq: u32,
    packets: u64,
    last_net: Option<Instant>,
    last_event: Option<Instant>,
    last_config: Option<Instant>,
    last_input: Option<Instant>,
    last_status: Option<Instant>,
    input_publishing: bool,
    paused: bool,
    started: Instant,
    status: AudioStatus,
    last_error: String,
    /// 最近一次壁纸端上报（界面通过 `param_report()` 取）。
    report: Option<ReportPayload>,
    /// 收到最近一次上报的时刻，界面上用于标注"这份能力列表是否还新鲜"。
    report_at: Option<Instant>,
}

impl AudioLink {
    fn new(settings: &Settings) -> Self {
        let mut link = Self {
            socket: None,
            target: None,
            enabled: settings.audio_enabled,
            gain: settings.audio_gain,
            net_interval: net_interval(settings.audio_frame_rate),
            settings: settings.clone(),
            beat: BeatDetector::new(),
            level: 0.0,
            seq: 0,
            packets: 0,
            last_net: None,
            last_event: None,
            last_config: None,
            last_input: None,
            last_status: None,
            input_publishing: false,
            paused: false,
            started: Instant::now(),
            status: AudioStatus::default(),
            last_error: String::new(),
            report: None,
            report_at: None,
        };
        link.apply_settings(settings);
        link
    }

    /// 按设置重开 UDP 套接字。
    ///
    /// socket 是「按地址」的资源：地址变了就重建，失败只记录不 panic ——
    /// 端口被占不该让整个采集停摆。
    fn apply_settings(&mut self, settings: &Settings) {
        self.settings = settings.clone();
        self.enabled = settings.audio_enabled;
        self.gain = settings.audio_gain;
        self.net_interval = net_interval(settings.audio_frame_rate);

        // 先记下旧值：下面要靠"变了没有"决定要不要重建 socket
        let old_host = self.status.udp_host.clone();
        let old_port = self.status.udp_port;

        self.status.enabled = settings.audio_enabled;
        self.status.source = settings.audio_source.clone();

        // ---- UDP
        let address = resolve(&settings.udp_host, settings.udp_port);
        let need_socket =
            self.socket.is_none() || old_host != settings.udp_host || old_port != settings.udp_port;
        if need_socket {
            self.socket = None;
            self.target = None;
            match UdpSocket::bind(("127.0.0.1", 0)) {
                Ok(socket) => {
                    let _ = socket.set_broadcast(true);
                    self.socket = Some(socket);
                }
                Err(err) => {
                    self.last_error = format!("创建 UDP 套接字失败：{err}");
                }
            }
        }
        match address {
            Ok(address) => self.target = Some(address),
            Err(err) => {
                self.target = None;
                self.last_error = format!("UDP 目标地址无效：{err}");
            }
        }

        self.status.udp_host = settings.udp_host.clone();
        self.status.udp_port = settings.udp_port;
    }

    fn send_frame(&mut self, packet: &[u8]) {
        let mut sent = false;
        if let (Some(socket), Some(target)) = (&self.socket, self.target) {
            match socket.send_to(packet, target) {
                Ok(_) => sent = true,
                Err(err) => self.last_error = format!("UDP 发送失败：{err}"),
            }
        }
        if sent {
            self.packets = self.packets.wrapping_add(1);
        }
    }

    fn send_json(&mut self, kind: u8, json: &str) {
        let mut packet = Vec::with_capacity(json.len() + 16);
        packet.extend_from_slice(&MAGIC);
        packet.push(VERSION);
        packet.push(kind);
        packet.extend_from_slice(&0u16.to_le_bytes());
        packet.extend_from_slice(&self.seq.wrapping_add(1).to_le_bytes());
        let bytes = json.as_bytes();
        packet.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
        packet.extend_from_slice(bytes);
        if let (Some(socket), Some(target)) = (&self.socket, self.target) {
            let _ = socket.send_to(&packet, target);
        }
    }
}

/// 立即重发一次配置包（不等下一个 1 秒周期）。
///
/// 用在"用户刚点了按钮、希望壁纸马上有反应"的场景：配置包平时每秒一发，
/// 若不自作主张补一发，点一次头最坏要等 1 秒才生效，手感像卡了。
///
/// 由 `settings` 变化触发（见 `commands::save_settings`）；没在推流时静默跳过。
pub fn flush_config() {
    let mut guard = lock();
    let Some(link) = guard.as_mut() else {
        return;
    };
    if !link.enabled {
        return;
    }
    let json = config_json(link);
    link.send_json(TYPE_CONFIG, &json);
    link.last_config = Some(Instant::now());
}

fn net_interval(fps: u32) -> Duration {
    if fps == 0 {
        Duration::from_millis(4)
    } else {
        Duration::from_millis((1000 / fps.max(1) as u64).clamp(4, 200))
    }
}

fn resolve(host: &str, port: u16) -> Result<SocketAddr, String> {
    if let Ok(ip) = host.parse::<IpAddr>() {
        return Ok(SocketAddr::new(ip, port));
    }
    (host, port)
        .to_socket_addrs()
        .map_err(|e| format!("{host}:{port} 解析失败：{e}"))?
        .next()
        .ok_or_else(|| format!("{host}:{port} 解析不出地址"))
}

/* ------------------------------------------------------------ 全局单例 */

static LINK: Mutex<Option<AudioLink>> = Mutex::new(None);

fn lock() -> MutexGuard<'static, Option<AudioLink>> {
    LINK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// 应用设置（启动时调一次，之后每次保存设置再调）。
pub fn configure(settings: &Settings) {
    let mut guard = lock();
    match guard.as_mut() {
        Some(link) => link.apply_settings(settings),
        None => *guard = Some(AudioLink::new(settings)),
    }
}

/// 采集源变化 / 重新起流时清掉节拍历史。
pub fn reset_source() {
    if let Some(link) = lock().as_mut() {
        link.beat.reset();
        link.level = 0.0;
        link.last_net = None;
    }
}

/// 更新会话信息（由采集层在每次扫描后写入）。
pub fn update_session(
    source: &str,
    process_name: &str,
    pid: u32,
    running: bool,
    state: &str,
    message: &str,
    dll_ok: bool,
    dll_version: u32,
) {
    if let Some(link) = lock().as_mut() {
        link.status.source = source.to_string();
        link.status.process_name = process_name.to_string();
        link.status.pid = pid;
        link.status.running = running;
        link.status.state = state.to_string();
        link.status.message = message.to_string();
        link.status.dll_ok = dll_ok;
        link.status.dll_version = dll_version;
    }
}

/// 壁纸进程当前是否被暂停（配置包里要如实告诉 Unity）。
pub fn set_paused(paused: bool) {
    if let Some(link) = lock().as_mut() {
        link.paused = paused;
    }
}

/// 输入转发是否开着（开着才发输入包）。
pub fn set_input_publishing(enabled: bool) {
    if let Some(link) = lock().as_mut() {
        link.input_publishing = enabled;
    }
}

/// 当前状态。
pub fn status() -> AudioStatus {
    lock()
        .as_ref()
        .map(|link| link.status.clone())
        .unwrap_or_default()
}

/// 已发出的包数。
pub fn packets_sent() -> u64 {
    lock().as_ref().map(|link| link.packets).unwrap_or(0)
}

/// 最近一次错误（没有则空串）。
pub fn last_error() -> String {
    lock()
        .as_ref()
        .map(|link| link.last_error.clone())
        .unwrap_or_default()
}

/// 采集发射线程每出一帧就调一次：转成协议格式，推给 Unity，顺便给界面发一份。
///
/// `pid` / `sample_rate` 用来标注数据来源；`app` 只用于界面事件。
pub fn publish<R: Runtime>(app: &AppHandle<R>, pid: u32, sample_rate: u32, frame: &AudioFrame) {
    let mut guard = lock();
    let Some(link) = guard.as_mut() else {
        return;
    };
    if !link.enabled {
        // 关掉推流时仍然给界面发数据，界面上那个频谱表还是要动的
        emit_to_ui(app, link, pid, frame, 0.0, 0.0);
        return;
    }

    // 网络推流按「推流帧率」限速；界面事件另按 30fps 限速
    let now = Instant::now();
    let net_due = link
        .last_net
        .map(|at| now.duration_since(at) >= link.net_interval)
        .unwrap_or(true);

    let bass = mean(&frame.spectrum, 0, 8) * link.gain;
    let mid = mean(&frame.spectrum, 8, 40) * link.gain;
    let treble = mean(&frame.spectrum, 40, SPECTRUM_BINS) * link.gain;
    let info = link.beat.process(bass, elapsed_ms(link.started));

    // 平滑响度：上升快、回落慢，界面和 Unity 两边都是这个值
    let rms = (frame.rms * link.gain).clamp(0.0, 1.0);
    link.level = if rms > link.level {
        rms
    } else {
        (link.level * 0.88 + rms * 0.12).clamp(0.0, 1.0)
    };

    if net_due {
        link.last_net = Some(now);
        link.seq = link.seq.wrapping_add(1);
        let level = link.level;
        let peak = (frame.peak * link.gain).clamp(0.0, 1.0);
        let (bass, mid, treble) = (
            bass.clamp(0.0, 1.0),
            mid.clamp(0.0, 1.0),
            treble.clamp(0.0, 1.0),
        );
        let packet = build_frame_packet(
            link.seq,
            elapsed_ms(link.started),
            sample_rate,
            rms,
            peak,
            level,
            info.beat,
            info.bpm,
            bass,
            mid,
            treble,
            &frame.spectrum,
            &frame.waveform,
        );
        link.send_frame(&packet);
    }

    emit_to_ui(app, link, pid, frame, info.beat, info.bpm);
}

fn emit_to_ui<R: Runtime>(
    app: &AppHandle<R>,
    link: &mut AudioLink,
    pid: u32,
    frame: &AudioFrame,
    beat: f32,
    bpm: f32,
) {
    let now = Instant::now();
    if let Some(last) = link.last_event {
        if now.duration_since(last) < EVENT_INTERVAL {
            return;
        }
    }
    link.last_event = Some(now);

    let spectrum: Vec<u8> = frame
        .spectrum
        .iter()
        .take(SPECTRUM_BINS)
        .map(|value| (value.clamp(0.0, 1.0) * 255.0).round() as u8)
        .collect();

    let _ = app.emit(
        FRAME_EVENT,
        FramePayload {
            pid,
            rms: (frame.rms * link.gain).clamp(0.0, 1.0),
            peak: (frame.peak * link.gain).clamp(0.0, 1.0),
            level: link.level,
            beat,
            bpm,
            bass: mean(&frame.spectrum, 0, 8).clamp(0.0, 1.0),
            mid: mean(&frame.spectrum, 8, 40).clamp(0.0, 1.0),
            treble: mean(&frame.spectrum, 40, SPECTRUM_BINS).clamp(0.0, 1.0),
            spectrum,
            waveform: downsample_waveform(&frame.waveform),
        },
    );
}

/// 内核的 256 组 `(min, max)` 两两合并成 128 组，压成 i8。
///
/// 界面波形与 UDP 帧包用的是同一份结果 —— 两处各算一遍很容易算歪。
fn downsample_waveform(waveform: &[f32]) -> Vec<i8> {
    let mut out = Vec::with_capacity(WAVE_PAIRS * 2);
    for index in 0..WAVE_PAIRS {
        let base = index * 4;
        let min = waveform
            .get(base)
            .copied()
            .unwrap_or(0.0)
            .min(waveform.get(base + 2).copied().unwrap_or(0.0));
        let max = waveform
            .get(base + 1)
            .copied()
            .unwrap_or(0.0)
            .max(waveform.get(base + 3).copied().unwrap_or(0.0));
        out.push((min.clamp(-1.0, 1.0) * 127.0).round() as i8);
        out.push((max.clamp(-1.0, 1.0) * 127.0).round() as i8);
    }
    out
}

/// 常驻推流线程：配置包 / 输入包 / 状态广播。
///
/// 帧包不在这里发 —— 它跟着采集节奏走（见 `publish`）。
pub fn spawn_pump<R: Runtime>(app: AppHandle<R>) {
    std::thread::Builder::new()
        .name("audio-link-pump".to_string())
        .spawn(move || loop {
            std::thread::sleep(PUMP_INTERVAL);
            pump_once(&app);
        })
        .expect("无法创建音频推流线程");
}

/// 常驻上报接收线程：监听壁纸端发回来的「参数能力」。
///
/// 为什么单独一条线程而不是复用推流线程：推流是**发**，这里要**阻塞收**，
/// 混在一起会让配置包被 `recv_from` 卡住。端口由 `Settings::report_port` 决定，
/// 设成 0 表示关闭（不 bind，界面也就没有可选项下拉）。
///
/// 端口变了会重建 socket：这里用一个"期望端口"比对，
/// 避免为了改个端口就把整个进程重启。
pub fn spawn_report_listener<R: Runtime>(app: AppHandle<R>) {
    std::thread::Builder::new()
        .name("audio-link-report".to_string())
        .spawn(move || {
            let mut socket: Option<UdpSocket> = None;
            let mut bound_port: u16 = 0;
            let mut buffer = vec![0u8; 64 * 1024];
            loop {
                let wanted = lock()
                    .as_ref()
                    .map(|link| link.settings.report_port)
                    .unwrap_or(0);

                // 期望端口变化：关掉旧的重开
                if wanted != bound_port {
                    socket = None;
                    bound_port = 0;
                    if wanted != 0 {
                        match UdpSocket::bind(("127.0.0.1", wanted)) {
                            Ok(sock) => {
                                // 读超时让线程周期性回到循环顶部，从而能感知端口变化
                                let _ = sock.set_read_timeout(Some(Duration::from_millis(500)));
                                socket = Some(sock);
                                bound_port = wanted;
                            }
                            Err(err) => {
                                if let Some(link) = lock().as_mut() {
                                    link.last_error =
                                        format!("上报端口 {wanted} 绑定失败：{err}");
                                }
                                std::thread::sleep(Duration::from_secs(2));
                                continue;
                            }
                        }
                    }
                }

                let Some(sock) = socket.as_ref() else {
                    // 上报关闭：睡一会儿再看设置
                    std::thread::sleep(Duration::from_millis(500));
                    continue;
                };

                match sock.recv_from(&mut buffer) {
                    Ok((count, _from)) => {
                        if let Some(payload) = parse_report(&buffer[..count]) {
                            let mut guard = lock();
                            if let Some(link) = guard.as_mut() {
                                link.report = Some(payload.clone());
                                link.report_at = Some(Instant::now());
                            }
                            let _ = app.emit(REPORT_EVENT, payload);
                        }
                    }
                    Err(err) => {
                        // 超时是常态（用来回到循环顶部检查端口变化）
                        if !matches!(
                            err.kind(),
                            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                        ) {
                            std::thread::sleep(Duration::from_millis(200));
                        }
                    }
                }
            }
        })
        .expect("无法创建参数上报线程");
}

/// 解析一条上报包（`magic + version + type=4 + reserved + seq + jsonLen + JSON`）。
///
/// 宽容策略：magic / 版本 / 类型不对、JSON 坏掉，都只丢这一包，不报错也不 panic ——
/// 这个端口上完全可能有别的程序在发包。
fn parse_report(data: &[u8]) -> Option<ReportPayload> {
    const HEADER: usize = 16;
    if data.len() < HEADER {
        return None;
    }
    if &data[0..4] != &MAGIC {
        return None;
    }
    if data[4] != VERSION || data[5] != TYPE_REPORT {
        return None;
    }
    let declared = u32::from_le_bytes([data[12], data[13], data[14], data[15]]) as usize;
    if declared == 0 || HEADER + declared > data.len() {
        return None;
    }
    let json = std::str::from_utf8(&data[HEADER..HEADER + declared]).ok()?;
    let value: serde_json::Value = serde_json::from_str(json).ok()?;

    let client = value
        .get("client")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();

    let mut params = Vec::new();
    if let Some(list) = value.get("params").and_then(|v| v.as_array()) {
        for item in list {
            let key = item
                .get("key")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .trim()
                .to_string();
            if key.is_empty() {
                continue;
            }
            let mut options = Vec::new();
            if let Some(raw) = item.get("options").and_then(|v| v.as_array()) {
                for option in raw {
                    // 可选项允许两种写法：`"value"` 或 `{ "value": ..., "label": ... }`
                    match option {
                        serde_json::Value::String(text) => options.push(ParamOption {
                            value: text.clone(),
                            label: text.clone(),
                        }),
                        serde_json::Value::Object(_) => {
                            let value = option
                                .get("value")
                                .and_then(|v| v.as_str())
                                .unwrap_or_default()
                                .to_string();
                            if value.is_empty() {
                                continue;
                            }
                            let label = option
                                .get("label")
                                .and_then(|v| v.as_str())
                                .unwrap_or(&value)
                                .to_string();
                            options.push(ParamOption { value, label });
                        }
                        _ => {}
                    }
                }
            }
            params.push(ParamReport {
                key,
                label: item
                    .get("label")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string(),
                value: item
                    .get("value")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string(),
                options,
                writable: item
                    .get("writable")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(true),
            });
        }
    }

    Some(ReportPayload { client, params })
}

/// 最近一次壁纸端上报（界面用）。
pub fn param_report() -> Option<ReportPayload> {
    lock().as_ref().and_then(|link| link.report.clone())
}

/// 距最近一次上报的毫秒数；从未收到过返回 `None`。
///
/// 界面用它显示"能力列表来自多久以前"，超时就该提示壁纸端没在跑。
pub fn report_age_ms() -> Option<u64> {
    lock()
        .as_ref()
        .and_then(|link| link.report_at.map(|at| at.elapsed().as_millis() as u64))
}

fn pump_once<R: Runtime>(app: &AppHandle<R>) {
    let mut guard = lock();
    let Some(link) = guard.as_mut() else {
        return;
    };
    let now = Instant::now();

    // 配置包：起流立刻一发，之后每秒一发
    let config_due = link
        .last_config
        .map(|at| now.duration_since(at) >= CONFIG_INTERVAL)
        .unwrap_or(true);
    if config_due && link.enabled {
        link.last_config = Some(now);
        let json = config_json(link);
        link.send_json(TYPE_CONFIG, &json);
    }

    // 输入包：只在转发打开时按 60Hz 发
    if link.input_publishing {
        let input_due = link
            .last_input
            .map(|at| now.duration_since(at) >= INPUT_INTERVAL)
            .unwrap_or(true);
        if input_due {
            link.last_input = Some(now);
            let json = input_json(link);
            link.send_json(TYPE_INPUT, &json);
        }
    }

    // 状态广播（1Hz）：界面的音频页靠它显示"在等哪个进程"
    link.status.packets_sent = link.packets;
    let status_due = link
        .last_status
        .map(|at| now.duration_since(at) >= CONFIG_INTERVAL)
        .unwrap_or(true);
    if status_due {
        link.last_status = Some(now);
        if !link.last_error.is_empty() {
            link.status.message = link.last_error.clone();
        }
        let _ = app.emit(STATUS_EVENT, link.status.clone());
    }
}

fn config_json(link: &AudioLink) -> String {
    let screenshot = crate::win::desktop::target_rect(link.settings.monitor_index);
    // 自定义参数透传：宿主不解释语义，只把 [{key, value, locked}] 原样交给壁纸端。
    let params: Vec<serde_json::Value> = link
        .settings
        .custom_params
        .iter()
        .map(|param| {
            serde_json::json!({
                "key": param.key,
                "value": param.value,
                "locked": param.locked,
            })
        })
        .collect();
    let value = serde_json::json!({
        "type": "config",
        "targetFps": link.settings.target_fps,
        "renderScale": link.settings.render_scale,
        "muted": link.settings.muted,
        "volume": link.settings.volume,
        "paused": link.paused,
        "language": link.settings.language,
        "audioGain": link.settings.audio_gain,
        // 自定义参数（透传）与上报端口：壁纸端读到 reportPort 才会把参数能力报回来
        "params": params,
        "reportPort": link.settings.report_port,
        "screen": {
            "x": screenshot.left,
            "y": screenshot.top,
            "width": screenshot.right - screenshot.left,
            "height": screenshot.bottom - screenshot.top,
        },
        "source": {
            "pid": link.status.pid,
            "process": link.status.process_name,
        },
    });
    value.to_string()
}

fn input_json(link: &AudioLink) -> String {
    let pointer = crate::win::input::pointer_state();
    let hwnd = crate::win::desktop::last_attached()
        .map(|value| windows::Win32::Foundation::HWND(value as *mut std::ffi::c_void));
    let (width, height) = hwnd
        .map(crate::win::desktop::client_size)
        .unwrap_or((0, 0));
    serde_json::json!({
        "type": "input",
        "pointer": {
            "x": pointer.x,
            "y": pointer.y,
            "inside": pointer.inside,
            "left": pointer.left,
            "right": pointer.right,
            "middle": pointer.middle,
            "wheel": pointer.wheel,
        },
        "client": { "width": width, "height": height },
        "locked": link.settings.input_locked,
    })
    .to_string()
}

/// 组一个定长帧包。
#[allow(clippy::too_many_arguments)]
fn build_frame_packet(
    seq: u32,
    timestamp_ms: u64,
    sample_rate: u32,
    rms: f32,
    peak: f32,
    level: f32,
    beat: f32,
    bpm: f32,
    bass: f32,
    mid: f32,
    treble: f32,
    spectrum: &[f32],
    waveform: &[f32],
) -> Vec<u8> {
    let mut packet = vec![0u8; FRAME_PACKET_LEN];
    packet[0..4].copy_from_slice(&MAGIC);
    packet[4] = VERSION;
    packet[5] = TYPE_FRAME;
    packet[8..12].copy_from_slice(&seq.to_le_bytes());
    packet[12..20].copy_from_slice(&timestamp_ms.to_le_bytes());
    packet[20..24].copy_from_slice(&sample_rate.to_le_bytes());
    for (offset, value) in [
        (24usize, rms),
        (28, peak),
        (32, level),
        (36, beat),
        (40, bass),
        (44, mid),
        (48, treble),
        (52, bpm),
    ] {
        packet[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
    }

    // 频谱：128 个 0–255
    for index in 0..SPECTRUM_BINS {
        let value = spectrum.get(index).copied().unwrap_or(0.0).clamp(0.0, 1.0);
        packet[56 + index] = (value * 255.0).round() as u8;
    }

    // 波形：交错 min/max（偶偏移 = min，奇偏移 = max），与 Unity 端实现一致
    for (offset, value) in downsample_waveform(waveform).iter().enumerate() {
        packet[184 + offset] = *value as u8;
    }

    packet
}

fn mean(values: &[f32], from: usize, to: usize) -> f32 {
    let slice = values.get(from..to.min(values.len()));
    match slice {
        Some(slice) if !slice.is_empty() => {
            slice.iter().copied().map(|value| value.max(0.0)).sum::<f32>() / slice.len() as f32
        }
        _ => 0.0,
    }
}

fn elapsed_ms(started: Instant) -> u64 {
    started.elapsed().as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine_spectrum() -> Vec<f32> {
        (0..SPECTRUM_BINS)
            .map(|index| index as f32 / SPECTRUM_BINS as f32)
            .collect()
    }

    #[test]
    fn frame_packet_layout_matches_the_contract() {
        let spectrum = sine_spectrum();
        let waveform: Vec<f32> = (0..512).map(|i| if i % 2 == 0 { -0.5 } else { 0.5 }).collect();
        let packet = build_frame_packet(
            7, 1234, 48_000, 0.25, 0.5, 0.3, 1.0, 120.0, 0.4, 0.3, 0.2, &spectrum, &waveform,
        );
        assert_eq!(packet.len(), FRAME_PACKET_LEN);
        assert_eq!(&packet[0..4], b"MWAV");
        assert_eq!(packet[4], VERSION);
        assert_eq!(packet[5], TYPE_FRAME);
        assert_eq!(u32::from_le_bytes(packet[8..12].try_into().unwrap()), 7);
        assert_eq!(u64::from_le_bytes(packet[12..20].try_into().unwrap()), 1234);
        assert_eq!(u32::from_le_bytes(packet[20..24].try_into().unwrap()), 48_000);
        assert!((f32::from_le_bytes(packet[24..28].try_into().unwrap()) - 0.25).abs() < 1e-6);
        assert!((f32::from_le_bytes(packet[52..56].try_into().unwrap()) - 120.0).abs() < 1e-4);
        // 频谱第一柱 0，最后一柱接近 255
        assert_eq!(packet[56], 0);
        assert!(packet[56 + SPECTRUM_BINS - 1] >= 250);
        // 波形两两合并后仍是 -0.5 / 0.5（交错存放：偶 = min，奇 = max）
        assert_eq!(packet[184] as i8, -64);
        assert_eq!(packet[185] as i8, 64);
        assert_eq!(packet[184 + WAVE_PAIRS * 2 - 1] as i8, 64);
    }

    #[test]
    fn missing_spectrum_bins_do_not_panic() {
        let packet = build_frame_packet(
            1, 0, 48_000, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, &[], &[],
        );
        assert_eq!(packet.len(), FRAME_PACKET_LEN);
        assert!(packet[56..184].iter().all(|value| *value == 0));
    }

    #[test]
    fn band_split_covers_the_whole_spectrum() {
        let spectrum = sine_spectrum();
        let bass = mean(&spectrum, 0, 8);
        let mid = mean(&spectrum, 8, 40);
        let treble = mean(&spectrum, 40, SPECTRUM_BINS);
        assert!(bass < mid && mid < treble, "对数频谱的均值应当随频率递增");
        assert!(treble <= 1.0);
    }

    #[test]
    fn interval_limits_are_sane() {
        assert_eq!(net_interval(0), Duration::from_millis(4));
        assert_eq!(net_interval(60), Duration::from_millis(16));
        assert_eq!(net_interval(1000), Duration::from_millis(4));
        assert_eq!(net_interval(1), Duration::from_millis(200));
    }

    /// 拼一条上报包，省得每个测试各写一遍包头。
    fn report_packet(json: &str) -> Vec<u8> {
        let mut packet = Vec::new();
        packet.extend_from_slice(&MAGIC);
        packet.push(VERSION);
        packet.push(TYPE_REPORT);
        packet.extend_from_slice(&0u16.to_le_bytes());
        packet.extend_from_slice(&1u32.to_le_bytes());
        packet.extend_from_slice(&(json.len() as u32).to_le_bytes());
        packet.extend_from_slice(json.as_bytes());
        packet
    }

    #[test]
    fn a_report_round_trips_with_options() {
        let json = r#"{
            "type": "report",
            "client": "wallpaper-1.0",
            "params": [
                {
                    "key": "costume",
                    "label": "米塔服装",
                    "value": "HellVamp",
                    "writable": true,
                    "options": [
                        { "value": "random", "label": "随机" },
                        { "value": "original", "label": "原皮" },
                        "HellVamp"
                    ]
                }
            ]
        }"#;
        let report = parse_report(&report_packet(json)).expect("应当解析成功");

        assert_eq!(report.client, "wallpaper-1.0");
        assert_eq!(report.params.len(), 1);
        let costume = &report.params[0];
        assert_eq!(costume.key, "costume");
        assert_eq!(costume.label, "米塔服装");
        assert_eq!(costume.value, "HellVamp");
        assert!(costume.writable);
        assert_eq!(costume.options.len(), 3);
        assert_eq!(costume.options[0].label, "随机");
        // 字符串简写：label 回落到 value 本身
        assert_eq!(costume.options[2].value, "HellVamp");
        assert_eq!(costume.options[2].label, "HellVamp");
    }

    #[test]
    fn garbage_on_the_report_port_is_ignored_not_fatal() {
        // 这个端口上完全可能有别的程序在发包 —— 一律当作"不是我们的包"丢掉
        assert!(parse_report(&[]).is_none());
        assert!(parse_report(b"hello world").is_none());
        // magic 对但类型错（例如把帧包发过来了）
        let mut wrong_type = report_packet("{}");
        wrong_type[5] = TYPE_FRAME;
        assert!(parse_report(&wrong_type).is_none());
        // jsonLen 声称比实际长：越界必须被挡住，不能靠后面的切片 panic
        let mut lying = report_packet("{}");
        let bad_len = 9_999u32.to_le_bytes();
        lying[12..16].copy_from_slice(&bad_len);
        assert!(parse_report(&lying).is_none());
        // JSON 本身坏掉
        assert!(parse_report(&report_packet("{not json")).is_none());
    }

    #[test]
    fn report_without_params_is_still_valid() {
        // 壁纸端可能只报个到（还没有任何参数），界面不该因此报错
        let report = parse_report(&report_packet(r#"{"client":"v1"}"#)).expect("应当解析成功");
        assert_eq!(report.client, "v1");
        assert!(report.params.is_empty());
    }

    #[test]
    fn report_drops_entries_without_a_key() {
        let json = r#"{"params":[{"key":"  ","value":"x"},{"key":"beat","value":"1"}]}"#;
        let report = parse_report(&report_packet(json)).expect("应当解析成功");
        assert_eq!(report.params.len(), 1);
        assert_eq!(report.params[0].key, "beat");
        // 缺 writable 字段时默认按"可写"处理（对端是老版本也要能选）
        assert!(report.params[0].writable);
    }

    #[test]
    fn config_json_carries_params_and_the_report_port() {
        let settings = Settings::default();
        let link = AudioLink::new(&settings);
        let value: serde_json::Value =
            serde_json::from_str(&config_json(&link)).expect("配置包应当是合法 JSON");

        let params = value.get("params").and_then(|v| v.as_array()).expect("应当带 params");
        let beat = params
            .iter()
            .find(|p| p.get("key").and_then(|v| v.as_str()) == Some("beat"))
            .expect("应当带节拍参数");
        assert_eq!(beat.get("locked").and_then(|v| v.as_bool()), Some(true));

        let costume = params
            .iter()
            .find(|p| p.get("key").and_then(|v| v.as_str()) == Some("costume"))
            .expect("应当带服装参数");
        assert_eq!(costume.get("locked").and_then(|v| v.as_bool()), Some(false));

        assert_eq!(
            value.get("reportPort").and_then(|v| v.as_u64()),
            Some(crate::prefs::DEFAULT_REPORT_PORT as u64)
        );
    }

    #[test]
    fn config_json_ships_the_params_the_wallpaper_expects() {
        // 控制端与壁纸端之间的契约：键名 / 默认值任何一边改了，这个测试都会立刻炸
        let settings = Settings::default();
        let link = AudioLink::new(&settings);
        let value: serde_json::Value =
            serde_json::from_str(&config_json(&link)).expect("配置包应当是合法 JSON");
        let params = value.get("params").and_then(|v| v.as_array()).expect("应当带 params");

        let find = |key: &str| -> String {
            params
                .iter()
                .find(|p| p.get("key").and_then(|v| v.as_str()) == Some(key))
                .unwrap_or_else(|| panic!("应当带 {key} 参数"))
                .get("value")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string()
        };

        assert_eq!(find("beat"), "1", "节拍驱动默认开启");
        assert_eq!(find("costume"), "random", "服装默认随机换一套");

        // beat 锁定（防误关整条节奏链路），costume 不锁定（用户随时能选）
        let locked = |key: &str| -> Option<bool> {
            params
                .iter()
                .find(|p| p.get("key").and_then(|v| v.as_str()) == Some(key))
                .and_then(|p| p.get("locked"))
                .and_then(|v| v.as_bool())
        };
        assert_eq!(locked("beat"), Some(true), "beat 应当被锁定");
        assert_eq!(locked("costume"), Some(false), "costume 不应被锁定");
    }
}
