// =============================================================================
//  AudioLinkDemo.cs  —  MisideAudioLink 的最小可用示例
// -----------------------------------------------------------------------------
//  这是一个「看得见」的示例：把 MisideAudioLink 和 AudioLinkDemo 挂到场景里任意
//  一个空 GameObject 上，直接运行就能看到：
//    * Level    → 整体缩放 + 自发光强度
//    * 节拍      → 一次性脉冲（OnBeat 回调 / BeatThisFrame）
//    * Spectrum → 一排运行时生成的方块高度
//    * Bpm      → 匀速旋转的速度
//    * 指针      → 鼠标跟随（数据来自宿主，和 Unity 输入系统无关）
//
//  空场景也能跑：找不到 MisideAudioLink 时会打印一条提示并自动切到内置模拟数据
//  （simulateWithoutLink），不会报一屏幕红字。
//
//  C# 7.3 兼容，不使用任何 Unity 2019 之后的 API。
// =============================================================================

using System;
using UnityEngine;

namespace MisideWallpaper
{
    /// <summary>MisideAudioLink 的示例驱动脚本，演示常见用法。</summary>
    [AddComponentMenu("MisideWallpaper/Audio Link Demo (示例)")]
    public sealed class AudioLinkDemo : MonoBehaviour
    {
        // ---------------------------------------------------------------------
        //  Inspector
        // ---------------------------------------------------------------------

        /// <summary>数据来源；留空则自动在场景里找。</summary>
        [Header("数据来源")]
        [Tooltip("留空会自动 FindObjectOfType<MisideAudioLink>()。")]
        public MisideAudioLink link;

        /// <summary>是否自动在场景里查找 MisideAudioLink。</summary>
        [Tooltip("link 为空时是否自动在场景里查找。")]
        public bool autoFindLink = true;

        /// <summary>没有链接时是否用内置模拟数据（保证空场景能看到东西）。</summary>
        [Tooltip("找不到 MisideAudioLink 时用内置的正弦/噪声数据驱动，方便先看效果。")]
        public bool simulateWithoutLink = true;

        /// <summary>用 Level 驱动的整体缩放目标（留空 = 不缩放）。</summary>
        [Header("Level → 缩放 / 发光")]
        [Tooltip("整体缩放的目标，留空则不缩放。")]
        public Transform scaleTarget;

        /// <summary>Level = 1 时额外缩放多少（0.25 表示最大放大到 1.25 倍）。</summary>
        [Tooltip("Level = 1 时的额外缩放量。")]
        public float scaleAmount = 0.25f;

        /// <summary>用 Level 驱动自发光的 Renderer。</summary>
        [Tooltip("用 Level 驱动 _EmissionColor 的 Renderer，留空则不做。")]
        public Renderer glowRenderer;

        /// <summary>自发光颜色。</summary>
        public Color glowColor = new Color(1f, 0.35f, 0.75f, 1f);

        /// <summary>自发光强度倍率。</summary>
        [Tooltip("自发光强度倍率（配合 HDR/后处理泛光才看得出来）。")]
        public float glowIntensity = 2f;

        /// <summary>节拍脉冲的缩放目标。</summary>
        [Header("节拍 → 一次性脉冲")]
        [Tooltip("节拍脉冲作用的 Transform，留空则用 scaleTarget。")]
        public Transform pulseTarget;

        /// <summary>脉冲最大额外缩放。</summary>
        [Tooltip("脉冲瞬间的额外缩放量。")]
        public float pulseScale = 0.4f;

        /// <summary>脉冲衰减时间（秒）。</summary>
        [Tooltip("脉冲从 1 衰减到 0 的秒数。")]
        public float pulseDecay = 0.3f;

        /// <summary>方块父节点；留空会自动创建一个。</summary>
        [Header("Spectrum → 一排方块")]
        [Tooltip("方块父节点，留空会自动创建一个名为 AudioBars 的 GameObject。")]
        public Transform barsRoot;

        /// <summary>方块数量。</summary>
        [Range(4, 128)]
        public int barCount = 32;

        /// <summary>方块间距。</summary>
        public float barSpacing = 0.35f;

        /// <summary>方块最大高度（频谱为 1 时）。</summary>
        public float barMaxHeight = 4f;

        /// <summary>方块厚度（X 方向）。</summary>
        public float barWidth = 0.22f;

        /// <summary>方块材质；留空用 Unity 默认材质。</summary>
        [Tooltip("方块材质，留空则用 CreatePrimitive 自带的默认材质。")]
        public Material barMaterial;

        /// <summary>是否在运行时生成方块（关闭后可以用你自己摆好的 barsRoot 子物体）。</summary>
        [Tooltip("开启会在 Awake 时用 CreatePrimitive 生成方块；关闭则复用 barsRoot 现有的子物体。")]
        public bool createBarsAtRuntime = true;

        /// <summary>BPM 驱动的旋转目标。</summary>
        [Header("Bpm → 匀速旋转")]
        [Tooltip("按 BPM 匀速旋转的目标，留空则不旋转。")]
        public Transform rotateTarget;

        /// <summary>每拍旋转多少度。</summary>
        [Tooltip("每拍旋转的角度（360 = 每拍一圈）。")]
        public float degreesPerBeat = 90f;

        /// <summary>转速平滑时间。</summary>
        [Tooltip("转速变化的平滑时间（秒），避免 BPM 抖动导致转速突变。")]
        public float rotationSmoothing = 0.25f;

        /// <summary>鼠标跟随目标。</summary>
        [Header("指针 → 鼠标跟随")]
        [Tooltip("跟随鼠标（宿主推送的指针数据）的 Transform，留空则不跟随。")]
        public Transform pointerTarget;

        /// <summary>跟随活动半径。</summary>
        [Tooltip("指针从中心到边缘时移动的世界单位距离。")]
        public float pointerFollowRadius = 5f;

        /// <summary>跟随速度。</summary>
        public float pointerFollowSpeed = 12f;

        /// <summary>是否显示左上角调试信息。</summary>
        [Header("调试")]
        [Tooltip("显示 BPM / 收包数 / 错误信息，方便排查“没数据”。")]
        public bool showOverlay = true;

        // ---------------------------------------------------------------------
        //  运行时
        // ---------------------------------------------------------------------

        private const int SimulationBins = 128;

        private readonly float[] _simSpectrum = new float[SimulationBins];
        private readonly float[] _emptySpectrum = new float[SimulationBins];

        private Transform[] _bars;
        private Renderer[] _barRenderers;
        private MaterialPropertyBlock _propertyBlock;
        private Vector3[] _barBaseScale;

        private float _pulse;
        private float _rotationSpeed;
        private float _simTime;
        private float _nextSimBeat;
        private bool _loggedNoLink;

        private void Awake()
        {
            ResolveLink();
            BuildBars();
        }

        private void OnEnable()
        {
            ResolveLink();
            if (link != null) link.OnBeat += HandleBeat;
        }

        private void OnDisable()
        {
            if (link != null) link.OnBeat -= HandleBeat;
        }

        private void ResolveLink()
        {
            if (link == null && autoFindLink)
            {
                link = FindObjectOfType<MisideAudioLink>();
            }
            if (link == null && !_loggedNoLink)
            {
                _loggedNoLink = true;
                if (simulateWithoutLink)
                {
                    Debug.Log("[AudioLinkDemo] 场景里没有 MisideAudioLink，已改用内置模拟数据；"
                        + "想接真实数据就新建一个空物体并挂上 MisideAudioLink 组件。");
                }
                else
                {
                    Debug.LogWarning("[AudioLinkDemo] 场景里没有 MisideAudioLink，示例不会动。");
                }
            }
        }

        private void BuildBars()
        {
            if (barsRoot == null)
            {
                GameObject root = new GameObject("AudioBars");
                root.transform.SetParent(transform, false);
                barsRoot = root.transform;
            }

            if (createBarsAtRuntime)
            {
                int count = Mathf.Clamp(barCount, 1, 128);
                _bars = new Transform[count];
                _barRenderers = new Renderer[count];
                _barBaseScale = new Vector3[count];

                float totalWidth = (count - 1) * barSpacing;
                for (int i = 0; i < count; i++)
                {
                    GameObject bar = GameObject.CreatePrimitive(PrimitiveType.Cube);
                    bar.name = "Bar" + i;
                    bar.transform.SetParent(barsRoot, false);
                    bar.transform.localPosition = new Vector3(i * barSpacing - totalWidth * 0.5f, 0f, 0f);
                    bar.transform.localScale = new Vector3(barWidth, 0.001f, barWidth);

                    // 示例不需要物理，去掉碰撞体省一点开销。
                    Collider collider = bar.GetComponent<Collider>();
                    if (collider != null) Destroy(collider);

                    if (barMaterial != null)
                    {
                        Renderer renderer = bar.GetComponent<Renderer>();
                        if (renderer != null) renderer.sharedMaterial = barMaterial;
                    }

                    _bars[i] = bar.transform;
                    _barRenderers[i] = bar.GetComponent<Renderer>();
                    _barBaseScale[i] = bar.transform.localScale;
                }
            }
            else
            {
                // 复用 barsRoot 现有的子物体
                int count = barsRoot.childCount;
                _bars = new Transform[count];
                _barRenderers = new Renderer[count];
                _barBaseScale = new Vector3[count];
                for (int i = 0; i < count; i++)
                {
                    _bars[i] = barsRoot.GetChild(i);
                    _barRenderers[i] = _bars[i].GetComponent<Renderer>();
                    _barBaseScale[i] = _bars[i].localScale;
                }
            }
        }

        private void HandleBeat(float strength)
        {
            // OnBeat 是主线程回调，可以直接碰 Unity API。
            _pulse = 1f;
        }

        private void Update()
        {
            float delta = Time.deltaTime;
            if (delta <= 0f) delta = 1f / 60f;

            float level;
            float bpm;
            float[] spectrum;

            if (link != null && link.IsReceiving)
            {
                level = link.Level;
                bpm = link.Bpm;
                spectrum = link.Spectrum;
                if (link.BeatThisFrame) _pulse = 1f;   // BeatThisFrame 与 OnBeat 二选一都行
            }
            else if (simulateWithoutLink)
            {
                Simulate(delta, out level, out bpm);
                spectrum = _simSpectrum;
            }
            else
            {
                level = link != null ? link.Level : 0f;
                bpm = link != null ? link.Bpm : 0f;
                spectrum = _emptySpectrum;
            }

            // 脉冲衰减
            if (_pulse > 0f)
            {
                float decay = pulseDecay > 0.001f ? pulseDecay : 0.001f;
                _pulse = Mathf.Clamp01(_pulse - delta / decay);
            }

            ApplyLevel(level);
            ApplyBars(spectrum);
            ApplyRotation(bpm, delta);
            ApplyPointer(delta);
        }

        /// <summary>Level → 整体缩放 + 自发光；叠上节拍脉冲。</summary>
        private void ApplyLevel(float level)
        {
            float scale = 1f + level * scaleAmount + _pulse * pulseScale;
            Transform target = scaleTarget != null ? scaleTarget : (pulseTarget != null ? pulseTarget : null);
            if (target != null) target.localScale = Vector3.one * scale;

            if (glowRenderer != null)
            {
                if (_propertyBlock == null) _propertyBlock = new MaterialPropertyBlock();
                Color emission = glowColor * (glowIntensity * (level + _pulse * 0.5f));
                emission.a = 1f;
                _propertyBlock.SetColor("_EmissionColor", emission);
                glowRenderer.SetPropertyBlock(_propertyBlock);
            }
        }

        /// <summary>Spectrum → 方块高度（从下往上长，所以位置也要跟着抬）。</summary>
        private void ApplyBars(float[] spectrum)
        {
            if (_bars == null || spectrum == null) return;
            if (_propertyBlock == null) _propertyBlock = new MaterialPropertyBlock();

            int count = _bars.Length;
            for (int i = 0; i < count; i++)
            {
                Transform bar = _bars[i];
                if (bar == null) continue;

                // 把第 i 根柱子映射到频谱的 (i + 0.5) / count 位置，取插值后的值。
                int bin = Mathf.Clamp((i * SimulationBins) / count, 0, SimulationBins - 1);
                float value = spectrum[bin];
                if (value < 0f) value = 0f;

                float height = Mathf.Max(value * barMaxHeight, 0.001f);
                Vector3 scale = _barBaseScale[i];
                scale.y = height;
                bar.localScale = scale;

                Vector3 position = bar.localPosition;
                position.y = height * 0.5f;
                bar.localPosition = position;

                if (_barRenderers[i] != null && _propertyBlock != null)
                {
                    // 顺手用亮度做个颜色变化，纯示例
                    _propertyBlock.SetColor("_EmissionColor", glowColor * (value * 2f));
                    _barRenderers[i].SetPropertyBlock(_propertyBlock);
                }
            }
        }

        /// <summary>Bpm → 匀速旋转（对转速做平滑，避免 BPM 抖动）。</summary>
        private void ApplyRotation(float bpm, float delta)
        {
            if (rotateTarget == null) return;

            float targetSpeed = bpm > 1f ? (bpm / 60f) * degreesPerBeat : 0f;
            float k = rotationSmoothing > 0.001f ? Mathf.Clamp01(delta / rotationSmoothing) : 1f;
            _rotationSpeed = Mathf.Lerp(_rotationSpeed, targetSpeed, k);
            if (Mathf.Abs(_rotationSpeed) > 0.001f)
            {
                rotateTarget.Rotate(0f, 0f, _rotationSpeed * delta);
            }
        }

        /// <summary>
        /// 指针跟随。PointerPosition / PointerNormalized 都是**宿主口径**（左上角为原点、Y 向下），
        /// 所以这里手动把 Y 翻过来变成 Unity 惯例（左下角原点）；等价的快捷属性是 link.PointerViewport。
        /// </summary>
        private void ApplyPointer(float delta)
        {
            if (pointerTarget == null) return;

            Vector2 normalized = new Vector2(0.5f, 0.5f);
            bool inside = false;
            bool leftDown = false;
            if (link != null && link.IsReceiving)
            {
                normalized = link.PointerNormalized;                 // 0–1，左上角原点
                inside = link.PointerInside;
                leftDown = link.PointerLeft;
            }

            // 左上角原点 → 左下角原点
            float x = (normalized.x - 0.5f) * 2f * pointerFollowRadius;
            float y = (1f - normalized.y - 0.5f) * 2f * pointerFollowRadius;
            float depth = pointerTarget.localPosition.z;
            Vector3 target = new Vector3(x, y, depth);

            float k = pointerFollowSpeed > 0.001f ? Mathf.Clamp01(pointerFollowSpeed * delta) : 1f;
            pointerTarget.localPosition = Vector3.Lerp(pointerTarget.localPosition, target, k);

            // 按下去的时候缩一点，证明按键状态也能拿到
            float press = leftDown ? 0.8f : 1f;
            float hover = inside ? 1f : 0.6f;
            pointerTarget.localScale = new Vector3(press, press, press) * hover;
        }

        /// <summary>内置模拟：120 BPM 的脉冲 + 噪声频谱，方便空场景直接看效果。</summary>
        private void Simulate(float delta, out float level, out float bpm)
        {
            _simTime += delta;

            float beatInterval = 0.5f; // 120 BPM
            if (_simTime >= _nextSimBeat)
            {
                _nextSimBeat = _simTime + beatInterval;
                _pulse = 1f;
            }

            level = 0.35f + 0.25f * Mathf.Sin(_simTime * 3.1f);
            bpm = 120f;

            float decay = Mathf.Clamp01(1f - (_simTime - (_nextSimBeat - beatInterval)) / beatInterval);
            for (int i = 0; i < SimulationBins; i++)
            {
                float t = (float)i / (SimulationBins - 1);
                float shaped = (1f - t) * 0.7f + 0.3f;
                float noise = Mathf.PerlinNoise(i * 0.15f, _simTime * 1.7f);
                _simSpectrum[i] = Mathf.Clamp01(shaped * (0.25f + 0.75f * noise) * (0.45f + 0.55f * decay));
            }
        }

        private void OnGUI()
        {
            if (!showOverlay) return;

            string text;
            if (link == null)
            {
                text = "MisideAudioLink: 未挂载（当前是内置模拟数据）";
            }
            else
            {
                text = "MisideAudioLink: " + (link.IsReceiving ? "接收中" : "无数据")
                    + "  包龄 " + (link.LastPacketAgeMs == int.MaxValue ? "-" : link.LastPacketAgeMs + "ms")
                    + "  收包 " + link.PacketsReceived
                    + "  丢弃 " + link.PacketsDropped
                    + "  丢弃(magic/版本/长度) " + link.PacketsDroppedBadMagic + "/" + link.PacketsDroppedBadVersion + "/" + link.PacketsDroppedTooShort
                    + "\nLevel " + link.Level.ToString("0.00")
                    + "  Bass " + link.Bass.ToString("0.00")
                    + "  BPM " + link.Bpm.ToString("0")
                    + "  帧 " + link.LastFrameSeq
                    + "  目标帧率 " + link.TargetFps
                    + "  渲染倍率 " + link.RenderScale.ToString("0.00")
                    + "  暂停 " + link.Paused
                    + "  语言 " + link.Language
                    + "\n屏幕 " + link.Screen.width + "x" + link.Screen.height + " @" + link.Screen.x + "," + link.Screen.y
                    + "  指针 " + link.PointerPosition.x.ToString("0") + "," + link.PointerPosition.y.ToString("0")
                    + "  inside=" + link.PointerInside;
                if (!string.IsNullOrEmpty(link.LastError)) text += "\n错误: " + link.LastError;
            }

            GUI.Label(new Rect(8f, 8f, 900f, 120f), text);
        }
    }
}
