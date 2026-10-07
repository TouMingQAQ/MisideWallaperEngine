# miside-wallpaper-engine 宿主 · 接口契约

> 这份文件是**前后端与 Unity 端共同的唯一事实来源**。三边同时开发时，任何一边都不许私自改字段名；
> 需要改先改这里。

工程根：仓库根目录（Tauri 2 + TypeScript + Rust）

---

## 1. 设置对象 `Settings`

持久化在 `<app_config_dir>/settings.json`，Rust 侧 `src-tauri/src/prefs.rs` 的 `Settings` 结构体，
JSON 一律 **camelCase**。字段全部有默认值：文件缺失 / 坏掉 / 缺字段都不能拦住启动。

| 字段 | 类型 | 默认 | 含义 |
| --- | --- | --- | --- |
| `language` | string | `"zh-CN"` | `zh-CN` / `en-US` |
| `themeMode` | string | `"dark"` | `dark` / `light` / `system` |
| `libraryDir` | string | `""` | 壁纸库根目录；空 = 用应用数据目录下的 `wallpapers` |
| `activeWallpaper` | string | `""` | 当前生效的壁纸 id（空 = 无） |
| `autoApply` | bool | `true` | 启动时自动应用 `activeWallpaper` |
| `audioEnabled` | bool | `true` | 是否推送音频节奏数据 |
| `audioSource` | string | `"auto"` | `auto`（自动跟随最响的进程）/ `process`（指定）/ `off` |
| `audioProcess` | string | `""` | `audioSource = "process"` 时的进程名（小写、带扩展名） |
| `udpHost` | string | `"127.0.0.1"` | UDP 目标地址（音频数据只走 UDP） |
| `udpPort` | number | `47810` | UDP 目标端口 |
| `audioGain` | number | `1.0` | 频段与电平的整体增益（0.1–4） |
| `audioFrameRate` | number | `60` | 推流帧率上限，`0` = 不限制 |
| `volume` | number | `1.0` | 壁纸进程音量 0–1（走 WASAPI 会话音量） |
| `muted` | bool | `false` | 壁纸进程静音 |
| `targetFps` | number | `60` | 目标帧率，`0` = 不限；写入配置包告知 Unity（`Application.targetFrameRate`） |
| `renderScale` | number | `1.0` | 渲染倍率 0.5 / 0.75 / 1.0。**不改变窗口尺寸**（窗口永远铺满显示器），通过配置包由 Unity 端 `ScalableBufferManager.ResizeBuffers` 落地；Windows 独立平台的动态分辨率需要 **DirectX 12**，D3D11 下无效（Unity 手册的限制） |
| `monitorIndex` | number | `0` | 挂到第几台显示器，`-1` = 全部显示器（整个虚拟桌面），`0` = 主显示器 |
| `extraArgs` | string | `""` | 追加的启动参数（空格分隔）；排在宿主自己拼的参数之后，可以覆盖它们 |
| `graphicsApi` | string | `"auto"` | `auto` / `d3d11` / `d3d12` → 壁纸进程的 `-force-d3d11` / `-force-d3d12`。**D3D12 下动态分辨率（`ScalableBufferManager`）才生效**，也就是 `renderScale` 只有选 D3D12 才有意义；改这一项会重启壁纸进程 |
| `pauseOnFullscreen` | bool | `true` | 全屏应用 / 演示模式时暂停壁纸 |
| `pauseOnBattery` | bool | `false` | 使用电池时暂停壁纸 |
| `pauseWhenForeground` | bool | `false` | 前台窗口最大化（非桌面）时暂停壁纸 |
| `inputForward` | bool | `false` | 是否把鼠标/键盘转发给壁纸窗口 |
| `inputLocked` | bool | `false` | 锁定：完全不转发（防误触） |
| `customParams` | array | 见下 | 透传给壁纸端的业务参数（宿主不解释语义） |
| `reportPort` | number | `47811` | 接收壁纸端「参数能力上报」的 UDP 端口，`0` = 关闭 |
| `autoStart` | bool | `false` | 开机自启 |
| `closeToTray` | bool | `true` | 点 × 收进托盘而不是退出 |
| `previewWidth` | number | `960` | 预览窗口宽（CSS 像素） |
| `previewHeight` | number | `540` | 预览窗口高 |

### 1.1 `CustomParam`（`customParams` 的元素）

一条下发给壁纸端的业务参数。宿主**只做透传**：存盘、随配置包每秒下发、
把壁纸端回传的能力列表（§2.8）显示在界面上，不解释任何语义。

```ts
{
  key: string;      // 小写字母/数字/下划线，长度 1–32
  value: string;    // 统一按字符串传；整数/布尔/枚举由壁纸端解析
  locked: boolean;  // 锁定 = 界面上不可改（值仍会照常下发）
}
```

读写规则（由 Rust 侧 `prefs.rs::normalize_params` 兜底）：

* 键名会统一成小写并剔除非法字符；空键丢弃，**同键去重**（后面的覆盖前面的）；
* 值里的换行/制表符压成空格并限长 256；
* **预置参数强制存在**：`beat` 永远补回来且**强制锁定**，`costume` 缺失时补 `"random"`。
  也就是说手改 `settings.json` 也关不掉节拍驱动 —— 这是"锁定"承诺的兜底。

| 键 | 默认值 | 默认锁定 | 语义（壁纸端实现） |
| --- | --- | --- | --- |
| `beat` | `"1"` | ✅ | 是否允许节拍驱动壁纸行为（`"0"` = 关） |
| `costume` | `"random"` | ⬜ | 米塔服装：`random` = 随机换一套，否则是服装存档名（`original` / `HellVamp`…） |

> 键名匹配**大小写不敏感**，所以手写成 `BEAT` 也能工作
> （宿主侧还会把它归一回官方拼写，好让界面查得到展示名）。

---

## 2. 数据类型

### 2.1 `WallpaperEntry`

```ts
{
  id: string;             // 目录名派生：slug + 短哈希，稳定不变
  name: string;           // 展示名（导入时的 zip 名 / 目录名 / 用户改名）
  dir: string;            // 绝对路径
  exe: string;            // 主程序绝对路径（空串 = 没找到 exe，broken）
  exeName: string;        // 文件名，如 "Miside.exe"
  sizeBytes: number;
  fileCount: number;
  importedAt: string;     // ISO8601
  unity: boolean;         // 识别为 Unity 程序（同级有 *_Data 目录 或 UnityPlayer.dll）
  unityVersion: string;   // 从 *_Data/globalgamemanagers 或 UnityPlayer.dll 猜，猜不到空串
  broken: boolean;        // 目录不存在 / 没有 exe
  missingExe: boolean;
}
```

### 2.2 `RuntimeState`

```ts
{
  mode: "stopped" | "preview" | "desktop";
  pid: number;            // 0 = 无进程
  hwnd: number;
  wallpaperId: string;
  exe: string;
  attached: boolean;      // 是否已挂到 WorkerW 桌面层
  paused: boolean;        // 实际处于挂起（进程被 suspend）
  userPaused: boolean;    // 用户手动暂停
  autoPaused: boolean;    // 因为暂停条件被自动挂起
  muted: boolean;
  volume: number;
  monitorIndex: number;
  startedAtMs: number;    // Unix 毫秒，0 = 未启动
  memoryMb: number;       // 0 = 未知
  occluded: boolean;      // 被全屏应用遮挡
  inputForwarding: boolean;
  lastError: string;
}
```

### 2.3 `AudioStatus`

```ts
{
  enabled: boolean;
  source: "auto" | "process" | "off";
  processName: string;
  pid: number;
  running: boolean;       // 采集会话是否在跑
  udpHost: string;
  udpPort: number;
  packetsSent: number;
  state: "idle" | "waiting" | "capturing" | "error";
  message: string;        // 给界面直接显示的文案（后端已本地化）
  dllOk: boolean;
  dllVersion: number;
}
```

### 2.4 `AudioTarget`（来自采集内核的目标枚举）

```ts
{
  pid: number;
  processName: string;
  title: string;
  hasWindow: boolean;
  windowVisible: boolean;
  sessionState: "none" | "active" | "inactive" | "expired";
  sessionPeak: number;
  mediaTitle: string;
  mediaArtist: string;
  mediaAlbum: string;
  mediaStatus: "unknown" | "playing" | "paused" | "stopped" | "closed" | "changing" | "opened";
}
```

### 2.5 `MonitorInfo`

```ts
{ index: number; name: string; x: number; y: number; width: number; height: number; primary: boolean; scaleFactor: number }
```

### 2.6 `AudioFramePayload`（事件 `wp://audio-frame`，默认 ≤30fps 给界面画图）

```ts
{
  pid: number;
  rms: number;        // 0–1
  peak: number;       // 0–1
  level: number;      // 0–1 平滑响度
  beat: number;       // 0–1，本帧是否检测到节拍（1 = 命中）
  bpm: number;        // 0 = 未估计出来
  bass: number;       // 0–1
  mid: number;        // 0–1
  treble: number;     // 0–1
  spectrum: number[]; // 128 个 0–255（对数频率柱，20Hz → Nyquist）
  waveform: number[]; // 256 个 -127–127，128 组**交错** (min, max)：偶索引 = min，奇索引 = max
}
```

> `waveform` 与推给 Unity 的帧包用的是同一份降采样结果（内核给 256 组，两两合并成 128 组）。

### 2.8 `ParamReportSnapshot`（命令 `param_report` / 事件 `wp://param-report`）

壁纸端上报的「参数能力」：某个参数有哪些可选值、当前生效的是哪个。
**可选项只有壁纸端知道**（它在运行时扫描自己的服装表），所以控制端不能硬编码枚举，
只能靠这份上报来渲染下拉框。

```ts
{
  client: string;         // 壁纸端标识（脚本版本号）；从未收到过时为空串
  ageMs: number | null;   // 距最近一次上报的毫秒数；null = 从未收到过
  params: Array<{
    key: string;          // 与 CustomParam.key 对应
    label: string;        // 人类可读的参数名；空则界面回落到 key
    value: string;        // 该参数当前在壁纸端生效的值
    writable: boolean;    // 壁纸端现在是否接受改这个参数
    options: Array<{ value: string; label: string }>;  // 空 = 自由填写
  }>;
}
```

> * 上报走 UDP（端口 = `reportPort`），**可能丢包**：界面用 `ageMs` 判断新鲜度，
>   超过几秒就该提示"壁纸端没在跑 / 这版壁纸不支持上报"。
> * 老版本壁纸端不上报时 `params` 为空数组，界面退回自由输入框 —— 功能不缺失，只是没有下拉。
> * 具体字段含义与发送时机见 [`protocol.md` §2.5](protocol.md)。

---

## 3. Tauri 命令（`invoke`）

Rust 命令名一律 snake_case，**参数名一律 camelCase**（Tauri 会自动转换）。

| 命令 | 参数 | 返回 |
| --- | --- | --- |
| `get_settings` | — | `Settings` |
| `save_settings` | `settings: Settings` | `Settings`（后端规范化后回传并广播） |
| `app_info` | — | `{ version, libraryDir, defaultLibraryDir, wallpaperCount, totalBytes, dllOk, dllVersion, dllPath, autostart }` |
| `list_monitors` | — | `MonitorInfo[]` |
| `library_list` | — | `WallpaperEntry[]` |
| `pick_zip_path` | — | `string \| null`（原生选文件对话框） |
| `pick_library_dir` | — | `string \| null`（原生选目录对话框） |
| `import_zip` | `zipPath: string`, `name?: string` | `WallpaperEntry` |
| `remove_wallpaper` | `id: string` | `WallpaperEntry[]` |
| `rename_wallpaper` | `id: string`, `name: string` | `WallpaperEntry` |
| `reveal_wallpaper` | `id: string` | `void`（资源管理器打开该目录） |
| `preview_wallpaper` | `id: string` | `RuntimeState`（普通窗口运行，不挂桌面） |
| `apply_wallpaper` | `id: string` | `RuntimeState`（挂到桌面 WorkerW） |
| `stop_wallpaper` | — | `RuntimeState` |
| `reload_wallpaper` | — | `RuntimeState` |
| `toggle_pause` | — | `RuntimeState` |
| `set_volume` | `volume: number` | `RuntimeState` |
| `set_muted` | `muted: boolean` | `RuntimeState` |
| `wallpaper_state` | — | `RuntimeState` |
| `wallpaper_thumbnail` | — | `ArrayBuffer`（裸二进制）：`[width u32 LE][height u32 LE][RGBA 像素，自上而下]`；**0 字节 = 拿不到预览**（没运行 / 已暂停 / 抓不到画面） |
| `list_audio_targets` | — | `AudioTarget[]` |
| `audio_status` | — | `AudioStatus` |
| `param_report` | — | `ParamReportSnapshot`（壁纸端上报的参数能力，见 2.8） |
| `set_audio_target` | `processName: string` | `AudioStatus`（空串 → 回到 `auto`） |
| `set_audio_enabled` | `enabled: boolean` | `AudioStatus` |
| `set_autostart` | `enabled: boolean` | `boolean` |
| `open_log_dir` | — | `void` |

> `wallpaper_thumbnail` 是主界面「当前壁纸预览」用的，走 `PrintWindow(PW_RENDERFULLCONTENT)` +
> `StretchBlt` 缩到 720px 宽以内。**壁纸进程被挂起时宿主不会去抓**（挂起的进程不响应
> `PrintWindow`，硬抓会把调用线程拖住），这种情况直接返回 0 字节。用 DirectX 独占渲染的窗口
> 经常只能抓到全黑帧，宿主也按「抓不到」处理，界面应该显示占位文案而不是一块黑屏。

## 4. 事件（`listen`）

| 事件名 | 负载 | 频率 |
| --- | --- | --- |
| `wp://state` | `RuntimeState` | 状态变化时 + 每 2s |
| `wp://library` | `WallpaperEntry[]` | 导入 / 删除 / 改名后 |
| `wp://import-progress` | `{ phase: "reading"\|"extracting"\|"finalizing"\|"done", percent: number, files: number, totalFiles: number, current: string, id: string }` | 导入期间（≤30/s 节流） |
| `wp://audio-frame` | `AudioFramePayload` | ≤30fps |
| `wp://audio-status` | `AudioStatus` | 每 1.2s + 变化时 |
| `wp://param-report` | `ParamReportSnapshot`（不含 `ageMs`） | 收到壁纸端上报时（约每 1–2s） |
| `wp://settings` | `Settings` | 保存后 |
| `wp://monitor` | `{ attached, occluded, fullscreen, onBattery, foreground, paused, pid, mode, monitorIndex }` | 每 2s |
| `wp://log` | `{ level: "info"\|"warn"\|"error", message: string, at: number }` | 有日志时 |

---

## 5. 推给 Unity 的协议（宿主 → Unity）

传输方式是**本地 UDP**（默认 `127.0.0.1:47810`，端口可配），这是唯一的通道。

### 5.1 UDP

目标 `127.0.0.1:47810`（端口可配）。全部**小端**。三个包类型：

**A. 帧包（type = 1）**，固定 440 字节：

| 偏移 | 类型 | 字段 |
| --- | --- | --- |
| 0 | char[4] | magic `"MWAV"` |
| 4 | u8 | version = 1 |
| 5 | u8 | type = 1 |
| 6 | u16 | reserved = 0 |
| 8 | u32 | seq（从 1 递增） |
| 12 | u64 | timestampMs（宿主单调时钟毫秒） |
| 20 | u32 | sampleRate（48000） |
| 24 | f32 | rms |
| 28 | f32 | peak |
| 32 | f32 | level（平滑响度） |
| 36 | f32 | beat（0 或 1，本帧命中节拍） |
| 40 | f32 | bass（0–1） |
| 44 | f32 | mid（0–1） |
| 48 | f32 | treble（0–1） |
| 52 | f32 | bpm（0 = 未知） |
| 56 | u8[128] | spectrum：128 根对数频率柱，0–255 |
| 184 | i8[256] | waveform：128 组 **交错** `(min, max)`（偶偏移 = min，奇偏移 = max），-127–127 |
| 440 | — | 结束（总长 **440**） |

**B. 配置包（type = 2）**：`magic(4) + version(1) + type(1) + reserved(2) + seq(4) + jsonLen(u32) + utf8 JSON`
JSON 形如：

```json
{
  "type": "config",
  "targetFps": 60,
  "renderScale": 1.0,
  "muted": false,
  "volume": 1.0,
  "paused": false,
  "language": "zh-CN",
  "audioGain": 1.0,
  "params": [
    { "key": "beat", "value": "1", "locked": true },
    { "key": "costume", "value": "random", "locked": false }
  ],
  "reportPort": 47811,
  "screen": { "x": 0, "y": 0, "width": 1920, "height": 1080 },
  "source": { "pid": 32440, "process": "cloudmusic.exe" }
}
```

> `params` 是**透传**的自定义参数（含义由壁纸端决定），`reportPort` 告诉壁纸端
> 往哪个端口回报参数能力。两者细节见 [`protocol.md` §2.4 / §2.5](protocol.md)
> 与本文档 §1.1 / §2.8。

> `renderScale`（0.5 / 0.75 / 1.0）落地方式是 Unity 端的 `ScalableBufferManager.ResizeBuffers(scale, scale)`：
> 壁纸窗口永远铺满整块显示器，降的是内部分辨率。宿主**不能**靠缩小窗口来实现倍率 ——
> 那只会让壁纸缩在屏幕一角。`targetFps` 同理，由 Unity 端 `Application.targetFrameRate` 落地。
> 随程序提供的 `unity/MisideAudioLink.cs` 已经处理了这两项（`applyTargetFramerate` / `applyRenderScale`）。

连接建立后立刻发一次，之后每 1 秒一次，设置变化时立刻补一发。

**C. 输入包（type = 3）**：`magic(4) + version(1) + type(1) + reserved(2) + seq(4) + jsonLen(u32) + utf8 JSON`

```json
{
  "type": "input",
  "pointer": { "x": 640.0, "y": 360.0, "inside": true, "left": false, "right": false, "middle": false, "wheel": 0 },
  "client": { "width": 1920, "height": 1080 },
  "locked": false
}
```

只有在"输入转发"打开且桌面处于前台时才有内容，频率 ≤60Hz。
`x/y` 是**壁纸窗口客户区坐标**（左上角原点，单位像素，已按窗口尺寸缩放）。

**D. 上报包（type = 4，方向相反：Unity → 宿主）**：包头与配置包相同。

壁纸端把「参数能力」（可用服装列表、当前值）发到配置包里的 `reportPort`。
这是唯一一条反向通道，存在的理由是：`costume` 有哪些可选值只有壁纸端知道。

```json
{
  "type": "report",
  "client": "wallpaper-hub-2.0",
  "params": [
    { "key": "costume", "label": "米塔服装", "value": "HellVamp", "writable": true,
      "options": [ { "value": "random", "label": "随机换一套" } ] }
  ]
}
```

> 宿主这侧的接收是**容错**的：magic / 版本 / 类型不符、`jsonLen` 越界、JSON 坏掉
> 都只是丢包（该端口上可能有别的程序在发包）。完整字段表见
> [`protocol.md` §2.5](protocol.md)。

---

## 6. 文件与目录约定

```
<app_config_dir>/settings.json          设置
<app_config_dir>/wallpapers.json        壁纸库索引（id / name / 导入时间 / exe 相对路径）
<libraryDir>/<id>/                      单个壁纸解压后的全部文件
<app_log_dir>/host.log                  运行日志（滚动，≤2MB）
```

* `<app_config_dir>` = `%APPDATA%\com.toumingqaq.miside-wallpaper-engine`
* `<app_log_dir>` = `%LOCALAPPDATA%\com.toumingqaq.miside-wallpaper-engine\logs`
* `libraryDir` 留空时 = `<app_config_dir>\wallpapers`；用户也可以在设置里指到程序目录或别的盘。
* 解压必须防 **zip-slip**（`..` / 绝对路径 / 符号链接一律拒绝）并保留目录结构。
* 中文 zip 的 GBK 文件名要在解压时正确还原（UTF-8 优先，失败回退 GBK）。
