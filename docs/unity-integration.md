# 把 Unity 工程接进 miside-wallpaper-engine 宿主（桌面壁纸）

面向 **Unity 使用者**。目标：让你的 Unity 打包 exe 能当桌面动态壁纸，并且跟着系统里正在播放的音乐动起来。

* 协议细节：[`protocol.md`](protocol.md)
* 整体契约：[`api-contract.md`](api-contract.md)
* 接收脚本：[`../unity/MisideAudioLink.cs`](../unity/MisideAudioLink.cs)、示例：[`../unity/AudioLinkDemo.cs`](../unity/AudioLinkDemo.cs)

---

## 1. 三步接入

### 第 1 步：把两个脚本放进工程

把这两个文件拖进 Unity 工程的 `Assets/` 下任意位置（建议 `Assets/MisideWallpaper/`）：

```
MisideAudioLink.cs     ← 接收器（必需）
AudioLinkDemo.cs       ← 示例（可选，先看效果用）
```

它们没有任何第三方依赖，也不需要 `.asmdef`。C# 7.3 兼容，Unity 2019 及以上都能直接编译。

### 第 2 步：在场景里挂组件

1. 场景里新建一个空物体，命名成 `AudioLink`（放哪都行，它不渲染任何东西）；
2. `Add Component` → **MisideWallpaper → Miside Audio Link**；
3. 保持默认值即可：
   * `Enable Udp` = ✅，`Udp Port` = `47810`（要和宿主设置里的「UDP 端口」一致）；
4. 想先看效果，再挂一个 **MisideWallpaper → Audio Link Demo**，两个都挂在同一个物体上也行。

运行后，如果宿主正在推数据，Demo 的左上角会显示 `接收中`、包龄、收包数；方块会跟着频谱跳。

### 第 3 步：构建 Windows x64

`File → Build Settings → PC, Mac & Linux Standalone`：

* Target Platform = **Windows**
* Architecture = **x86_64**
* Build 出来是一个目录，里面至少有：`你的名字.exe`、`你的名字_Data/`、`UnityPlayer.dll`。

把**整个目录**打包成 zip（见第 3 节），在宿主界面里「导入压缩包」即可。

---

## 2. Player Settings 必须项

`Edit → Project Settings → Player → Other Settings / Resolution and Presentation`：

| 设置 | 值 | 为什么 |
| --- | --- | --- |
| **Run In Background** | ✅ **必须开** | 壁纸窗口永远不是前台窗口。不开的话 Unity 在失焦时会**停止渲染并暂停 Update**，壁纸就静止了，音频数据也不会被消费。 |
| **Visible In Background** | ✅ 建议开 | 配合 Run In Background，减少被系统"看不见就当你不存在"的优化。 |
| **Player Log**（`usePlayerLog`） | ✅ 建议开 | 关掉就没有 `Player.log`，壁纸出问题时**完全无从查起**（宿主日志里也只能看到进程还在）。 |
| **Fullscreen Mode** | `Windowed` | 宿主把窗口挂到桌面 WorkerW 层，全屏窗口挂不上去。 |
| **Resizable Window** | ⬜ 建议关 | 宿主用命令行参数指定尺寸；允许用户拖拽改变大小没有意义，还会让客户区坐标和宿主下发的不一致。 |
| **Display Resolution Dialog** | `Disabled`（老版本才有这一项） | 开着的话 exe 启动时会先弹一个分辨率选择框，壁纸模式下就卡在那边了。 |
| **Allow Fullscreen Switch** | ⬜ 建议关 | 防止误触 Alt+Enter 把壁纸切到独占全屏。 |
| **Api Compatibility Level** | `.NET Standard 2.1` 或 `.NET Framework` | 两个都行，按工程本身的需要选即可：UDP 接收只用 `System.Net.Sockets`，两种配置下都可用。 |
| **Managed Stripping Level** | `Minimal`（**不要** `Medium`/`High`） | 高强度剥离会剥掉一批"看起来没人用"的类型，问题常常到运行时才暴露、排查成本很高。接收端只用 UDP，对剥离不算敏感，但也没必要开高。 |
| **Scripting Backend** | `Mono` 或 `IL2CPP` 都行 | Mono 构建快、无需 C++ 工具链；IL2CPP 性能与启动更好。 |
| **Graphics API** | 保持默认或 `Direct3D11` | 挂到桌面层靠的是窗口句柄，和图形 API 无关。**只有想用宿主的「渲染倍率」时才需要注意：Windows 上的动态分辨率只在 `Direct3D12` 下生效**，见下面「关于渲染倍率」。 |

> ⚠️ **构建 Profile 会覆盖这些项，务必一并检查。**
> Unity 6 的 Build Profile（`Assets/Settings/Build Profiles/*.asset`）里带一份完整的 PlayerSettings 副本，
> 它**优先于** `ProjectSettings/ProjectSettings.asset`。踩过的坑：全局 `runInBackground: 1` 是对的，
> 但 Wallpaper Profile 里被覆盖成 `0`，于是壁纸**一失焦就停摆**（表现为"切到别的程序壁纸就不动了"，
> 而且因为拿不回焦点，它不会再自己恢复）。同一个 Profile 里还把 `visibleInBackground` 与
> `usePlayerLog` 一起关掉了，后者让排查时连 `Player.log` 都没有。
> **改完记得在 Build Profile 窗口里核对一遍复选框**，Profile 的 YAML 手改有可能被编辑器覆盖回旧值。

### 关于目标帧率

宿主会通过配置包下发 `targetFps`（默认 60），`MisideAudioLink` 默认会把它写进 `Application.targetFrameRate`。

注意：**`QualitySettings.vSyncCount` 不为 0 时，垂直同步会盖掉 `targetFrameRate`**。想让帧率限制真正生效：

```csharp
// 放在你自己的启动脚本里
QualitySettings.vSyncCount = 0;
```

壁纸场景一般建议 `vSyncCount = 0` + `targetFrameRate = 30` 或 `60`，能明显降低 CPU/GPU 占用。

### 关于渲染倍率（`applyRenderScale`，默认开启）

宿主设置里的「渲染倍率」（0.5 / 0.75 / 1.0）会通过配置包里的 `renderScale` 下发。
**窗口始终铺满整块显示器，降的只是 Unity 的内部分辨率**（再由 GPU 上采样回全屏），
这样省 GPU 又不会让壁纸缩在屏幕一角。

`MisideAudioLink` 默认帮你落地这件事（`applyRenderScale = true`）：

1. 倍率变化时调用一次 `ScalableBufferManager.ResizeBuffers(scale, scale)`；
2. 倍率小于 1.0 时，把所有相机的 `Camera.allowDynamicResolution` 打开（**只在变化时扫一次相机**，不会每帧遍历）；
3. 倍率回到 1.0 时把可缩放缓冲复位；
4. 运行期把 `applyRenderScale` 关掉时，如果之前降过分辨率，会**复位到 1.0 一次**，免得画面一直糊着。

倍率不变时它什么都不做（每帧只有一次 float 比较），所以不用担心中间有额外开销。

#### 什么时候要把它关掉

* 壁纸工程**自己管分辨率**（比如自己调 `ScalableBufferManager`、自己设 `Screen.SetResolution`）；
* 你用的是 **URP / HDRP**，想用渲染管线自带的 Render Scale（见下）；
* 你的画质方案依赖后处理 RT 的固定尺寸，不想被动态分辨率影响。

关掉之后宿主下发的数值照样能在 `link.RenderScale` 里读到，你可以自己决定怎么用。

#### Windows 上的硬限制：需要 DirectX 12

Unity 官方手册写得很明确：Windows 独立平台的动态分辨率**只在 DirectX 12 下支持**
（macOS 需要 Metal、Android 需要 Vulkan）。所以在默认的 **D3D11** 下，
`ResizeBuffers` + `allowDynamicResolution` 是**没有视觉效果**的。

* 想让它生效：`Player Settings → Other Settings → Graphics APIs` 里启用 **Direct3D12**（可以只留 D3D12，或把 D3D12 放前面）；
* 不想切 D3D12：改用渲染管线自带的 Render Scale（URP 的 `URP Asset → Quality → Render Scale`），
  但那个是**静态**的，宿主的倍率就没法实时改——这种情况建议把 `applyRenderScale` 关掉，避免误会；
* `MisideAudioLink` 检测到当前是 D3D11 且倍率小于 1.0 时会打一条 warning 说明这一点（不会刷屏，只在倍率变化时打一次）。

#### URP / HDRP 下怎么配合

* **URP**：URP 有自己的 Render Scale，且它会**盖过 / 叠加**在 `ScalableBufferManager` 之上。
  两处都降会让画面糊得比预期更厉害（0.5 × 0.5 = 0.25）。
  推荐二选一：
  * 用宿主的倍率：把 URP Asset 的 Render Scale 保持 `1.0`，让 `applyRenderScale` 生效（并切到 D3D12）；
  * 用 URP 的倍率：把 `applyRenderScale` 关掉，自己按 `link.RenderScale` 去改 URP Asset 的 render scale；
    注意 URP 的 render scale 是 Asset 上的静态设置，**每个 Quality Level 一份**，实时改要遍历
    `QualitySettings` 或者维护多个 URP Asset。
* **相机自己渲染到 RenderTexture** 的情况：还额外要求该 RenderTexture 允许动态缩放
  （`RenderTexture.useDynamicScale = true`，或创建时带 `DynamicallyScalable` 标志），
  否则那个相机的目标不会被缩放。
* **HDRP**：动态分辨率的启用方式与内置管线不同，请按 HDRP 文档配置；必要时直接关掉 `applyRenderScale`。

#### 想自己确认倍率有没有生效

```csharp
// 实际内部分辨率（不管是谁设的，都能读到当前可缩放缓冲的系数）
int w = Mathf.CeilToInt(ScalableBufferManager.widthScaleFactor * UnityEngine.Screen.width);
int h = Mathf.CeilToInt(ScalableBufferManager.heightScaleFactor * UnityEngine.Screen.height);
Debug.Log($"内部分辨率 {w}x{h}，宿主下发倍率 {link.RenderScale:0.00}");
```

Demo 的左上角浮层也会显示 `link.RenderScale`。

---

## 3. 打包与导入

1. Unity 构建出来的**整个目录**（不是单独一个 exe）右键 → 压缩成 zip；
2. zip 里**不要再套一层目录**。也就是说解压后应该直接看到 `xx.exe`、`xx_Data/`、`UnityPlayer.dll`，
   而不是 `xx/xx.exe`；
3. 在宿主里「导入压缩包」→ 宿主会解压到壁纸库目录并识别主程序。

宿主是**怎么认出主 exe** 的（按优先级）：

* 和压缩包同名（去掉 `.zip`）的 exe，例如 `Miside.zip` → `Miside.exe`；
* 否则取目录里**体积最大**的 exe。

所以最省事的做法是：**用主 exe 的名字给 zip 命名**。

> ⚠️ **别漏掉 exe 之外要跟着走的目录。**
> * Unity 播放器依赖同级的 `xx_Data/`、`UnityPlayer.dll`、`GameAssembly.dll`（IL2CPP）、`baselib.dll`、`D3D12/`；
> * **工程自己的数据目录也要一起打**——例如 MiSide 壁纸的 `GlobalGame.dataFiles = "Data"` 是**相对工作目录**解析的，
>   而宿主正是以 exe 所在目录为工作目录启动它。少了 `Data/`（Languages / LanguagesVoice / Custom，约 171 MB），
>   运行时会抛 `DirectoryNotFoundException: ...\Data\LanguagesVoice` 并且没有语音/本地化。
> * **排除调试符号目录**：`*_BackUpThisFolder_ButDontShipItWithYourGame/`（可能上 GB）与
>   `*_BurstDebugInformation_DoNotShip/` 都不该进包。

被识别为 Unity 程序的标志：同级目录里有 `*_Data` 目录或 `UnityPlayer.dll`（宿主会在壁纸信息里显示 `unity: true`）。
缺少这些文件的 zip 仍然能导入，但只是当普通程序跑，音频数据照样能收到。

---

## 4. 宿主传给你的命令行参数

宿主启动你的 exe 时会带上这些参数（用户还可以在设置里加 `extraArgs`）：

| 参数 | 说明 |
| --- | --- |
| `-screen-width <px>` | 目标客户区宽度。窗口始终铺满整块显示器；**这不是「渲染倍率」**，倍率由配置包 + Unity 端落地，见第 2 节「关于渲染倍率」。 |
| `-screen-height <px>` | 目标客户区高度（同上）。 |
| `-screen-fullscreen 0` | 强制窗口化（Unity 原生参数）。 |
| `-popupwindow` | 无边框弹出窗口（Unity 原生参数）。挂到桌面层时通常需要它。 |
| 用户自定义参数 | 直接追加在最后。 |

这些是 **Unity 玩家程序自带支持**的参数，你不需要写任何代码就能生效（前提是分辨率对话框已关闭）。

### 读你自己的自定义参数

```csharp
using System;
using UnityEngine;

public class CommandLineConfig : MonoBehaviour
{
    void Awake()
    {
        string[] args = Environment.GetCommandLineArgs();   // 含 exe 路径本身
        for (int i = 0; i < args.Length; i++)
        {
            if (args[i] == "-quality" && i + 1 < args.Length)
            {
                Debug.Log("用户指定的画质: " + args[i + 1]);
            }
        }
    }
}
```

> Unity 不会把这些 `-xxx` 参数从 `Environment.GetCommandLineArgs()` 里删掉，所以直接读就行。
> 注意参数是**空格分隔**的，值里带空格需要用户自己加引号。
> 运行期想拿到窗口真实尺寸，用 `UnityEngine.Screen.width/height` 或配置包里的 `Screen` 字段。

---

## 5. 输入系统的坑（重要）

| 输入类型 | 宿主怎么转发 | 旧版 Input Manager | 新版 Input System |
| --- | --- | --- | --- |
| **鼠标**（移动/按键/滚轮） | 向壁纸窗口**合成窗口消息**（`WM_MOUSEMOVE` / `WM_LBUTTONDOWN` / `WM_LBUTTONUP` / `WM_MOUSEWHEEL` …） | ✅ 能收到 | ❌ **收不到**（新版走 Raw Input / `WM_INPUT`，必须有真实硬件事件） |
| **键盘** | 把真实交点交给壁纸窗口，按键是真实输入 | ✅ | ✅ |
| **指针状态** | 宿主在**输入包**里一并推给你 | ✅ | ✅（和输入系统完全无关） |

结论与建议：

* 壁纸工程**优先用旧版 Input Manager**（`Project Settings → Player → Active Input Handling = Input Manager (Old)`）；
* 或者**完全不用 Unity 的鼠标输入**，改用本脚本提供的：
  `PointerPosition` / `PointerNormalized` / `PointerViewport` / `PointerInside` /
  `PointerLeft` / `PointerRight` / `PointerMiddle` / `PointerWheelDelta` / `PointerLocked`；
* 键鼠同时要时也可以混用：键盘用 `Input.GetKey`，鼠标用 `MisideAudioLink` 的属性。

```csharp
void Update()
{
    if (link.PointerInside && link.PointerLeft) { /* 相当于 GetMouseButton(0) */ }
    float scroll = link.PointerWheelDelta;      // 本帧滚轮增量，一格通常是 ±120
    Vector2 n = link.PointerNormalized;         // 0–1，左上角原点（宿主口径）
    Vector2 v = link.PointerViewport;           // 0–1，左下角原点（Unity 惯例）
    Vector3 world = cam.ViewportToWorldPoint(new Vector3(v.x, v.y, 10f));   // cam = 你自己的相机
}
```

> 坐标口径提醒：`PointerPosition` / `PointerNormalized` 是**宿主口径**（左上角原点、Y 向下），
> 和宿主协议逐字段一致；`PointerViewport` 已经翻成 **Unity 惯例**（左下角原点、Y 向上），
> 直接丢给 `Camera.ViewportToWorldPoint` 即可。
> 另外，宿主只有在「输入转发」打开、且桌面在前台时才会发输入包，所以**一定要看 `PointerInside`**。

---

## 6. 调试与排查

### 6.1 完全没数据

按顺序查：

1. **先看 `LastError` 和 `PacketsReceived`**（Demo 的左上角浮层会显示，也可以自己 `Debug.Log`）：
   * `LastError` 非空 → 端口绑定失败，看错误文本；
   * `PacketsReceived == 0` 且 `LastError` 为空 → 宿主根本没在发，或者发到别的端口；
   * `PacketsReceived` 在涨但 `IsReceiving == false` → 数据是旧的（宿主中途停了）。
2. **`PacketsDroppedBadMagic` 在涨** → 这个端口上有人在发**别的协议**的数据（端口冲突）。
   换一个端口，同时在宿主设置里改「UDP 端口」保持一致。
3. **端口被占**：宿主默认端口是 `47810`。别的壁纸实例、别的软件都可能占用。`LastError` 会写明
   `UDP 端口 47810 绑定失败：AddressAlreadyInUse`。
4. **防火墙**：本地回环（127.0.0.1）一般不会被拦，但某些第三方安全软件会拦 UDP。可以临时关掉验证，
   或者把宿主和你的 exe 都加进白名单。
5. **宿主侧**：
   * 设置里「推送音频节奏」是否打开（`audioEnabled`）；
   * 「音源」是不是 `off`，或者选了 `process` 但进程选错了（`audioSource` / `audioProcess`）；
   * 那个进程**现在必须在出声**——很多播放器暂停时 WASAPI 会话是静音的，采到全是 0；
   * 独占模式（WASAPI Exclusive）输出的播放器可能采不到。

### 6.2 有数据但画面不对

| 现象 | 原因 |
| --- | --- |
| 数值一跳一跳的 | 平滑关掉了或半衰期太小；调大 `levelSmoothingHalfLife` / `spectrumSmoothingHalfLife` |
| 数值一直不动 | `Run In Background` 没开，Unity 暂停了；或者 `Paused` 为 true |
| 频谱只在最左边有值 | 频谱是对数频率轴，低频本来就占前几根柱子；用 `GetBandAt` 均匀取样 |
| 鼠标没反应 | 输入转发没开 / 用了新版 Input System（见第 5 节）/ 没检查 `PointerInside` |
| 帧率限制没生效 | `QualitySettings.vSyncCount` 不是 0 |
| 宿主改了「渲染倍率」但画面/GPU 没变化 | Windows 上是 DX11（动态分辨率只支持 DX12）；或者 URP Asset 的 Render Scale 盖住了；或者 `applyRenderScale` 被关掉了；或者用了 `RenderTexture` 但没开 `useDynamicScale` |
| 壁纸静止但 exe 没崩 | 失焦被 Unity 暂停（`Run In Background`），或者宿主因为全屏应用/电池策略挂起了进程 |

### 6.3 性能建议

* **`Update` 里不要分配**：`MisideAudioLink` 的 `Spectrum` / `WaveformMax` / `WaveformMin` 都是
  预分配并复用的数组，直接读、不要 `ToArray()` / 不要 `new`。
* 频谱**只取你要的柱数**：要 32 根柱子就用 `GetBandAt(i / 31f)`，不要去遍历 128 个再算平均。
* 自己再平滑的话，用与帧率无关的写法：
  ```csharp
  float k = 1f - Mathf.Exp(-0.6931472f * Time.unscaledDeltaTime / 0.08f);
  value = Mathf.Lerp(value, target, k);
  ```
* `Mathf.PerlinNoise`、`GameObject.Find`、`Camera.main` 这类调用不要放在每帧的循环里。
* **示例里的 `OnGUI` 浮层会持续分配内存**，正式发布请把 `AudioLinkDemo.showOverlay` 关掉，
  或者干脆不要挂 Demo。
* 接收线程空闲时按 `receivePollIntervalMs`（默认 1ms）轮询，几乎不吃 CPU；
  设成 0 是忙等，只在极端低延迟需求下用。
* 壁纸常年后台跑：把 `Application.targetFrameRate` 控制在 30–60，
  阴影/后处理/实时光照该关就关（`QualitySettings` 里配一个低配档位）。
* **GPU 还是吃满就把「渲染倍率」调到 0.75 或 0.5**（宿主设置里改，会通过 `renderScale` 实时下发）。
  0.5 大约能把像素量降到 1/4，是壁纸最划算的一个开关；前提是 `applyRenderScale` 保持开启，
  且（Windows 上）工程启用了 DirectX 12，详见「关于渲染倍率」一节。

---

## 7. 想自己写接收端？最小可用代码

不用 `MisideAudioLink` 也行。下面是**只解析帧包**的最小实现（放进 `Assets/` 即可）：

```csharp
using System;
using System.Net;
using System.Net.Sockets;
using System.Threading;
using UnityEngine;

public class MyTinyAudioReceiver : MonoBehaviour
{
    const int Port = 47810;
    const int FramePacketSize = 440;    // 56 + 128(频谱) + 256(波形)

    readonly float[] _spectrum = new float[128];
    readonly object _lock = new object();
    Thread _thread;
    UdpClient _udp;
    volatile bool _running;
    float _level;
    long _packets;

    void OnEnable()
    {
        _running = true;
        _thread = new Thread(Loop) { IsBackground = true };
        _thread.Start();
    }

    void OnDisable()
    {
        _running = false;
        if (_udp != null) { try { _udp.Close(); } catch (Exception) { } _udp = null; }
        if (_thread != null) { _thread.Join(300); _thread = null; }
    }

    void Loop()
    {
        byte[] buffer = new byte[1024];
        try { _udp = new UdpClient(Port); }
        catch (SocketException e) { Debug.LogWarning("UDP 绑定失败: " + e.SocketErrorCode); return; }

        EndPoint any = new IPEndPoint(IPAddress.Any, 0);
        while (_running)
        {
            try
            {
                if (_udp.Available <= 0) { Thread.Sleep(1); continue; }
                int n = _udp.Client.ReceiveFrom(buffer, 0, buffer.Length, SocketFlags.None, ref any);

                // 1) 校验：magic "MWAV" + version 1 + type 1 + 长度
                if (n < FramePacketSize) continue;
                if (buffer[0] != 0x4D || buffer[1] != 0x57 || buffer[2] != 0x41 || buffer[3] != 0x56) continue;
                if (buffer[4] != 1 || buffer[5] != 1) continue;

                // 2) 小端读 f32：offset 32 = level，40 = bass，52 = bpm，56 = 频谱起点
                float level = BitConverter.ToSingle(buffer, 32);
                lock (_lock)
                {
                    _level = level;                       // 只演示 level，bass/bpm 同理
                    for (int i = 0; i < 128; i++) _spectrum[i] = buffer[56 + i] / 255f;
                    _packets++;
                }
            }
            catch (SocketException) { }
            catch (ObjectDisposedException) { break; }
        }
    }

    void Update()
    {
        float level;
        lock (_lock) { level = _level; }
        transform.localScale = Vector3.one * (1f + level);   // 想怎么用就怎么用
    }
}
```

要点：

* `BitConverter.ToSingle` / `BitConverter.ToUInt32` 在 Windows x64/x86 上就是小端，可以直接用；
* 上线前把 `ReceiveFrom` 换成"预分配缓冲 + 一次性解析"，不要每包 `new byte[]`；
* 别在 `Start()` 里阻塞主线程收包，永远放到后台线程；
* 一个更完整的版本（配置包、输入包、平滑、节拍 latch）就是
  [`MisideAudioLink.cs`](../unity/MisideAudioLink.cs)，直接抄它的 `MisideProtocol` 类也行。

---

## 8. `MisideAudioLink` 速查

| 成员 | 类型 | 说明 |
| --- | --- | --- |
| `Rms` / `Peak` / `Level` | `float` 0–1 | 响度类 |
| `Bass` / `Mid` / `Treble` | `float` 0–1 | 三频段能量 |
| `Bpm` | `float` | `0` = 未知 |
| `Spectrum` | `float[128]` 0–1 | 对数频谱，只读 |
| `GetBand(int)` / `GetBandAt(float)` | `float` | 按下标 / 归一化位置取柱（后者带插值） |
| `WaveformMax` / `WaveformMin` | `float[128]` -1–1 | 波形包络，只读 |
| `GetWaveform(int)` / `GetWaveformRange(int, out, out)` | `float` | 中点 / min+max |
| `BeatThisFrame` | `bool` | 本帧有节拍（一帧最多一次）—— **壁纸端自己检测**，见下方说明 |
| `BeatStrength` / `BeatPulse` | `float` | 原始强度 / 会衰减的脉冲包络 |
| `LastBeatTime` / `TimeSinceBeat` | `float` | `Time.time` 口径；没有节拍时为 ±Infinity |
| `OnBeat` | `event Action<float>` | 节拍主线程回调，参数是强度 |
| `LocalBeatThisFrame` / `LocalBpm` | `bool` / `float` | 本地检测结果（`LocalBeatThisFrame` **未过 `beat` 开关**，用于排查） |
| `LocalBeatEnergy` / `LocalBeatThreshold` | `float` | 本地检测本帧的能量与阈值（"为什么没打上拍"看这里） |
| `HostBeatFlag` / `HostBpm` | `float` | 宿主帧包里的 `beat` / `bpm`，**仅诊断**，不驱动玩法 |
| `TargetFps` / `RenderScale` / `Muted` / `Volume` / `Paused` / `Language` / `Screen` | — | 配置包内容（`RenderScale` 是宿主下发的值，0.25–1.0，没收到配置包时为 1.0） |
| `OnConfigChanged` | `event Action` | 配置变化时主线程回调 |
| `PointerPosition` / `PointerNormalized` / `PointerViewport` | `Vector2` | 前两个是宿主口径，第三个已翻成 Unity 惯例 |
| `PointerInside` / `PointerLeft` / `PointerRight` / `PointerMiddle` / `PointerWheelDelta` / `PointerLocked` | — | 指针状态 |
| `IsReceiving` / `LastPacketAgeMs` / `PacketsReceived` | — | 连接状态 |
| `PacketsDropped` / `PacketsDroppedBadMagic` / `...BadVersion` / `...TooShort` | `long` | 坏包计数，排查端口冲突很有用 |
| `LastError` / `Reconnect()` | — | 最近一条错误 / 重新开始接收 |

Inspector 里可调的：`enableUdp`、`udpPort`、
`logLevel`、`staleTimeoutMs`、`smoothing`、`levelSmoothingHalfLife`、`spectrumSmoothingHalfLife`、
`applyTargetFramerate`、`applyRenderScale`、`pauseAudioListenerWhenPaused`、`receivePollIntervalMs`、
`inputStaleTimeoutMs`、`beatPulseDecay`。

> `applyRenderScale`（默认开）负责把 `renderScale` 落到 `ScalableBufferManager.ResizeBuffers`
> 并把相机的 `allowDynamicResolution` 打开；壁纸工程自己管分辨率、或要用 URP 的 Render Scale 时把它关掉。
> 细节与 DirectX 12 限制见「关于渲染倍率」。

---

## 9. 自定义参数与参数上报

除了音频 / 配置 / 输入，宿主还会下发一组**自定义参数**（`config` 包里的 `params`），
并期待壁纸端把「这些参数有哪些可选值」报回去（反向的 `type = 4` 包）。

### 9.1 宿主下发什么

宿主**不解释**参数语义，只做透传。开箱预置两条：

| 键 | 默认值 | 默认锁定 | 约定语义 |
| --- | --- | --- | --- |
| `beat` | `"1"` | ✅ **锁定** | 是否允许节拍驱动壁纸行为（`"0"` = 关）。锁定防止用户误关整条节奏链路 |
| `costume` | `"random"` | ⬜ 不锁定 | 米塔服装：`random` = 随机换一套，否则是服装存档名 |

用户还能在控制端「通用 → 自定义参数」里自己加键值对。值**统一是字符串**，
所以接收端做类型转换时必须给兜底值：

```csharp
// 用统一接收端（推荐）
var hub = WallpaperDataHub.Instance;
bool beatOn   = hub.GetParamBool("beat", true);        // "1"/"true"/"on"/"yes" → true
float strength = hub.GetParamFloat("myGain", 1f);      // 非法内容 → 兜底 1
string outfit  = hub.GetParamString("costume", "");
bool locked    = hub.IsParamLocked("costume");
```

> 本节的 `WallpaperDataHub` / `MitaCostumeSwapper` 是 **MiSide 壁纸工程自带**的门面组件
> （见 §9.3）。用随程序分发的 `unity/MisideAudioLink.cs` 搭自己的工程时，`params` 数组同样
> 会在配置包里下发 —— 只是那个精简版接收端不认识它（协议是**增量兼容**的，老接收端会
> 直接忽略新字段，不会有任何报错）。想用自定义参数就按 §9.3 自己包一层，
> 或直接读 [`protocol.md` §2.4](protocol.md) 的 JSON 结构。

### 9.2 壁纸端回报「有哪些可选值」

`costume` 的可选项只有壁纸端知道（运行时扫描服装表），所以控制端的下拉框
靠你注册的 **provider** 填充：

```csharp
hub.RegisterParamProvider(
    WallpaperDataHub.ParamCostume,          // key，与 config 里的对应
    "米塔服装",                              // 展示名
    () => GlobalGame.clothMita,             // 当前生效的值
    () => BuildOptions(),                   // 可选项；返回 null/空 = 自由填写
    () => true);                            // 现在是否接受改（false 会让输入置灰）
hub.ReportNow();                            // 注册完立刻报一次，别让用户干等
```

接收端会自动按 `reportPort` 周期上报（默认 2 秒一次，`reportInterval = 0` 可关）。
控制端收到后广播 `wp://param-report` 事件，界面据此渲染下拉；**没上报时就退回自由输入框**，
所以老版本接收端不会因为缺这个功能而出问题。

> 完整字段表：配置包参数见 [`protocol.md` §2.4](protocol.md)、上报包见 [§2.5](protocol.md)。
> 宿主侧的类型定义见 [`api-contract.md` §1.1 / §2.8](api-contract.md)。

### 9.3 统一接收端 `WallpaperDataHub`（壁纸工程用）

壁纸工程把「收包 + 频谱换算 + 参数访问 + 上报」收进了一个门面组件
（`Assets/MisideWallpaper/Script/Audio/WallpaperDataHub.cs`），
底层仍是 `MisideAudioLink`（纯协议解析）：

```csharp
var hub = WallpaperDataHub.Instance;      // 自动安装，无需改场景
float level = hub.Level;
float[] bars = hub.SmoothSpectrum;        // 256 根，与 GetSpectrumData 同量级
if (hub.BeatThisFrame) { /* 已受 beat 参数开关控制 */ }
hub.OnBeat += strength => { /* ditto */ };
```

它比直接用 `MisideAudioLink` 多做了三件事，**这三件都建议走它**：

1. **频谱换算** —— 128 根对数频段 → 与原 `GetSpectrumData` 同量级的线性频谱（带自适应归一化）；
2. **节拍闸门** —— `BeatThisFrame` / `OnBeat` 已经过 `beat` 参数过滤，
   业务侧不用各自判断开关（关掉后节拍玩法自然停住）；
3. **参数与上报** —— `GetParam*` / `RegisterParamProvider` 都在这里。

> 需要 `MisideAudioLink` 上的底层能力（比如坏包计数、收包时间）时用 `hub.Link`。

#### 9.3.1 节拍判定归壁纸端

**控制端只采集音频、推数据**；"哪一帧算节拍"由壁纸端自己决定
（`WallpaperDataHub` 里的 `DetectBeat()` +
`Assets/MisideWallpaper/Script/Audio/WallpaperBeatDetector.cs`）。
帧包里的 `beat` / `bpm` 字段仍然存在，但只作诊断（`HostBeatFlag` / `Bpm`）。

理由：节拍判定是**玩法决策**（"这一刻该不该动"），应当和动画状态在同一侧；
而且判定要用的那份"平滑放大后频谱"（对应原始端的 `smoothBuffer`）本来就由 hub 维护，
检测和使用放在一起才和原始端结构一致。

判定用的是**原始端 `UnityAudioBeat` 的算法**：

```
energy = 均方(smoothSpectrum)
beat   = energy > 0.0125 && (上一帧 energy - energy) > 0
```

即「音乐在响 + 这一帧能量在回落」，音乐播放期间约一半帧成立 —— 这正是
原始端点头明显的原因（给 `beatPulse` 一个近乎恒定的抬升，而非离散脉冲）。
回归测试见 `.verify-unity/beat-check/`（与原始端参照实现逐帧比对，3000 帧零差异）。

> 因为这套算法没有灵敏度旋钮，宿主界面上原来的「节拍灵敏度」滑杆已经删掉
> （配置包里也不再下发 `beatSensitivity`）。

### 9.4 实例：把「米塔服装」接进壁纸

MiSide 壁纸工程里有一个完整例子可以参考 ——
`Assets/MisideWallpaper/Script/Audio/WallpaperCostumeBridge.cs`。
它把一条参数双向接到了 `MitaCostumeSwapper` 上：

| 方向 | 参数键 | 壁纸端落点 |
| --- | --- | --- |
| 下行（控制端 → 壁纸） | `costume` | `MitaCostumeSwapper.ApplyCostume()` / `SwapRandomCostume()` |
| 上行（壁纸 → 控制端） | `costume` 的可选项 | `hub.RegisterParamProvider(..., BuildOptions, ...)` |

要点（写自己的桥时都适用）：

* **节拍相关的玩法不要自己做离散动画**。原始壁纸的"音乐点头"是
  `headDance.Evaluate(timeDance) × (lerpJump × intensityJumpHeadDance)`
  这条**连续**链路：`timeDance` 自由跑曲线，幅度由音乐能量（含节拍脉冲
  `beatPulse`）平滑调制。曾经额外加过一层"每个节拍播一次单次点头"的离散实现，
  结果是和连续链路互相打架（顿挫、先仰头再低头），现已删除 ——
  **节拍要接进 `lerpJump` 那条链路，而不是另起一条动画**。
* **目标组件要轮询等待**。Mita prefab 由 `MisideWallpaperControl.Start()` 运行时实例化，
  `AfterSceneLoad` 那一刻还不存在；桥自己装成常驻物体轮询，
  并且**等一帧再首次应用参数**（`MitaClothes.Start()` 得先跑完，否则换装会骨骼错乱）。
* **参数随配置包每秒重发**，所以下行处理必须**按值去重**，
  否则每秒都会重放一次动作（换装 / 触发）。
* **上行能力要注册**：控制端不能硬编码枚举（服装表只有壁纸端知道），
  用 `RegisterParamProvider` 报「当前值 + 可选项 + 是否可写」。
