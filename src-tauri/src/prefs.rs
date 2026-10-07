//! 设置读写与广播。
//!
//! 落盘在 `<app_config_dir>/settings.json`，改一次广播 `wp://settings` 给所有窗口。
//! 字段全部有默认值：文件缺失 / 坏掉 / 旧版本缺字段都不能拦住启动。

use std::fs;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager, Runtime};

/// 设置变化广播事件名，与前端 `api.ts` 的 `EVT_SETTINGS` 一致。
pub const SETTINGS_EVENT: &str = "wp://settings";

/// 一条「自定义参数」：控制端下发给壁纸端的业务参数。
///
/// 与 `Settings` 里那些宿主自己消费的字段（音量、帧率……）不同，本结构是**透传**的：
/// 宿主只负责存盘、下发、以及把壁纸端回传的能力列表显示出来，不解释语义。
/// 语义由壁纸端决定（见 `docs/protocol.md` §2.4）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct CustomParam {
    /// 参数键，小写字母/数字/下划线，例：`beat` / `costume`。
    pub key: String,
    /// 参数值。统一按**字符串**传：整数、布尔、枚举都由壁纸端自行解析，
    /// 免得协议里再摊一套类型系统。
    pub value: String,
    /// 锁定：界面上不允许改值。预置的关键参数（如节拍驱动开关）用它防止被误关。
    pub locked: bool,
}

impl Default for CustomParam {
    fn default() -> Self {
        Self {
            key: String::new(),
            value: String::new(),
            locked: false,
        }
    }
}

/// 预置参数键：是否允许节拍驱动壁纸行为（**默认锁定**，防止误关闭整条节奏链路）。
pub const PARAM_BEAT: &str = "beat";
/// 预置参数键：米塔服装（**默认不锁定**，用户可以自由选）。
pub const PARAM_COSTUME: &str = "costume";
/// 服装参数的「随机换一套」魔法值。
pub const PARAM_COSTUME_RANDOM: &str = "random";

/// 接收壁纸端上报的默认 UDP 端口（与推流端口错开，避免自收自发）。
pub const DEFAULT_REPORT_PORT: u16 = 47811;

/// 参数键合法字符：字母、数字、下划线，长度 1–32。
///
/// **保留大小写**：参数键统一转小写会让 `MyKey` 变成 `mykey`，用户在界面上
/// 看到的就不再是自己填的那个键名了。
/// 大小写差异靠**不敏感匹配**兜住（见 `normalize_params` 与 Unity 侧的 `GetCustomParam`）。
fn sanitize_param_key(key: &str) -> String {
    key.trim()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '_')
        .take(32)
        .collect()
}

/// 全部界面偏好。字段含义见 `docs/api-contract.md` 第 1 节。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Settings {
    /* ------------------------------------------------------------- 通用 */
    pub language: String,
    pub theme_mode: String,
    pub library_dir: String,
    pub active_wallpaper: String,
    pub auto_apply: bool,
    pub close_to_tray: bool,

    /* ------------------------------------------------------------- 音频 */
    pub audio_enabled: bool,
    /// `auto` / `process` / `off`
    pub audio_source: String,
    pub audio_process: String,
    pub udp_host: String,
    pub udp_port: u16,
    pub audio_gain: f32,
    pub audio_frame_rate: u32,

    /* ----------------------------------------------------------- 壁纸进程 */
    pub volume: f32,
    pub muted: bool,
    /// 目标帧率，`0` = 不限。
    pub target_fps: u32,
    /// 渲染倍率：0.5 / 0.75 / 1.0。
    pub render_scale: f32,
    /// `-1` = 全部显示器。
    pub monitor_index: i32,
    pub extra_args: String,
    /// 壁纸进程用哪个图形 API：`auto`（Unity 自己挑）/ `d3d11` / `d3d12`。
    ///
    /// 落到启动参数就是 `-force-d3d11` / `-force-d3d12`。存在的意义：
    /// * D3D12 下 Unity 的**动态分辨率**（`ScalableBufferManager`）才生效，渲染倍率才有用；
    /// * 有些工程/驱动在 D3D12 下反而不稳，用户能退回 D3D11。
    pub graphics_api: String,

    /* ------------------------------------------------------------- 行为 */
    pub pause_on_fullscreen: bool,
    pub pause_on_battery: bool,
    pub pause_when_foreground: bool,

    /* ------------------------------------------------------------- 输入 */
    pub input_forward: bool,
    pub input_locked: bool,

    /* --------------------------------------------------- 自定义参数（透传） */
    /// 下发给壁纸端的业务参数列表（见 `CustomParam`）。
    ///
    /// 默认两条：`beat`（锁定）与 `costume`（不锁定）。
    /// 之所以做成"列表"而不是固定字段：壁纸端可以按 key 读取任意扩展参数，
    /// 宿主这边不需要为了一个新玩法再发一版。
    pub custom_params: Vec<CustomParam>,
    /// 接收壁纸端「参数能力上报」的 UDP 端口（0 = 关闭上报通道）。
    ///
    /// 壁纸端把可用服装列表 / 当前值发到这个端口，控制端界面才能给出可选项下拉。
    pub report_port: u16,

    /* ------------------------------------------------------------- 其它 */
    pub auto_start: bool,
    pub preview_width: u32,
    pub preview_height: u32,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            language: "zh-CN".to_string(),
            theme_mode: "dark".to_string(),
            library_dir: String::new(),
            active_wallpaper: String::new(),
            auto_apply: true,
            close_to_tray: true,

            audio_enabled: true,
            audio_source: "auto".to_string(),
            audio_process: String::new(),
            udp_host: "127.0.0.1".to_string(),
            udp_port: 47810,
            audio_gain: 1.0,
            audio_frame_rate: 60,

            volume: 1.0,
            muted: false,
            target_fps: 60,
            render_scale: 1.0,
            // 默认只铺**主显示器**：桌面层 WorkerW 覆盖的是整个虚拟桌面，选「全部显示器」时
            // 窗口会把多屏之间的空洞一起盖住（两块屏错位时尤其明显），所以开箱给一个
            // "一定好看"的值，想跨屏由用户自己选。
            monitor_index: 0,
            extra_args: String::new(),
            // 默认交给 Unity 自己挑（壁纸工程实测默认是 D3D11；想要动态分辨率就在设置里选 D3D12）
            graphics_api: "auto".to_string(),

            pause_on_fullscreen: true,
            pause_on_battery: false,
            pause_when_foreground: false,

            input_forward: false,
            input_locked: false,

            // 预置参数：节拍（锁定，防误关）+ 服装（用户自选）。
            // 服装的初始值 `random` = 每次启动随机换一套，对应原版壁纸行为。
            custom_params: vec![
                CustomParam {
                    key: PARAM_BEAT.to_string(),
                    value: "1".to_string(),
                    locked: true,
                },
                CustomParam {
                    key: PARAM_COSTUME.to_string(),
                    value: PARAM_COSTUME_RANDOM.to_string(),
                    locked: false,
                },
            ],
            report_port: DEFAULT_REPORT_PORT,

            auto_start: false,
            preview_width: 960,
            preview_height: 540,
        }
    }
}

impl Settings {
    /// 把外部传进来的值收敛到合法范围。
    ///
    /// 前端保存的是整份对象，任何一项都可能是脏的（旧版本字段、手改 json、undefined 落成 0），
    /// 所以在落盘之前统一夹一次；顺便保证几个枚举字符串不会变成不认识的取值。
    pub fn normalized(mut self) -> Self {
        let pick = |value: &str, allowed: &[&str], fallback: &str| {
            if allowed.contains(&value) {
                value.to_string()
            } else {
                fallback.to_string()
            }
        };
        self.language = pick(&self.language, &["zh-CN", "en-US"], "zh-CN");
        self.theme_mode = pick(&self.theme_mode, &["dark", "light", "system"], "dark");
        self.audio_source = pick(&self.audio_source, &["auto", "process", "off"], "auto");
        self.graphics_api = pick(&self.graphics_api, &["auto", "d3d11", "d3d12"], "auto");

        self.audio_process = self.audio_process.trim().to_lowercase();
        // 显示器索引：`-1` = 全部显示器，`0..` = 具体某一块；其它脏值一律落回主显示器
        if self.monitor_index < -1 {
            self.monitor_index = 0;
        }
        self.udp_host = if self.udp_host.trim().is_empty() {
            "127.0.0.1".to_string()
        } else {
            self.udp_host.trim().to_string()
        };
        if self.udp_port == 0 {
            self.udp_port = 47810;
        }

        self.audio_gain = clamp_finite(self.audio_gain, 0.1, 4.0, 1.0);
        self.audio_frame_rate = match self.audio_frame_rate {
            0 => 0,
            value if value < 15 => 15,
            value if value > 240 => 240,
            value => value,
        };

        self.volume = clamp_finite(self.volume, 0.0, 1.0, 1.0);
        self.target_fps = match self.target_fps {
            0 => 0,
            value if value < 15 => 30,
            value if value > 360 => 360,
            value => value,
        };
        self.render_scale = match self.render_scale {
            value if (value - 0.5).abs() < 0.01 => 0.5,
            value if (value - 0.75).abs() < 0.01 => 0.75,
            _ => 1.0,
        };
        self.extra_args = self.extra_args.trim().to_string();

        self.preview_width = self.preview_width.clamp(320, 3840);
        self.preview_height = self.preview_height.clamp(180, 2160);

        self.custom_params = normalize_params(std::mem::take(&mut self.custom_params));
        // 上报端口：`0` 是明确的"关闭"，保留；其余低于 1024 的一律落回默认值
        self.report_port = if self.report_port != 0 && self.report_port < 1024 {
            DEFAULT_REPORT_PORT
        } else {
            self.report_port
        };

        if self.audio_source != "process" {
            // 自动模式下不保留旧的手动进程名，免得界面看到一个"其实没生效"的选择
            self.audio_process = String::new();
        }
        self
    }

    /// 壁纸库根目录：`library_dir` 为空时回落到应用数据目录下的 `wallpapers`。
    pub fn library_root<R: Runtime>(&self, app: &AppHandle<R>) -> PathBuf {
        let trimmed = self.library_dir.trim();
        if !trimmed.is_empty() {
            return PathBuf::from(trimmed);
        }
        app.path()
            .app_config_dir()
            .unwrap_or_else(|_| std::env::temp_dir())
            .join("wallpapers")
    }
}

fn clamp_finite(value: f32, min: f32, max: f32, fallback: f32) -> f32 {
    if value.is_finite() {
        value.clamp(min, max)
    } else {
        fallback
    }
}

/// 把已知预置键的写法归一到官方拼写。
///
/// 为什么需要：用户在 `settings.json` 里手打 `BEAT`（大写）也能工作
/// （匹配是大小写不敏感的），但界面上的「键名 → 展示名」是按官方拼写查表的，
/// 归一之后 `beat` 才能显示成「节拍」而不是一串裸键名。
fn canonical_param_key(key: &str) -> &str {
    for known in [PARAM_BEAT, PARAM_COSTUME] {
        if key.eq_ignore_ascii_case(known) {
            return known;
        }
    }
    key
}

/// 收敛自定义参数列表。
///
/// 规则：
/// 1. 键名过一遍 `sanitize_param_key`，空的丢掉；
/// 2. **同键去重**（后面的覆盖前面的），避免界面出现两行同名参数；
/// 3. 值里的换行/制表符压成空格并限长，免得一个超长串把配置包撑爆；
/// 4. **预置参数强制存在**：`beat` 永远补回来并保持锁定，`costume` 缺了补默认值。
///    这是"锁定"承诺的兜底 —— 用户手改 `settings.json` 也关不掉节拍驱动。
fn normalize_params(params: Vec<CustomParam>) -> Vec<CustomParam> {
    let mut out: Vec<CustomParam> = Vec::with_capacity(params.len() + 2);

    for param in params {
        let raw = sanitize_param_key(&param.key);
        if raw.is_empty() {
            continue;
        }
        let key = canonical_param_key(&raw).to_string();
        let value: String = param
            .value
            .replace(['\r', '\n', '\t'], " ")
            .trim()
            .chars()
            .take(256)
            .collect();
        let locked = param.locked;

        // 去重按**大小写不敏感**：`beat` 与 `BEAT` 视为同一条，
        // 免得界面出现两行只差大小写的参数（键名保留先出现的那个写法）。
        match out
            .iter_mut()
            .find(|existing| existing.key.eq_ignore_ascii_case(&key))
        {
            Some(existing) => {
                existing.value = value;
                existing.locked = locked;
            }
            None => out.push(CustomParam {
                key,
                value,
                locked,
            }),
        }
    }

    // 节拍：缺失补 `1` 并强制锁定（"默认带有节拍参数并锁定"的落实点）
    match find_param_ci(&mut out, PARAM_BEAT) {
        Some(existing) => {
            existing.locked = true;
            if existing.value.trim().is_empty() {
                existing.value = "1".to_string();
            }
        }
        None => out.push(CustomParam {
            key: PARAM_BEAT.to_string(),
            value: "1".to_string(),
            locked: true,
        }),
    }

    // 服装：缺失补 `random`（不锁定）
    if find_param_ci(&mut out, PARAM_COSTUME).is_none() {
        out.push(CustomParam {
            key: PARAM_COSTUME.to_string(),
            value: PARAM_COSTUME_RANDOM.to_string(),
            locked: false,
        });
    }

    out
}

/// 大小写不敏感地找一条参数（键名保留原有写法）。
fn find_param_ci<'a>(params: &'a mut [CustomParam], key: &str) -> Option<&'a mut CustomParam> {
    params
        .iter_mut()
        .find(|existing| existing.key.eq_ignore_ascii_case(key))
}

fn config_file<R: Runtime>(app: &AppHandle<R>) -> PathBuf {
    app.path()
        .app_config_dir()
        .unwrap_or_else(|_| std::env::temp_dir())
        .join("settings.json")
}

/// 去掉 UTF-8 BOM。
///
/// 记事本、Windows PowerShell 的 `-Encoding utf8` 都会给文件加 BOM，而 `serde_json`
/// 见到 BOM 会直接判定解析失败。用户手改过配置就很容易踩到，这里统一容错。
pub fn strip_bom(text: &str) -> &str {
    text.trim_start_matches('\u{feff}')
}

/// 读取设置。读不动 / 解析失败一律回退默认值。
pub fn load<R: Runtime>(app: &AppHandle<R>) -> Settings {
    fs::read_to_string(config_file(app))
        .ok()
        .and_then(|text| serde_json::from_str::<Settings>(strip_bom(&text)).ok())
        .unwrap_or_default()
        .normalized()
}

/// 写入设置。只在真正落盘成功后才算成功。
pub fn store<R: Runtime>(app: &AppHandle<R>, settings: &Settings) -> Result<(), String> {
    let file = config_file(app);
    if let Some(dir) = file.parent() {
        fs::create_dir_all(dir).map_err(|e| format!("创建配置目录失败：{e}"))?;
    }
    let text =
        serde_json::to_string_pretty(settings).map_err(|e| format!("序列化设置失败：{e}"))?;
    fs::write(&file, text).map_err(|e| format!("写入 {} 失败：{e}", file.display()))
}

/// 把设置广播给所有窗口。
pub fn broadcast<R: Runtime>(app: &AppHandle<R>, settings: &Settings) {
    let _ = app.emit(SETTINGS_EVENT, settings.clone());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_survive_a_round_trip() {
        let text = serde_json::to_string(&Settings::default()).expect("serialize");
        let back: Settings = serde_json::from_str(&text).expect("deserialize");
        assert_eq!(back, Settings::default());
    }

    #[test]
    fn a_stale_beat_sensitivity_key_is_ignored_without_wiping_other_settings() {
        // `beatSensitivity` 已从 Settings 里删掉（原始端那套节拍算法没有灵敏度旋钮），
        // 但老用户的 settings.json 里还留着这个键。
        // 关键：不认识的键必须被**安静忽略**，而不是让整个反序列化失败 ——
        // 一旦失败，load() 会退回 Settings::default()，用户所有设置会被一次清空。
        let text = r#"{"language":"en-US","audioGain":1.7,"beatSensitivity":1.8,"udpPort":47810}"#;
        let parsed: Settings = serde_json::from_str(text).expect("旧键应当被忽略，不能解析失败");
        assert_eq!(parsed.language, "en-US", "同文件里的其它设置必须保住");
        assert_eq!(parsed.audio_gain, 1.7, "同文件里的其它设置必须保住");
    }

    #[test]
    fn saved_settings_no_longer_contain_beat_sensitivity() {
        // 存盘也不该再写出这个键（否则每次加载都在读一个没人用的字段）
        let text = serde_json::to_string(&Settings::default()).expect("serialize");
        assert!(
            !text.contains("beatSensitivity"),
            "存盘内容里不该再有 beatSensitivity：{text}"
        );
    }

    #[test]
    fn missing_fields_fall_back_to_defaults() {
        let partial: Settings = serde_json::from_str(r#"{"language":"en-US"}"#).expect("partial");
        assert_eq!(partial.language, "en-US");
        assert_eq!(partial.udp_port, 47810);
    }

    #[test]
    fn normalization_clamps_and_pins_enums() {
        let dirty = Settings {
            language: "xx".into(),
            theme_mode: "neon".into(),
            volume: 9.0,
            render_scale: 0.6,
            target_fps: 5,
            audio_source: "process".into(),
            audio_process: "  CloudMusic.EXE ".into(),
            ..Default::default()
        }
        .normalized();

        assert_eq!(dirty.language, "zh-CN");
        assert_eq!(dirty.theme_mode, "dark");
        assert_eq!(dirty.volume, 1.0);
        assert_eq!(dirty.render_scale, 1.0);
        assert_eq!(dirty.target_fps, 30);
        assert_eq!(dirty.audio_process, "cloudmusic.exe");
    }

    #[test]
    fn auto_source_drops_a_stale_manual_process() {
        let settings = Settings {
            audio_source: "auto".into(),
            audio_process: "cloudmusic.exe".into(),
            ..Default::default()
        }
        .normalized();
        assert!(settings.audio_process.is_empty());
    }

    #[test]
    fn a_utf8_bom_does_not_break_parsing() {
        let text = format!("\u{feff}{}", serde_json::to_string(&Settings::default()).unwrap());
        let parsed: Settings = serde_json::from_str(strip_bom(&text)).expect("BOM 应当被吃掉");
        assert_eq!(parsed, Settings::default());
        // 没有 BOM 时也不能被误伤
        assert_eq!(strip_bom("{}"), "{}");
    }

    #[test]
    fn defaults_ship_beat_locked_and_costume_unlocked() {
        let params = Settings::default().custom_params;
        let beat = params.iter().find(|p| p.key == PARAM_BEAT).expect("节拍参数应当预置");
        assert_eq!(beat.value, "1");
        assert!(beat.locked, "节拍参数必须默认锁定");

        let costume = params.iter().find(|p| p.key == PARAM_COSTUME).expect("服装参数应当预置");
        assert_eq!(costume.value, PARAM_COSTUME_RANDOM);
        assert!(!costume.locked, "服装参数默认不锁定");
    }

    #[test]
    fn preset_keys_match_regardless_of_case_but_keep_their_spelling() {
        // 手工写的 settings.json 用了别的写法：应当归一成官方拼写，且用户填的值要保留
        let settings = Settings {
            custom_params: vec![
                CustomParam { key: "COSTUME".into(), value: "casual".into(), locked: false },
            ],
            ..Default::default()
        }
        .normalized();

        let matches: Vec<&CustomParam> = settings
            .custom_params
            .iter()
            .filter(|p| p.key.eq_ignore_ascii_case(PARAM_COSTUME))
            .collect();
        assert_eq!(matches.len(), 1, "大小写不同的同名参数只应保留一条");
        assert_eq!(matches[0].key, PARAM_COSTUME, "应当归一成官方拼写");
        assert_eq!(matches[0].value, "casual", "用户填的值应当被尊重");
    }

    #[test]
    fn a_hand_edited_file_cannot_unlock_or_drop_the_beat_param() {
        // 用户手改 settings.json：试图删掉节拍、或把它解锁并关掉 —— 都必须被纠正回来
        let tampered: Settings = serde_json::from_str(
            r#"{"customParams":[{"key":"BEAT","value":"0","locked":false}]}"#,
        )
        .expect("partial");
        let params = tampered.normalized().custom_params;

        let beat = params
            .iter()
            .find(|p| p.key.eq_ignore_ascii_case(PARAM_BEAT))
            .expect("节拍参数必须被补回");
        assert!(beat.locked, "锁定状态必须被强制恢复");
        assert_eq!(beat.value, "0", "值本身是用户可调的自由度，只锁「不可关闭」这一点");
        assert!(
            params.iter().any(|p| p.key.eq_ignore_ascii_case(PARAM_COSTUME)),
            "服装参数应当被补回"
        );
    }

    #[test]
    fn duplicate_and_dirty_param_keys_collapse() {
        let dirty = Settings {
            custom_params: vec![
                CustomParam { key: " My Key! ".into(), value: "a".into(), locked: false },
                CustomParam { key: "mykey".into(), value: "b".into(), locked: true },
                CustomParam { key: "   ".into(), value: "dropped".into(), locked: false },
                CustomParam { key: "multiline".into(), value: "x\ny".into(), locked: false },
            ],
            ..Default::default()
        }
        .normalized();

        // 脏字符被剔除后与第二条同名 → 后者覆盖前者。
        // 注意 `My Key!` 剔掉非法字符后是 `MyKey`，而第二条是 `mykey` ——
        // 去重按大小写不敏感，所以会合并成一条；键名保留**先出现**的写法。
        let merged = dirty
            .custom_params
            .iter()
            .find(|p| p.key.eq_ignore_ascii_case("mykey"))
            .expect("应当合并为一条");
        assert_eq!(merged.value, "b");
        assert!(merged.locked);
        assert_eq!(
            dirty
                .custom_params
                .iter()
                .filter(|p| p.key.eq_ignore_ascii_case("mykey"))
                .count(),
            1
        );
        // 空键丢弃
        assert!(!dirty.custom_params.iter().any(|p| p.value == "dropped"));
        // 换行被压平
        let multi = dirty.custom_params.iter().find(|p| p.key == "multiline").unwrap();
        assert_eq!(multi.value, "x y");
    }

    #[test]
    fn preset_keys_snap_to_their_official_spelling() {
        // 手打别的写法也能用，但会被归一成官方拼写，界面才能查到展示名
        let settings = Settings {
            custom_params: vec![
                CustomParam { key: "BEAT".into(), value: "1".into(), locked: false },
            ],
            ..Default::default()
        }
        .normalized();

        let beat = settings
            .custom_params
            .iter()
            .find(|p| p.key.eq_ignore_ascii_case(PARAM_BEAT))
            .expect("节拍参数应当存在");
        assert_eq!(beat.key, PARAM_BEAT, "应当归一成官方拼写");
        assert_eq!(beat.value, "1", "用户填的值要保留");
    }

    #[test]
    fn a_silly_report_port_falls_back() {
        let settings = Settings { report_port: 80, ..Default::default() }.normalized();
        assert_eq!(settings.report_port, DEFAULT_REPORT_PORT);
        // 0 = 显式关闭上报，应当被保留
        let off = Settings { report_port: 0, ..Default::default() }.normalized();
        assert_eq!(off.report_port, 0);
    }
}
