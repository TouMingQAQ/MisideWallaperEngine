# miside-wallpaper-engine

**把一个 Unity 打包出来的 exe，变成会跟着音乐律动的桌面壁纸。**

Windows 桌面工具（Tauri 2 + Rust + TypeScript），版本 `0.0.1`：

1. **导入**：选一个壁纸压缩包（zip），程序自己解压到壁纸库并管理（列表 / 预览 / 重命名 / 删除）；
2. **上桌面**：启动里面的 Unity exe，把它的窗口挂到桌面 `WorkerW` 图层 —— 就在桌面图标下面、真壁纸上面，
   任务栏和 Alt-Tab 里都不会多出条目；
3. **控制它**：类 Wallpaper Engine 的一套设置 —— 音量 / 静音 / 暂停 / 重载、目标帧率、渲染倍率、指定显示器、
   全屏或电池时自动暂停、开机自启、鼠标键盘输入转发（可锁定防误触）；
4. **让它动起来**：从系统里任意一个正在出声的进程（音乐播放器 / 浏览器 / 游戏）采集音频，
   把**频谱 / 响度 / 低频能量 / 节拍 / BPM** 经本地 UDP 推给 Unity，
   Unity 端只要丢一个 C# 脚本进来就能用（见 [Unity 端接入](docs/unity-integration.md)）。

> 音频采集内核（`ProcessAudioCapture.dll`，按进程抓 WASAPI 回环）与宿主侧的采集/DSP 桥接代码
> 拷贝自 [`TouMingQAQ/ProcessAudioCaptureUI`](https://github.com/TouMingQAQ/ProcessAudioCaptureUI)，
> 本工程在其之上做壁纸宿主（本工程不会改动那个仓库）。

| 主界面（仪表盘：可视化 / 监听进程 / 壁纸预览） | 设置弹窗 · 壁纸库 |
| --- | --- |
| ![主界面](docs/screenshots/main.png) | ![壁纸库](docs/screenshots/settings-library.png) |

| 设置弹窗 · 通用 | 设置弹窗 · 关于 / 状态 |
| --- | --- |
| ![通用](docs/screenshots/settings-general.png) | ![关于](docs/screenshots/settings-about.png) |

---

## 快速开始

```powershell
npm install
npm run app:dev      # 开发模式（前端 HMR + Rust 热重启）
npm run app:build    # 打包 NSIS 安装包
npm run check        # tsc --noEmit + cargo check
npm run test:rust    # Rust 侧单测
```

打包产物：

```
src-tauri/target/release/miside-wallpaper-engine.exe       主程序
src-tauri/target/release/binaries/ProcessAudioCapture.dll    采集内核（随程序分发）
src-tauri/target/release/bundle/nsis/*.exe                   NSIS 安装包
```

把 exe 与同目录的 `binaries\ProcessAudioCapture.dll` 一起拷走即可免安装运行。

### 界面：一个仪表盘 + 一个设置弹窗

主界面不做分页，打开就是三块东西：

* **音频节奏可视化**（左，占大头）：128 柱对数频谱 + 256 点峰谷波形 + RMS/峰值/低中高三段电平 + BPM 与节拍闪烁；
* **当前监听进程**（左下）：默认「自动跟随最响的进程」，也可以直接指定某个程序；旁边是采集状态与音频总开关；
* **当前壁纸预览**（右）：实时缩略图 + 壁纸名与 Unity 版本 + 预览 / 应用到桌面 / 停止 / 重载 / 暂停。

**壁纸库、音频设置、通用设置、关于 / 日志** 全部收在一个设置弹窗里（右上角 ⚙，弹窗内分 tab），
主界面不再有左侧导航。

### 使用流程

1. 把壁纸 zip 拖进主窗口（或点设置 →「壁纸库」→ 导入压缩包）→ 程序解压到壁纸库目录，
   自动认出主 exe 与 Unity 版本；
2. 右侧预览卡片上点「预览」（普通窗口运行，不动桌面）先看效果；
3. 满意了点「应用到桌面」→ 窗口被挂到壁纸层并铺满屏幕，右侧缩略图会实时反映壁纸画面；
4. 左下选音频源（默认自动跟随当前最响的进程），Unity 壁纸立刻开始跟着音乐动；
5. 关上主界面 = 收进托盘，壁纸与音频采集继续跑；要彻底退出用托盘右键菜单的「退出」。

> 实时缩略图走 `PrintWindow`，用 DirectX 独占渲染的壁纸可能抓不到画面（返回空），
> 这时卡片会提示「无法实时预览」，点「预览」用独立窗口看即可。

### 壁纸进程归属于宿主

壁纸 exe 由宿主 `Command::spawn` 直接拉起 —— 是**真正的子进程**，我们握着它的句柄，
可以停、可以查、能量它占多少内存。但"父子进程"本身**并不保证**父进程被强杀时子进程会跟着走，
所以额外加了一层：把壁纸放进一个设了 **`JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`** 的 **Job Object**。
这样宿主无论以什么方式结束（正常退出 / 托盘退出 / 任务管理器强杀 / 崩溃），内核都会把壁纸一并结束 ——
不会在桌面上留下一层点「停止」也摘不掉的壁纸。

再加一道兜底：每次起流都把 `PID + exe 路径` 落盘到 `running.json`。万一 Job 分配失败
（很老的系统不允许嵌套 Job），或遇到更早版本留下的孤儿，下次启动会**核对这个 PID 现在的可执行
文件是不是当初那个 exe**（PID 会被复用，不能只看号），确认后才摘窗口 + 结束进程。

---

## 设置项

| 分组 | 设置 | 说明 |
| --- | --- | --- |
| 外观 | 主题 / 语言 | 深色、浅色、跟随系统；中文 / English |
| 壁纸进程 | 音量 / 静音 | 走 WASAPI **会话音量**：只影响壁纸进程，不动系统音量 |
| | 目标帧率 | `0` = 不限。通过配置包告知 Unity，由它 `Application.targetFrameRate` |
| | 渲染倍率 | 0.5 / 0.75 / 1.0：窗口照旧铺满整块屏，降的是 Unity 的内部分辨率（配置包 → `ScalableBufferManager`）。**Windows 独立平台的动态分辨率只支持 DirectX 12**，所以要配合下面的图形 API 选 D3D12；选了其它 API 时宿主会明确提示这项不生效 |
| | 图形 API | 自动 / D3D11 / D3D12 → 壁纸进程的 `-force-d3d11` / `-force-d3d12`。默认「自动」交给 Unity 自己挑（实测壁纸工程默认是 D3D11）；想要渲染倍率省显存就选 D3D12，D3D12 下壁纸异常再退回 D3D11。**改这一项会自动重启壁纸进程** |
| | 显示器 | 全部显示器（整个虚拟桌面）或指定某一块（默认只铺主显示器） |
| | 额外启动参数 | 原样拼在 Unity 启动命令行后面 |
| | 暂停 / 重载 | 暂停 = 挂起进程（`NtSuspendProcess`），恢复即继续；重载 = 重启进程 |
| 暂停条件 | 全屏应用时 / 电池供电时 / 前台窗口最大化时 | 命中条件自动挂起壁纸，条件消失自动恢复。全屏判定用 `SHQueryUserNotificationState`，但**不把 `QUNS_BUSY` 单独当全屏**（切换窗口、Alt+Tab、开开始菜单时外壳会短暂报忙），还要求前台真有一个盖满显示器的窗口；另外条件必须**连续满足 3 秒**才生效 —— 否则会出现"切个窗口壁纸就停一下"。每次自动暂停/恢复都会写日志并带上原因 |
| 输入 | 转发鼠标键盘 | 桌面在前台时，鼠标事件转给壁纸窗口；键盘靠把真实焦点交给它 |
| | 锁定 | 完全不转发：壁纸只显示、不响应，点桌面不会误触它 |
| 系统 | 开机自启 | 写 `HKCU\...\CurrentVersion\Run`，以设置文件为准自动纠正 |
| | 关闭到托盘 | 点 × 只收进托盘 |
| | 壁纸库目录 | 默认 `%APPDATA%\com.toumingqaq.miside-wallpaper-engine\wallpapers`，可指到程序目录或别的盘 |
| | 音频 | 见下 |
| 音频 | 开关 / 音频源 | 自动跟随最响的进程，或指定某个进程（列表每 1.2 秒刷新，标出谁在出声） |
| | UDP 地址端口 | 音频数据只经本地 UDP 推送，默认 `127.0.0.1:47810` |
| | 增益 / 推流帧率 | 增益放大频段与电平；帧率 `0` = 不限 |

---

## 音频节奏 → Unity

宿主每个推流周期发三类包（协议全文见 [docs/protocol.md](docs/protocol.md)）：

| 包 | 内容 |
| --- | --- |
| **帧包**（440 字节定长） | `rms` / `peak` / 平滑 `level` / `beat` / `bpm` / `bass` `mid` `treble` + 128 柱对数频谱 + 128 组峰谷波形 |
| **配置包**（JSON，1Hz） | 目标帧率、音量、静音、是否暂停、语言、屏幕矩形、当前音频源 |
| **输入包**（JSON，≤60Hz） | 鼠标在壁纸窗口客户区里的坐标、按键、滚轮增量 |

传输方式只有一条 —— **本地 UDP**（默认 `127.0.0.1:47810`，地址端口可在设置里改）：
Unity 端零原生插件，一个 `UdpClient` 就行。

### Unity 端要做什么（三步）

1. 把 [`unity/MisideAudioLink.cs`](unity/MisideAudioLink.cs) 与 [`unity/AudioLinkDemo.cs`](unity/AudioLinkDemo.cs)
   丢进工程的 `Assets/`；
2. 场景里任意物体上挂 `MisideAudioLink`，用它的 `Level` / `Spectrum` / `BeatThisFrame` / `Bpm` /
   `PointerNormalized` 驱动你的效果（`AudioLinkDemo.cs` 是可直接运行的例子）；
3. **Player Settings 里必须打开 `Run In Background`**，`Fullscreen Mode` 设为 `Windowed`，
   `Display Resolution Dialog` 关掉（宿主的渲染倍率靠命令行参数生效），然后照常构建 Windows x64。

构建出来的整个目录打成 zip，在宿主里「导入压缩包」即可。

> **输入系统的坑**：宿主转发鼠标靠合成窗口消息（`WM_MOUSEMOVE/WM_LBUTTONDOWN/...`），
> 旧版 Input Manager 收得到，**新版 Input System 收不到**。所以壁纸工程要么用旧输入系统，
> 要么干脆用 `MisideAudioLink.PointerNormalized` / `PointerLeft` —— 宿主把鼠标状态一并推过来了，
> 和输入系统无关。键盘不用管：宿主把真实焦点给了壁纸窗口，两种输入系统都正常。

详细的 Player Settings 解释、打包规则、按现象分类的排查清单、以及"我想自己写接收端"的最小代码，
都在 [docs/unity-integration.md](docs/unity-integration.md)。

---

## 架构

```
   界面（src/*.ts：单页仪表盘 + 设置弹窗）
  ┌──────────────────────────────────────────────────────┐
  │ 顶栏：状态 · 音量 · ⚙ 设置                             │
  │ ┌────────────────────────┬─────────────────────────┐ │
  │ │ 音频可视化（频谱/波形/电平）│ 当前壁纸实时缩略图        │ │
  │ │ 当前监听进程 + 音频源选择  │ 预览/应用/停止/重载/暂停  │ │
  │ └────────────────────────┴─────────────────────────┘ │
  │ 设置弹窗：壁纸库 / 音频 / 通用 / 关于                   │
  └───────────────────────┬──────────────────────────────┘
                          │ Tauri 命令与事件
  ┌───────────────────────▼──────────────────────────────┐
  │  commands.rs  状态 + 全部 IPC 命令 + 1.2s 扫描线程      │
  ├──────────────────────────────────────────────────────┤
  │  library.rs   zip 导入（GBK 名回退 / zip-slip 防护）    │
  │  unity.rs     启动参数 / 挂起 / 音量 / 生命周期         │
  │  win/desktop  WorkerW 挂载 · 几何 · 显示器 · 缩略图抓取 │
  │  win/input    鼠标钩子转发 + 焦点接管（可锁定）          │
  │  win/control  全屏/电池检测 · WASAPI 音量 · 自启        │
  │  audio_link   音频链路：UDP 三类包 + 输入包             │
  │  beat.rs      动态阈值节拍检测 + BPM 估计              │
  │  pac/dsp/capture  采集内核绑定与 DSP 桥接              │
  └───────────────────────┬──────────────────────────────┘
                          │ C ABI
  ┌───────────────────────▼──────────────────────────────┐
  │  ProcessAudioCapture.dll（v3）按进程抓回环音频          │
  └──────────────────────────────────────────────────────┘
```

| 模块 | 负责 |
| --- | --- |
| `commands.rs` | 应用状态、26 个 IPC 命令、后台扫描（壁纸进程 + 音频源编排 + 托盘刷新）、日志 |
| `library.rs` | zip 导入/索引/删除/改名；UTF-8 优先、GBK 回退解码；绝对路径与 `..` 一律拒绝 |
| `unity.rs` | Unity 启动参数、进程挂起/恢复、状态机、几何与音量同步 |
| `win/desktop.rs` | WorkerW 发现（含 Win11 24H2 回退）、`SetParent`、坐标换算、显示器枚举、主窗口查找、`PrintWindow` 缩略图 |
| `win/input.rs` | `WH_MOUSE_LL` 钩子 → 合成消息；焦点接管做键盘；只在桌面前台开闸 |
| `win/control.rs` | `SHQueryUserNotificationState` 全屏判定、`GetSystemPowerStatus` 电池、WASAPI 会话音量、`NtSuspendProcess`、注册表自启、Job Object 进程归属、按 PID 杀进程 |
| `audio_link.rs` | UDP 三类包发送 + 1Hz 状态广播 |
| `beat.rs` | 低频能量动态阈值 onset 检测（按时间窗，不随帧率漂移）+ BPM 中位数估计 |

事件：`wp://state`（运行状态）、`wp://library`、`wp://import-progress`、
`wp://audio-frame`（≤30fps 给界面画图：频谱 + 波形 + 电平 + 节拍/BPM）、
`wp://audio-status`、`wp://settings`、`wp://monitor`（全屏/电池/遮挡）、`wp://log`。
主界面的壁纸实时预览另走命令 `wallpaper_thumbnail`（裸二进制 RGBA，空 = 抓不到）。

---

## 已知限制

* **Windows 10 2004（build 19041）及以上**：采集内核依赖进程回环（`VAD\Process_Loopback`）；
* **壁纸 exe 不能要求管理员权限**：宿主若不是管理员，`SetParent` 到高完整性进程的窗口会被 UIPI 挡掉。
  需要管理员就先以管理员身份启动宿主；
* **一块屏一个壁纸进程**：默认只铺**主显示器**。多屏各自的分辨率/高度不一致时，跨屏铺满会出现
  "没有显示器的空洞"（看起来就是黑边），所以默认不这么做；想跨屏就在设置里选「全部显示器
  （整个虚拟桌面）」。暂不支持每块屏挂不同壁纸 —— 那需要给每块屏各起一个实例；
* **帧率与渲染倍率都靠 Unity 端落地**：宿主只把数值放进配置包，由 `MisideAudioLink` 调
  `Application.targetFrameRate` 与 `ScalableBufferManager.ResizeBuffers`。壁纸工程不挂这个脚本
  （或自己也不处理），这两项就只是"设了没效果"；渲染倍率还有平台限制 —— Windows 独立平台的动态
  分辨率只支持 DirectX 12；
* **暂停是进程挂起**：`NtSuspendProcess` 属未公开 API，实测稳定，但极端情况下可能让某些自制渲染循环
  在恢复后需要一点时间回到正常节奏；不需要就关掉暂停条件；
* **音量在壁纸出声前设置不上去**：Unity 没产生音频会话时 WASAPI 找不到它，宿主会每 2 秒重试，
  界面显示的是你的目标值；
* **输入转发只在桌面是前台时生效**（和 Lively 一样的规则）：否则全系统的鼠标移动都会涌进壁纸窗口；
  键盘会真实抢焦点，接受不了就把转发关掉或打开「锁定」；
* **受保护内容与独占模式音频抓不到**，这是内核上游的限制；
* **实时缩略图抓不到所有壁纸**：`PrintWindow` 对 DirectX 独占渲染的窗口常常只能拿到全黑帧
  （宿主按「抓不到」处理），这类壁纸用「预览」开独立窗口看；壁纸被暂停时宿主不会去抓
  （挂起的进程不响应 `PrintWindow`）。

---

## 目录结构

```
miside-wallpaper-engine/
├── index.html / src/                # 界面（原生 TS + Vite，无框架）
│   ├── api.ts                       # 契约层：命令 wrapper、事件、类型
│   ├── state.ts / i18n.ts / ui.ts   # 状态中枢、中英文案、DOM 与图标助手
│   ├── library.ts / audio.ts / settings-panel.ts / about.ts
│   ├── visualizer.ts                # 频谱 + 电平表 + BPM 可视化
│   └── main.ts / styles.css
├── src-tauri/
│   ├── binaries/ProcessAudioCapture.dll
│   └── src/                         # 见上面「架构」表
├── unity/
│   ├── MisideAudioLink.cs           # Unity 端接收器（UDP + 输入）
│   └── AudioLinkDemo.cs             # 示例：频谱方块 / 节拍脉冲 / 鼠标跟随
├── docs/
│   ├── api-contract.md              # 前后端与 Unity 三方的接口契约（唯一事实来源）
│   ├── protocol.md                  # UDP 协议逐字节表
│   └── unity-integration.md         # Unity 接入与排查
└── scripts/with-msvc.mjs            # 注入 MSVC 环境后执行命令
```

## 测试

```powershell
npm run test:rust    # Rust 侧单测：包布局、zip 导入端到端、GBK 文件名、zip-slip、
                     #              节拍检测、启动参数、显示器枚举、BOM 容错…
npm run check        # 前端类型检查 + Rust 编译检查
```

Unity 端的接收逻辑另有独立自测（临时 .NET 工程 + UnityEngine stub，**248 项断言**，覆盖坏包、
平滑收敛、`OnBeat` 每帧只回调一次、渲染倍率只在变化时落地、线程退出等），细节见
[docs/unity-integration.md](docs/unity-integration.md)。
