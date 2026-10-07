# miside-wallpaper-engine 宿主 → Unity 协议（第 5 节完整版）

本文是 [`api-contract.md`](api-contract.md) 第 5 节的展开版本，作为**接收端唯一的实现依据**。
宿主的 Rust 实现（`src-tauri/src/audio_link.rs`）与 Unity 端的 `unity/MisideAudioLink.cs` 都以本文为准。

* 方向：**宿主 → Unity**（单向，Unity 不需要回包）
* 通道：本地 **UDP**（默认 `127.0.0.1:47810`），这是**唯一**的传输通道
* 字节序：**一律小端（little-endian）**
* 协议版本：**1**（`version = 1`；版本不符的包必须丢弃）

---

## 1. 通用约定

### 1.1 包头

| 包类型 | type | 包头长度 | 结构 |
| --- | --- | --- | --- |
| 帧包 | 1 | 8 字节 | `magic(4) + version(1) + type(1) + reserved(2)` |
| 配置包 | 2 | 16 字节 | `magic(4) + version(1) + type(1) + reserved(2) + seq(4) + jsonLen(4)` |
| 输入包 | 3 | 16 字节 | 同配置包 |

* `magic` = ASCII `"MWAV"` = `4D 57 41 56`
* `version` = `1`
* `reserved` = `0`（忽略，不要用它判断任何东西）
* 接收端必须先校验 `长度 ≥ 包头` → `magic` → `version` → `type`，任何一步不符合就**整包丢弃并计数**，不要尝试"尽力解析"。

### 1.2 数值约定

| 字段族 | 取值范围 | 备注 |
| --- | --- | --- |
| `rms` / `peak` / `level` / `bass` / `mid` / `treble` / `beat` | `0.0 – 1.0` | 已经乘过宿主设置里的 `audioGain` |
| `bpm` | `0 – 400` | `0` = 还没估计出来，**必须当成"未知"而不是"0 BPM"处理** |
| `spectrum[128]` | `0 – 255`（`u8`） | 对数频率轴，`[0]` ≈ 20Hz，`[127]` ≈ Nyquist |
| `waveform[256]` | `-127 – 127`（`i8`） | 每 2 字节一组 `(min, max)`，共 128 组 |
| `timestampMs` | `u64` | **宿主自己的单调时钟**，只能用于算包间隔，不能和 `DateTime.Now` 比 |
| `seq` | `u32` | 从 1 开始递增，用于判重/判丢包；会回绕 |

---

## 2. UDP

### 2.1 帧包（`type = 1`，**总长 440 字节**）

| 偏移 | 长度 | 类型 | 字段 | 说明 |
| --- | --- | --- | --- | --- |
| 0 | 4 | `char[4]` | `magic` | `"MWAV"` |
| 4 | 1 | `u8` | `version` | `1` |
| 5 | 1 | `u8` | `type` | `1` |
| 6 | 2 | `u16` | `reserved` | `0` |
| 8 | 4 | `u32` | `seq` | 从 1 递增 |
| 12 | 8 | `u64` | `timestampMs` | 宿主单调毫秒 |
| 20 | 4 | `u32` | `sampleRate` | 通常 `48000` |
| 24 | 4 | `f32` | `rms` | 0–1 |
| 28 | 4 | `f32` | `peak` | 0–1 |
| 32 | 4 | `f32` | `level` | 0–1 平滑响度 |
| 36 | 4 | `f32` | `beat` | `0` 或 `1`，本帧是否命中节拍 |
| 40 | 4 | `f32` | `bass` | 0–1 |
| 44 | 4 | `f32` | `mid` | 0–1 |
| 48 | 4 | `f32` | `treble` | 0–1 |
| 52 | 4 | `f32` | `bpm` | `0` = 未知 |
| 56 | 128 | `u8[128]` | `spectrum` | 0–255 |
| 184 | 256 | `i8[256]` | `waveform` | 每 2 字节 `(min, max)`，偶 = min、奇 = max |
| 440 | — | — | （结束） | 总长 = 56 + 128 + 256 = **440** |

> **契约歧义（已按 440 处理）**：`api-contract.md` 的表头写「固定 434 字节」，但同一节的字段偏移表与
> "实际总长 440 字节"的说明都指向 **440**（`56 + 128 + 256`），且 434 无法容纳整个波形区。
> 本实现与 `unity/MisideAudioLink.cs` 一律按 **440** 处理：`长度 < 440` 视为坏包丢弃，
> `长度 > 440` 视为向前兼容的多余尾部（忽略多余字节）。

### 2.2 配置包（`type = 2`）

```
偏移 0  magic(4) | 4 version(1) | 5 type=2(1) | 6 reserved(2) | 8 seq(4) | 12 jsonLen(4) | 16 utf8 JSON
```

`jsonLen` 是 **JSON 字节数**（不含包头，不含结尾 `\0`）。发送频率：连接建立后立刻一次，之后**每 1 秒一次**，设置变化时立刻补一发。

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

| 字段 | 类型 | 含义 |
| --- | --- | --- |
| `targetFps` | int | 目标帧率，`0` = 不限（建议接收端写进 `Application.targetFrameRate`） |
| `renderScale` | number | **渲染倍率**，合法范围 **0.25–1.0**（宿主 UI 上只有 0.5 / 0.75 / 1.0）。**只降 Unity 的内部分辨率，窗口照旧铺满整块显示器**；落地方式见下面 §2.2.1 |
| `muted` | bool | 宿主是否把壁纸进程静音（宿主走 WASAPI 会话音量，Unity 内部音源不受它影响） |
| `volume` | number | 宿主设置的壁纸音量 0–1 |
| `paused` | bool | 宿主是否暂停壁纸（用户手动暂停 / 自动暂停条件命中） |
| `language` | string | `zh-CN` / `en-US` |
| `audioGain` | number | 采集增益 0.1–4（已作用在数值上，这里只是告知） |
| `params` | array | **自定义参数**，见 §2.4；缺字段 = 保持上一次的值 |
| `reportPort` | number | 宿主接收**参数上报**的 UDP 端口（§2.5），`0` = 上报通道关闭 |
| `screen` | object | 壁纸窗口的屏幕位置与尺寸（像素） |
| `source` | object | 当前采集的进程 `pid` 与 `process`（小写进程名，可能为空） |

> 字段可能缺失（例如还没选到音源时 `source` 为空对象）。**缺字段时保留上一次的值**，不要清零。
> 唯一的例外是 `renderScale`：它缺字段时按 **1.0**（不降分辨率）处理，理由见 §2.2.1。

#### 2.2.1 `renderScale` 的语义

* 它描述的是**内部分辨率**，不是窗口大小：壁纸窗口永远铺满整块显示器，
  宿主**不会**再靠 `-screen-width/-screen-height` 去缩小窗口
  （那样只会让壁纸缩在屏幕左上角）。宿主只负责下发数值，**落地完全在 Unity 端**。
* 合法范围 **0.25–1.0**，宿主 UI 上只有 `0.5` / `0.75` / `1.0` 三档，每 1 秒随配置包重发，设置变了立刻补发。
* 接收端的处理规则（`unity/MisideAudioLink.cs` 即按此实现）：
  * 缺字段 → `1.0`（老版本宿主没这个概念，就当不降分辨率）；
  * `NaN` / `±Inf` / `<= 0` → `1.0`（垃圾值，绝不把 0 传下去）；
  * `0 < v < 0.25` → 钳到 `0.25`；
  * `v > 1.0` → 钳到 `1.0`。
* Windows 独立平台的落地手段是 `ScalableBufferManager.ResizeBuffers(scale, scale)`
  （配合 `Camera.allowDynamicResolution = true`），**它只在 DirectX 12 下生效**
  （[Unity 手册：动态分辨率](https://docs.unity3d.com/cn/2021.2/Manual/DynamicResolution.html)）。
  用 DX11 的工程要么切到 D3D12，要么改用渲染管线自带的 Render Scale，
  要么把这个特性交给壁纸工程自己处理。
* 注意 `renderScale` **不属于**"接收端可以随便忽略的字段"：宿主 UI 上用户改了倍率，
  期望立刻看到 GPU 占用下降。接收端如果因为渲染管线限制做不到，应当在文档/日志里说清楚，
  而不是静默忽略。

### 2.3 输入包（`type = 3`）

包头与配置包完全相同，`jsonLen` 后跟：

```json
{
  "type": "input",
  "pointer": { "x": 640.0, "y": 360.0, "inside": true, "left": false, "right": false, "middle": false, "wheel": 0 },
  "client": { "width": 1920, "height": 1080 },
  "locked": false
}
```

| 字段 | 类型 | 含义 |
| --- | --- | --- |
| `pointer.x` / `pointer.y` | number | **壁纸窗口客户区坐标**，左上角为原点，单位像素，已按窗口尺寸缩放 |
| `pointer.inside` | bool | 指针是否在客户区内 |
| `pointer.left` / `right` / `middle` | bool | 对应按键是否处于按下状态 |
| `pointer.wheel` | number | 滚轮增量（`+120` / `-120` 的倍数） |
| `client.width` / `height` | number | 客户区尺寸，用来自行归一化 |
| `locked` | bool | 宿主"锁定输入"；为 `true` 时请忽略 `pointer` |

* 只有**输入转发打开**且桌面处于前台时才会发；频率 ≤ 60Hz。
* **不发包 ≠ 指针在 (0,0)**。接收端应该给输入状态加超时（建议 250ms 左右），超时后把 `inside`/按键都当作 `false`。

### 2.4 自定义参数（配置包里的 `params`）

宿主**不解释**这些参数的语义：它只负责存盘、随配置包每秒下发、以及把壁纸端回传的
能力列表（§2.5）显示在界面上。含义完全由壁纸端决定。

```json
"params": [
  { "key": "beat",    "value": "1",      "locked": true  },
  { "key": "costume", "value": "random", "locked": false }
]
```

| 字段 | 类型 | 含义 |
| --- | --- | --- |
| `key` | string | 参数键，小写字母 / 数字 / 下划线，长度 1–32 |
| `value` | string | 参数值。**统一是字符串**：整数、布尔、枚举都由壁纸端自己解析 |
| `locked` | bool | 控制端是否锁定该参数（锁定 = 界面上不可改，`value` 仍会照常下发） |

宿主侧的固定约定（`prefs.rs` 的 `normalize_params` 兜底，手改 `settings.json` 也绕不过）：

| 键 | 默认值 | 默认锁定 | 语义（由壁纸端实现） |
| --- | --- | --- | --- |
| `beat` | `"1"` | ✅ **锁定** | 是否允许节拍驱动壁纸行为（`"0"` = 关）。锁定是为了防止误关掉整条节奏链路 |
| `costume` | `"random"` | ⬜ 不锁定 | 米塔服装：`random` = 随机换一套，否则是服装的存档名（如 `original` / `HellVamp`） |

> **键名匹配统一大小写不敏感**，所以手写成 `BEAT` 也能工作
> （宿主侧还会把它归一回官方拼写，好让界面查得到展示名）。

> * **缺 `params` 字段时保留上一次的值**（与配置包其它字段一致），不要清空。
> * 值可能不是合法数字（用户自由填写）：接收端做类型转换时**必须给兜底值**，
>   不能因为一个 `"abc"` 就把玩法打挂。
> * 接收端读到不认识的键时忽略即可 —— 这是给扩展留的口子，不需要报错。

### 2.5 参数上报包（`type = 4`，**Unity → 宿主**）

方向与其它包**相反**：壁纸端是唯一知道「`costume` 到底有哪些可选服装」的一方
（它在运行时扫描 `Resources/Clothes`），所以由它把能力列表报回去，
控制端界面才能给出可选项下拉。

包头与配置包完全一致，`jsonLen` 后跟：

```json
{
  "type": "report",
  "client": "wallpaper-hub-2.0",
  "params": [
    {
      "key": "costume",
      "label": "米塔服装",
      "value": "HellVamp",
      "writable": true,
      "options": [
        { "value": "random",   "label": "随机换一套" },
        { "value": "original", "label": "original"   },
        { "value": "HellVamp", "label": "HellVamp"   }
      ]
    }
  ]
}
```

| 字段 | 类型 | 含义 |
| --- | --- | --- |
| `client` | string | 壁纸端标识（脚本版本号），便于排查"对面是哪一版" |
| `params[].key` | string | 参数键，与配置包里的 `key` 对应 |
| `params[].label` | string | 人类可读的参数名；为空时界面回落到 `key` |
| `params[].value` | string | 该参数**当前在壁纸端生效**的值 |
| `params[].writable` | bool | 壁纸端现在是否接受改这个参数（缺字段按 `true`） |
| `params[].options` | array | 可选值列表；**空 = 自由填写**（界面退回输入框） |
| `params[].options[].value` / `label` | string | 可选项的值 / 展示名。可选项也允许写成裸字符串 `"HellVamp"`（此时 `label = value`） |

* 目标端口取自配置包的 `reportPort`；该值为 `0` 时**不要发**。
* 建议**首次注册完能力立刻发一次**，之后每 1–2 秒重发（控制端靠它判断壁纸是否活着）。
* 走的是 UDP，**可能丢包**；宿主端的解析是容错的 —— magic / 版本 / 类型不符、
  `jsonLen` 越界、JSON 坏掉都只是丢掉这一包（这个端口上可能有别的程序在发包）。
* 宿主收到后会广播 `wp://param-report` 事件，并提供 `param_report` 命令供界面拉取。

---

## 3. 丢包 / 乱序 / 抖动的处理建议

1. **校验优先**：magic / version / type / 长度 任一不符 → 整包丢弃并计数。计数器暴涨通常意味着
   端口被别的程序占用（全是别的程序的数据报），或者双方的 `version` 对不上。
2. **判重与判序**：UDP 会乱序、会重复。用 `seq` 过滤：
   ```csharp
   // u32 回绕安全的"seq 是否更新"判断
   bool isNewer = (int)(seq - lastSeq) > 0;
   ```
   迟到的旧包直接丢，不要用它覆盖更新的数据。
3. **只保留最新帧**：同一帧（两次渲染之间）收到多个帧包时，**只采用最后一个**。
   接收线程轮转写入槽、主线程只读"已发布"的那个槽，即可天然做到（见 `MisideAudioLink` 的 `RxSlot`）。
4. **节拍不能靠"最新帧"**：如果一帧内收到 3 个 `beat = 1` 的包，只保留最新会丢掉 2 个节拍事件。
   建议接收端**latch**（累加计数 + 取最大强度），在主线程每帧最多回调一次。
5. **抖动要平滑，但别加延迟**：对 `rms/level/bass/.../spectrum` 做**指数平滑**，系数用半衰期换算，
   保证与帧率无关：
   ```csharp
   float k = 1f - Mathf.Exp(-0.6931472f * Time.unscaledDeltaTime / halfLife);
   value = Mathf.Lerp(value, target, k);
   ```
   半衰期 0.03–0.10 秒比较合适。**不要**平滑 `beat`。
6. **断流检测**：用 `Environment.TickCount` 之类的单调时钟记录最后收包时刻，
   超过 `staleTimeoutMs`（默认 1000ms）就认为断流，画面保持最后一帧而不要瞬间归零。
7. **BPM 为 0 时**：视为"未知"，保持上一次的转速或退化成缓慢自转，不要让旋转突然停死。
8. **输入包超时**：250ms 没有新的输入包就把指针状态清空（`inside = false`、按键全 `false`、滚轮归 0）。

---

## 4. 字段 → 常见用法对照表

| 字段 | 推荐用法 | 备注 |
| --- | --- | --- |
| `level` | 整体缩放 / 整体亮度 / 后处理强度 / 泛光强度 | 最稳的"总强度"，首选它做全局效果 |
| `rms` | 平均音量条、音量表 | 比 `level` 更"跳"，适合做仪表 |
| `peak` | 峰值指示、削波提示 | 变化最剧烈 |
| `bass` | 低音鼓触发的位移/抖动、低频粒子爆发、地面起伏 | 和 `beat` 搭配：`bass` 做连续量，`beat` 做离散脉冲 |
| `mid` | 主体色相/形状变化 | |
| `treble` | 高频闪光、噪点、粒子闪烁、描边 | |
| `beat` | **一次性脉冲**（命中帧才有值） | 建议 latch 后每帧最多触发一次 |
| `bpm` | 匀速旋转速度、循环动画速度、节拍同步 | `拍长 = 60 / bpm` 秒；`0` 表示未知 |
| `spectrum[128]` | 频谱条、地形高度、顶点位移、灯光阵列 | 只需要 N 根柱子时用 `GetBandAt(i/(N-1))` 或按区间取最大值 |
| `waveform` `(min,max)` | 示波器、线条粗细、波形带、音频响应曲线 | 偶字节 = min，奇字节 = max；两者都归一化到 -1–1 |
| `targetFps` | 写进 `Application.targetFrameRate` | 建议只在值变化时写 |
| `renderScale` | 写进 `ScalableBufferManager.ResizeBuffers(scale, scale)`，同时把 `Camera.allowDynamicResolution` 打开 | 只降内部分辨率，窗口照旧铺满；Windows 上需 DirectX 12。详见 §2.2.1 |
| `muted` / `volume` | 自行控制 Unity 内部音源；做 UI 提示 | 宿主侧的静音走 WASAPI，不影响 Unity 内部音频 |
| `paused` | 暂停动画 / 降低更新频率 / 停止粒子 | 默认**不要**改 `Time.timeScale`（会连带影响用户逻辑） |
| `language` | 切换 UI 文案 | `zh-CN` / `en-US` |
| `screen` | 按分辨率适配、算宽高比、摆放 UI | 这是**壁纸窗口本身**的位置与尺寸（铺满显示器），和 `renderScale` 的内部分辨率无关 |
| `pointer.*` | 鼠标跟随、点击交互、滚轮缩放 | 与 Unity 输入系统无关，宿主直接推给你 |
| `locked` | 输入被锁定 → 完全忽略 `pointer` | |
| `source` | 自检：确认宿主采的是哪个进程 | 排查"没数据"最有用 |

### 4.1 `beat` / `bpm` 的归属：宿主推数据，壁纸端判节拍

**宿主只负责采集音频并推送数据**；`beat` / `bpm` 这两个字段虽然仍在帧包里
（旧版壁纸端、以及想把它们当诊断量的场景还在用），但**壁纸端不应再依赖它们做玩法决策**。

MiSide 壁纸的实际做法：在壁纸端跑自己的检测器
（`Assets/MisideWallpaper/Script/Audio/WallpaperBeatDetector.cs`），
判定用的是**原始端 `UnityAudioBeat` 那两条**（详见 §4.2）：

```
energy = 均方(smoothSpectrum)                          // = 原始频谱 ×1000 再平滑
beat   = energy > 0.0125 && (上一帧 energy - energy) > 0
```

这么做的好处：

* **玩法归属正确** —— "哪一帧算节拍"是玩法决策，放在能同时看到动画状态的一侧；
* **没有额外延迟** —— 频谱与节拍本来就在同一个包里到达，本地算不慢于读字段；
* **观感可回归** —— 判定逻辑在壁纸工程里，改它不用动宿主、也不用重发 Rust 侧。

### 4.2 为什么是「能量 + 下降沿」而不是「检测鼓点」

原始端的 `UnityAudioBeat.SetSample` 只有：

```csharp
energy    = samples 的均方能量;
disEnergy = currentEnergy - energy;                  // 本帧比上一帧**低**时为正
beate     = currentEnergy > 0.0125f && disEnergy > 0;
```

它**不是检测鼓点**，而是「音乐在响 + 这一帧能量在回落」。对任何连续信号，
相邻两帧大约一半在上升、一半在下降，所以这个条件在音乐播放期间
**结构性地约一半帧成立**（实测约 50%，见 `.verify-unity/beat-check/`）。

结果是给 `MitaDance.beatPulse` 一个近乎恒定的 `+0.12` 抬升（衰减根本来不及走低），
而不是离散脉冲 —— 这正是原始端"点头明显"的原因。

> 曾经这里用的是 Patin 动态阈值法（宿主 Rust 侧 `beat.rs` 那套：按 1.2 秒窗口
> 算均值与方差、据此推阈值）。它只在真实 onset 上触发（约 2 次/秒），
> 幅度明显小于原始端，所以才换回上面这两条。
> `beat.rs` 仍在宿主侧运行并下发 `beat` / `bpm` 字段，但已不参与壁纸的玩法判定。

> 实现要点：判定输入必须是**平滑放大后**的那份频谱（对应原始端的 `smoothBuffer`），
> 因为 `nodMinEnergy = 0.0125` 这个地板是对着那个量级标定的；
> 换成未放大的原始频谱会让判定整体失效。

---

## 5. 版本与兼容

* 当前 `version = 1`。**收到 `version != 1` 的包必须丢弃**，并建议在日志里明确提示
  "协议版本不符"，因为这说明宿主和 Unity 端不是同一版本。
* 帧包尾部如有扩展，接收端应容忍"长度大于已知长度"并忽略多余字节。
* 配置包/输入包的 JSON 里**新增字段是允许的**，接收端必须忽略不认识的字段（`MisideAudioLink`
  的手写扫描器天然如此）。**删字段/改语义属于破坏性变更**，需要同时升 `version`。

## 6. 已知歧义与处理方式

| 歧义 | 处理方式 |
| --- | --- |
| 帧包"固定 434 字节" vs 字段表算出的 440 | 按 **440** 处理（`< 440` 丢弃），理由见 §2.1 |
| `waveform` 每组的字节顺序 | 按契约字面：**偶数字节 = min，奇数字节 = max**。示例代码不依赖顺序。若宿主实际相反，波形带会上下翻转但不会崩 |
| `pointer.wheel` 是"增量"还是"累计值" | 按**增量**处理：两次渲染之间收到的输入包的 `wheel` 相加作为本帧滚轮量 |
| `paused` 从哪来 | `Paused = 配置包.paused`（UDP 配置包每秒下发一次；配置包还没到过时视为未暂停） |
| `PointerNormalized` 的 Y 方向 | 协议口径是左上角原点（Y 向下）。`MisideAudioLink` 里 `PointerPosition`/`PointerNormalized` 保持协议口径不变，另提供已翻转成 Unity 惯例（左下角原点）的 `PointerViewport` |
| `bpm = 0` | 视为"未知"，不要当成 0 BPM 计算转速 |
| `beat` 是 `f32` 而不是 `u8` | 判定阈值取 `>= 0.5f` |
| `renderScale` 到底谁来落地 | **宿主只下发数值，落地完全在 Unity 端**（`ScalableBufferManager.ResizeBuffers` + `Camera.allowDynamicResolution`）。宿主不能靠缩小窗口实现倍率，那只会让壁纸缩在屏幕一角。接收端缺字段/非法值一律回退 1.0，并用 `applyRenderScale` 开关把控制权留给壁纸工程 |
| `renderScale` 在 DX11 上无效 | 这是平台限制（[Unity 手册](https://docs.unity3d.com/cn/2021.2/Manual/DynamicResolution.html)：Windows 独立平台的动态分辨率仅 DirectX 12）。接收端应给出一次性警告，而不是假装生效 |
