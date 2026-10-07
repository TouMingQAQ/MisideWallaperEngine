// =============================================================================
//  MisideAudioLink.cs  —  miside-wallpaper-engine 桌面壁纸宿主 ⇄ Unity 音频节奏接收器
// -----------------------------------------------------------------------------
//  把这个文件（和 AudioLinkDemo.cs）丢进 Unity 工程的 Assets/ 任意目录即可。
//
//  宿主（Tauri + Rust）会做两件事：
//    1. 把用户的 Unity exe 挂到桌面 WorkerW 壁纸层；
//    2. 采集系统里某个进程的声音，算出 rms/peak/level/bass/mid/treble/频谱/波形/节拍/BPM，
//       通过本地 UDP（默认 127.0.0.1:47810）推给 Unity；
//       同时周期性下发「配置包」（目标帧率/静音/音量/暂停/语言/屏幕尺寸）
//       与「输入包」（鼠标在壁纸窗口客户区里的位置与按键）。
//
//  协议的唯一事实来源：miside-wallpaper-engine/docs/api-contract.md 第 5 节。
//  本文的常量与解析代码严格按该契约实现，字节序一律小端（little-endian）。
//
//  性能约定：
//    * 后台线程收包，主线程 Update() 里只做加锁快照拷贝；
//    * 帧包 / 输入包路径**零 GC 分配**（缓冲区全部预分配，JSON 走手写无分配扫描）；
//    * 配置包约 1Hz，解析语言字符串时会有一次很小的字符串分配，可忽略。
//
//  平台：全平台可用（纯 UDP，无平台相关 API）。
//  语言：C# 7.3 兼容（Unity 2019 可用），不使用 record / init / 顶层语句 等新语法。
// =============================================================================

using System;
using System.Net;
using System.Net.Sockets;
using System.Runtime.InteropServices;
using System.Text;
using System.Threading;
using UnityEngine;

namespace MisideWallpaper
{
    /// <summary>日志详细程度。</summary>
    public enum MisideLogLevel
    {
        /// <summary>完全不输出日志。</summary>
        None = 0,
        /// <summary>只输出错误。</summary>
        Error = 1,
        /// <summary>输出警告与错误（连接失败、版本不符等）。</summary>
        Warning = 2,
        /// <summary>额外输出连接成功、降级等提示，排查问题时建议用这一级。</summary>
        Info = 3,
        /// <summary>额外输出每个配置包的字段，最啰嗦。</summary>
        Verbose = 4
    }

    /// <summary>宿主下发的屏幕配置（像素，宿主坐标）。</summary>
    [Serializable]
    public struct ScreenConfig
    {
        /// <summary>壁纸窗口在桌面上的 X（像素）。</summary>
        public int x;
        /// <summary>壁纸窗口在桌面上的 Y（像素）。</summary>
        public int y;
        /// <summary>壁纸窗口宽度（像素）。</summary>
        public int width;
        /// <summary>壁纸窗口高度（像素）。</summary>
        public int height;
    }

    // =========================================================================
    //  纯协议核心：不引用任何 UnityEngine 类型，可脱离 Unity 单独编译与自测。
    //  （==BEGIN/END PURE PROTOCOL CORE== 之间不引用任何 UnityEngine 类型，
    //    想写单元测试可以把这一段单独抽出来编译，不需要 Unity 环境）
    // =========================================================================

    // ==BEGIN PURE PROTOCOL CORE==

    /// <summary>一帧音频数据的标量快照（频谱/波形在独立的预分配数组里）。</summary>
    public struct MisideFrameData
    {
        /// <summary>本次解析是否成功。</summary>
        public bool Valid;
        /// <summary>包序号（从 1 递增）。</summary>
        public uint Seq;
        /// <summary>宿主单调时钟毫秒。</summary>
        public ulong TimestampMs;
        /// <summary>采样率（通常 48000）。</summary>
        public uint SampleRate;
        /// <summary>0–1 均方根。</summary>
        public float Rms;
        /// <summary>0–1 峰值。</summary>
        public float Peak;
        /// <summary>0–1 平滑响度。</summary>
        public float Level;
        /// <summary>0 或 1，本帧是否命中节拍。</summary>
        public float Beat;
        /// <summary>0–1 低频能量。</summary>
        public float Bass;
        /// <summary>0–1 中频能量。</summary>
        public float Mid;
        /// <summary>0–1 高频能量。</summary>
        public float Treble;
        /// <summary>BPM，0 = 未估计出来。</summary>
        public float Bpm;
        /// <summary>Beat 是否算作命中（Beat &gt;= 0.5）。</summary>
        public bool HasBeat;
        /// <summary>宿主是否处于暂停（来自配置包的 paused 字段）。</summary>
        public bool Paused;
    }

    /// <summary>
    /// miside-wallpaper-engine 宿主 → Unity 的二进制协议解析器（纯逻辑，无 Unity 依赖）。
    /// 全部多字节整数/浮点均为小端。
    /// </summary>
    public static class MisideProtocol
    {
        /// <summary>协议版本，当前为 1。</summary>
        public const int Version = 1;

        /// <summary>帧包类型。</summary>
        public const int PacketTypeFrame = 1;
        /// <summary>配置包类型（JSON）。</summary>
        public const int PacketTypeConfig = 2;
        /// <summary>输入包类型（JSON）。</summary>
        public const int PacketTypeInput = 3;

        /// <summary>magic 长度（"MWAV"）。</summary>
        public const int MagicLength = 4;
        /// <summary>JSON 包的包头长度：magic(4)+version(1)+type(1)+reserved(2)+seq(4)+jsonLen(4)。</summary>
        public const int JsonHeaderSize = 16;
        /// <summary>帧包总长度：56 + 128(频谱) + 256(波形) = 440。</summary>
        public const int FramePacketSize = 440;
        /// <summary>频谱柱数。</summary>
        public const int SpectrumBins = 128;
        /// <summary>波形字节数：128 组 (min, max)。</summary>
        public const int WaveformBytes = 256;
        /// <summary>波形组数（每 2 个字节一组）。</summary>
        public const int WaveformPairs = 128;

        // ---- 帧包字段偏移 ----
        /// <summary>magic 偏移。</summary>
        public const int FrameOffMagic = 0;
        /// <summary>version 偏移。</summary>
        public const int FrameOffVersion = 4;
        /// <summary>type 偏移。</summary>
        public const int FrameOffType = 5;
        /// <summary>reserved 偏移。</summary>
        public const int FrameOffReserved = 6;
        /// <summary>seq 偏移。</summary>
        public const int FrameOffSeq = 8;
        /// <summary>timestampMs 偏移。</summary>
        public const int FrameOffTimestamp = 12;
        /// <summary>sampleRate 偏移。</summary>
        public const int FrameOffSampleRate = 20;
        /// <summary>rms 偏移。</summary>
        public const int FrameOffRms = 24;
        /// <summary>peak 偏移。</summary>
        public const int FrameOffPeak = 28;
        /// <summary>level 偏移。</summary>
        public const int FrameOffLevel = 32;
        /// <summary>beat 偏移。</summary>
        public const int FrameOffBeat = 36;
        /// <summary>bass 偏移。</summary>
        public const int FrameOffBass = 40;
        /// <summary>mid 偏移。</summary>
        public const int FrameOffMid = 44;
        /// <summary>treble 偏移。</summary>
        public const int FrameOffTreble = 48;
        /// <summary>bpm 偏移。</summary>
        public const int FrameOffBpm = 52;
        /// <summary>频谱偏移（128 字节，0–255）。</summary>
        public const int FrameOffSpectrum = 56;
        /// <summary>波形偏移（256 字节，i8，min/max 交替）。</summary>
        public const int FrameOffWaveform = 184;

        // ---- JSON 包头字段偏移 ----
        /// <summary>JSON 包里 jsonLen 的偏移。</summary>
        public const int JsonOffLength = 12;

        [StructLayout(LayoutKind.Explicit)]
        private struct FloatUnion
        {
            [FieldOffset(0)] public uint Bits;
            [FieldOffset(0)] public float Value;
        }

        /// <summary>校验 4 字节 magic 是否为 "MWAV"。</summary>
        public static bool HasMagic(byte[] buffer, int length)
        {
            return buffer != null
                && length >= MagicLength
                && buffer[0] == 0x4D && buffer[1] == 0x57 && buffer[2] == 0x41 && buffer[3] == 0x56;
        }

        /// <summary>读取小端 u32。</summary>
        public static uint ReadUInt32(byte[] buffer, int offset)
        {
            return (uint)(buffer[offset]
                | (buffer[offset + 1] << 8)
                | (buffer[offset + 2] << 16)
                | (buffer[offset + 3] << 24));
        }

        /// <summary>读取小端 u64。</summary>
        public static ulong ReadUInt64(byte[] buffer, int offset)
        {
            uint lo = ReadUInt32(buffer, offset);
            uint hi = ReadUInt32(buffer, offset + 4);
            return ((ulong)hi << 32) | lo;
        }

        /// <summary>读取小端 f32（bit 级重解释，不依赖 BitConverter 的平台字节序）。</summary>
        public static float ReadFloat32(byte[] buffer, int offset)
        {
            FloatUnion u = default(FloatUnion);
            u.Bits = ReadUInt32(buffer, offset);
            return u.Value;
        }

        /// <summary>把值钳制到 0–1。</summary>
        public static float Clamp01(float value)
        {
            if (value < 0f) return 0f;
            if (value > 1f) return 1f;
            return value;
        }

        /// <summary>渲染倍率下限（低于它 GPU 省不了多少，画质却已经明显糊了）。</summary>
        public const float MinRenderScale = 0.25f;

        /// <summary>渲染倍率上限（1.0 = 不降分辨率；宿主一般只发 0.5 / 0.75 / 1.0）。</summary>
        public const float MaxRenderScale = 1f;

        /// <summary>
        /// 把宿主下发的 renderScale 规范化，保证调用方永远拿到一个能用的值：
        /// 缺字段由调用方负责回退成 <see cref="MaxRenderScale"/>；这里处理数值本身——
        /// NaN / 无穷 / &lt;= 0 视为垃圾值 → 1.0；其余钳制到 0.25–1.0。
        /// </summary>
        public static float SanitizeRenderScale(float value)
        {
            if (float.IsNaN(value) || float.IsInfinity(value) || value <= 0f) return MaxRenderScale;
            if (value < MinRenderScale) return MinRenderScale;
            if (value > MaxRenderScale) return MaxRenderScale;
            return value;
        }

        private static void ReadScalars(byte[] buffer, int scalarBase, ref MisideFrameData frame)
        {
            frame.Rms = ReadFloat32(buffer, scalarBase + 0);
            frame.Peak = ReadFloat32(buffer, scalarBase + 4);
            frame.Level = ReadFloat32(buffer, scalarBase + 8);
            frame.Beat = ReadFloat32(buffer, scalarBase + 12);
            frame.Bass = ReadFloat32(buffer, scalarBase + 16);
            frame.Mid = ReadFloat32(buffer, scalarBase + 20);
            frame.Treble = ReadFloat32(buffer, scalarBase + 24);
            frame.Bpm = ReadFloat32(buffer, scalarBase + 28);
            frame.HasBeat = frame.Beat >= 0.5f;
        }

        /// <summary>
        /// 解析 UDP 帧包（type = 1）。
        /// 失败原因：长度不足 440 / magic 不符 / version 不符 / type 不符 / 目标数组太小。
        /// </summary>
        /// <param name="buffer">收到的数据报。</param>
        /// <param name="length">有效长度。</param>
        /// <param name="frame">解析结果（标量）。</param>
        /// <param name="spectrum">128 字节频谱输出，0–255 原样写入。</param>
        /// <param name="waveform">256 字节波形输出，i8 原样写入（偶=min，奇=max）。</param>
        public static bool TryParseFramePacket(byte[] buffer, int length, ref MisideFrameData frame, byte[] spectrum, sbyte[] waveform)
        {
            if (buffer == null || length < FramePacketSize) return false;
            if (spectrum == null || spectrum.Length < SpectrumBins) return false;
            if (waveform == null || waveform.Length < WaveformBytes) return false;
            if (!HasMagic(buffer, length)) return false;
            if (buffer[FrameOffVersion] != Version) return false;
            if (buffer[FrameOffType] != PacketTypeFrame) return false;

            frame.Valid = true;
            frame.Seq = ReadUInt32(buffer, FrameOffSeq);
            frame.TimestampMs = ReadUInt64(buffer, FrameOffTimestamp);
            frame.SampleRate = ReadUInt32(buffer, FrameOffSampleRate);
            ReadScalars(buffer, FrameOffRms, ref frame);
            frame.Paused = false;
            Buffer.BlockCopy(buffer, FrameOffSpectrum, spectrum, 0, SpectrumBins);
            Buffer.BlockCopy(buffer, FrameOffWaveform, waveform, 0, WaveformBytes);
            return true;
        }

        // ---------------------------------------------------------------------
        //  极简 JSON 扫描（无 GC）：只支持契约里出现的那几种形状
        //    { "a": 1, "b": 1.5, "c": true, "d": "文字", "e": { ... } }
        //  字段名匹配要求前后带引号，因此 "x" 不会误配 "max"。
        // ---------------------------------------------------------------------

        private static bool IsJsonWhitespace(byte c)
        {
            return c == (byte)' ' || c == (byte)'\t' || c == (byte)'\r' || c == (byte)'\n';
        }

        /// <summary>找到 "field" 之后冒号后面的第一个有效字节下标；找不到返回 -1。</summary>
        public static int FindJsonValueStart(byte[] json, int start, int length, string field)
        {
            if (json == null || field == null || field.Length == 0) return -1;
            if (start < 0 || length <= 0) return -1;
            int end = start + length;
            if (end > json.Length) end = json.Length;
            for (int i = start; i < end; i++)
            {
                if (json[i] != (byte)'"') continue;
                int k = i + 1;
                int j = 0;
                while (j < field.Length && k < end && json[k] == (byte)field[j]) { j++; k++; }
                if (j != field.Length) continue;
                if (k >= end || json[k] != (byte)'"') continue;
                k++;
                while (k < end && IsJsonWhitespace(json[k])) k++;
                if (k >= end || json[k] != (byte)':') continue;
                k++;
                while (k < end && IsJsonWhitespace(json[k])) k++;
                if (k >= end) return -1;
                return k;
            }
            return -1;
        }

        /// <summary>读取一个 JSON 对象字段的字节范围（含花括号）。</summary>
        public static bool TryGetJsonObjectRange(byte[] json, int start, int length, string field, out int objectStart, out int objectLength)
        {
            objectStart = 0;
            objectLength = 0;
            int i = FindJsonValueStart(json, start, length, field);
            if (i < 0 || json[i] != (byte)'{') return false;
            int end = start + length;
            if (end > json.Length) end = json.Length;
            int depth = 0;
            bool inString = false;
            for (int k = i; k < end; k++)
            {
                byte c = json[k];
                if (inString)
                {
                    if (c == (byte)'\\') { k++; continue; }
                    if (c == (byte)'"') inString = false;
                    continue;
                }
                if (c == (byte)'"') { inString = true; continue; }
                if (c == (byte)'{') depth++;
                else if (c == (byte)'}')
                {
                    depth--;
                    if (depth == 0)
                    {
                        objectStart = i;
                        objectLength = k - i + 1;
                        return true;
                    }
                }
            }
            return false;
        }

        /// <summary>读取 JSON 整数字段。</summary>
        public static bool TryGetJsonInt(byte[] json, int start, int length, string field, out int value)
        {
            value = 0;
            int i = FindJsonValueStart(json, start, length, field);
            if (i < 0) return false;
            int end = start + length;
            if (end > json.Length) end = json.Length;
            bool negative = false;
            if (json[i] == (byte)'-') { negative = true; i++; }
            if (i >= end || json[i] < (byte)'0' || json[i] > (byte)'9') return false;
            long acc = 0;
            while (i < end && json[i] >= (byte)'0' && json[i] <= (byte)'9')
            {
                acc = acc * 10 + (json[i] - (byte)'0');
                if (acc > int.MaxValue) acc = int.MaxValue;
                i++;
            }
            value = (int)(negative ? -acc : acc);
            return true;
        }

        /// <summary>读取 JSON 浮点字段（容错：整数、小数、科学计数法都吃）。</summary>
        public static bool TryGetJsonFloat(byte[] json, int start, int length, string field, out float value)
        {
            value = 0f;
            int i = FindJsonValueStart(json, start, length, field);
            if (i < 0) return false;
            int end = start + length;
            if (end > json.Length) end = json.Length;
            bool negative = false;
            if (json[i] == (byte)'-') { negative = true; i++; }
            double acc = 0.0;
            bool any = false;
            while (i < end && json[i] >= (byte)'0' && json[i] <= (byte)'9')
            {
                acc = acc * 10.0 + (json[i] - (byte)'0');
                any = true;
                i++;
            }
            if (i < end && json[i] == (byte)'.')
            {
                i++;
                double scale = 0.1;
                while (i < end && json[i] >= (byte)'0' && json[i] <= (byte)'9')
                {
                    acc += (json[i] - (byte)'0') * scale;
                    scale *= 0.1;
                    any = true;
                    i++;
                }
            }
            if (!any) return false;
            if (i < end && (json[i] == (byte)'e' || json[i] == (byte)'E'))
            {
                i++;
                bool expNegative = false;
                if (i < end && (json[i] == (byte)'+' || json[i] == (byte)'-'))
                {
                    expNegative = json[i] == (byte)'-';
                    i++;
                }
                int exp = 0;
                bool expAny = false;
                while (i < end && json[i] >= (byte)'0' && json[i] <= (byte)'9')
                {
                    exp = exp * 10 + (json[i] - (byte)'0');
                    if (exp > 308) exp = 308;
                    expAny = true;
                    i++;
                }
                if (expAny && exp != 0) acc *= Math.Pow(10.0, expNegative ? -exp : exp);
            }
            value = (float)(negative ? -acc : acc);
            return true;
        }

        /// <summary>读取 JSON 布尔字段。</summary>
        public static bool TryGetJsonBool(byte[] json, int start, int length, string field, out bool value)
        {
            value = false;
            int i = FindJsonValueStart(json, start, length, field);
            if (i < 0) return false;
            if (json[i] == (byte)'t') { value = true; return true; }
            if (json[i] == (byte)'f') { value = false; return true; }
            return false;
        }

        /// <summary>读取 JSON 字符串字段的**原始**字节范围（不含两侧引号，保留转义）。</summary>
        public static bool TryGetJsonStringRange(byte[] json, int start, int length, string field, out int valueStart, out int valueLength)
        {
            valueStart = 0;
            valueLength = 0;
            int i = FindJsonValueStart(json, start, length, field);
            if (i < 0 || json[i] != (byte)'"') return false;
            int end = start + length;
            if (end > json.Length) end = json.Length;
            int k = i + 1;
            while (k < end && json[k] != (byte)'"')
            {
                if (json[k] == (byte)'\\') k++;
                k++;
            }
            if (k >= end) return false;
            valueStart = i + 1;
            valueLength = k - i - 1;
            return true;
        }

        /// <summary>读取并解码 JSON 字符串字段；字段缺失时返回 fallback。</summary>
        public static string GetJsonString(byte[] json, int start, int length, string field, string fallback)
        {
            int s;
            int l;
            if (!TryGetJsonStringRange(json, start, length, field, out s, out l)) return fallback;
            return DecodeJsonString(json, s, l);
        }

        /// <summary>把 JSON 字符串的原始字节解码成 string（处理常见转义）。</summary>
        public static string DecodeJsonString(byte[] json, int start, int length)
        {
            if (json == null || length <= 0) return string.Empty;
            bool escaped = false;
            for (int i = 0; i < length; i++)
            {
                if (json[start + i] == (byte)'\\') { escaped = true; break; }
            }
            if (!escaped) return Encoding.UTF8.GetString(json, start, length);

            StringBuilder sb = new StringBuilder(length);
            int end = start + length;
            for (int i = start; i < end; i++)
            {
                byte c = json[i];
                if (c != (byte)'\\' || i + 1 >= end)
                {
                    sb.Append((char)c);
                    continue;
                }
                i++;
                byte e = json[i];
                switch (e)
                {
                    case (byte)'"': sb.Append('"'); break;
                    case (byte)'\\': sb.Append('\\'); break;
                    case (byte)'/': sb.Append('/'); break;
                    case (byte)'b': sb.Append('\b'); break;
                    case (byte)'f': sb.Append('\f'); break;
                    case (byte)'n': sb.Append('\n'); break;
                    case (byte)'r': sb.Append('\r'); break;
                    case (byte)'t': sb.Append('\t'); break;
                    case (byte)'u':
                        {
                            int code = 0;
                            for (int d = 0; d < 4 && i + 1 < end; d++)
                            {
                                i++;
                                byte h = json[i];
                                int v;
                                if (h >= (byte)'0' && h <= (byte)'9') v = h - (byte)'0';
                                else if (h >= (byte)'a' && h <= (byte)'f') v = h - (byte)'a' + 10;
                                else if (h >= (byte)'A' && h <= (byte)'F') v = h - (byte)'A' + 10;
                                else { v = 0; }
                                code = (code << 4) | v;
                            }
                            sb.Append((char)code);
                            break;
                        }
                    default: sb.Append((char)e); break;
                }
            }
            return sb.ToString();
        }

        /// <summary>
        /// 校验并定位 JSON 包（配置包 / 输入包）的 JSON 载荷。
        /// 长度不足、magic 不符、版本不符、type 不符、jsonLen 越界都返回 false。
        /// </summary>
        public static bool TryParseJsonHeader(byte[] buffer, int length, int expectedType, out int jsonStart, out int jsonLength)
        {
            jsonStart = 0;
            jsonLength = 0;
            if (buffer == null || length < JsonHeaderSize) return false;
            if (!HasMagic(buffer, length)) return false;
            if (buffer[FrameOffVersion] != Version) return false;
            if (buffer[FrameOffType] != expectedType) return false;
            uint declared = ReadUInt32(buffer, JsonOffLength);
            if (declared == 0u) return false;
            if (declared > (uint)(length - JsonHeaderSize)) return false;
            jsonStart = JsonHeaderSize;
            jsonLength = (int)declared;
            return true;
        }
    }

    // ==END PURE PROTOCOL CORE==

    /// <summary>
    /// 接收 MisideWallpaper 宿主推送的音频节奏 / 配置 / 输入数据。
    /// 直接把这个组件挂到场景里任意 GameObject 上即可，不需要任何子物体。
    /// </summary>
    /// <remarks>
    /// 常见用法：
    /// <code>
    /// var link = GetComponent&lt;MisideAudioLink&gt;();
    /// transform.localScale = Vector3.one * (1f + link.Level * 0.2f);
    /// foreach (var band in link.Spectrum) { ... }
    /// link.OnBeat += strength =&gt; { /* 一次性脉冲 */ };
    /// </code>
    /// </remarks>
    [AddComponentMenu("MisideWallpaper/Miside Audio Link (音频节奏接收器)")]
    [DisallowMultipleComponent]
    public sealed class MisideAudioLink : MonoBehaviour
    {
        // ---------------------------------------------------------------------
        //  Inspector
        // ---------------------------------------------------------------------

        /// <summary>是否接收 UDP 推送（默认开启）。</summary>
        [Header("UDP 传输")]
        [Tooltip("接收宿主通过 UDP 推送的帧包 / 配置包 / 输入包。")]
        public bool enableUdp = true;

        /// <summary>UDP 监听端口，需与宿主设置里的 udpPort 一致。</summary>
        [Tooltip("UDP 监听端口，必须与宿主设置里的「UDP 端口」一致（默认 47810）。")]
        public int udpPort = 47810;

        /// <summary>日志级别。</summary>
        [Header("诊断")]
        [Tooltip("排查问题时选 Info 或 Verbose；正式发布建议 Warning 或 None。")]
        public MisideLogLevel logLevel = MisideLogLevel.Info;

        /// <summary>断流判定时间（毫秒）。</summary>
        [Tooltip("超过这个时间没有收到任何包，IsReceiving 变为 false（毫秒）。")]
        public float staleTimeoutMs = 1000f;

        /// <summary>是否启用指数平滑。</summary>
        [Header("平滑（抑制 UDP 抖动）")]
        [Tooltip("开启后用与帧率无关的指数平滑，避免包到达抖动导致数值乱跳。")]
        public bool smoothing = true;

        /// <summary>电平类数值的平滑半衰期（秒）。</summary>
        [Tooltip("rms/peak/level/bass/mid/treble/bpm 的半衰期（秒），越小越跟手、越大越平滑。")]
        public float levelSmoothingHalfLife = 0.06f;

        /// <summary>频谱与波形的平滑半衰期（秒）。</summary>
        [Tooltip("频谱柱与波形包络的半衰期（秒）。")]
        public float spectrumSmoothingHalfLife = 0.05f;

        /// <summary>是否把宿主下发的目标帧率写进 Application.targetFrameRate。</summary>
        [Header("行为")]
        [Tooltip("把配置包里的 targetFps 应用到 Application.targetFrameRate。")]
        public bool applyTargetFramerate = true;

        /// <summary>暂停时是否暂停 AudioListener。</summary>
        [Tooltip("宿主暂停壁纸时同步设置 AudioListener.pause。默认关闭，由用户自己决定。")]
        public bool pauseAudioListenerWhenPaused = false;

        /// <summary>是否把宿主下发的渲染倍率落到 ScalableBufferManager。</summary>
        [Tooltip("把配置包里的 renderScale 应用到 ScalableBufferManager.ResizeBuffers："
            + "窗口照旧铺满显示器，只降 Unity 的内部分辨率（省 GPU）。"
            + "壁纸工程自己管分辨率、或者要用渲染管线自带的 Render Scale 时，把它关掉。")]
        public bool applyRenderScale = true;

        /// <summary>空闲时的轮询间隔（毫秒）。</summary>
        [Header("高级")]
        [Range(0, 20)]
        [Tooltip("接收线程空闲时的轮询间隔（毫秒）。0 = 忙等（最跟手但吃一个核）。")]
        public int receivePollIntervalMs = 1;

        /// <summary>输入包过期时间（毫秒）。</summary>
        [Tooltip("超过这个时间没有新的输入包，就把指针状态视为离开（毫秒）。")]
        public float inputStaleTimeoutMs = 250f;

        /// <summary>节拍脉冲的衰减时间（秒）。</summary>
        [Tooltip("BeatPulse 从 1 衰减到 0 所需时间（秒）。")]
        public float beatPulseDecay = 0.35f;

        // ---------------------------------------------------------------------
        //  内部数据类型
        // ---------------------------------------------------------------------

        private struct ConfigData
        {
            public bool Valid;
            public int TargetFps;
            public float RenderScale;
            public bool Muted;
            public float Volume;
            public bool Paused;
            public string Language;
            public float AudioGain;
            public int SourcePid;
            public string SourceProcess;
            public ScreenConfig Screen;
        }

        private struct InputData
        {
            public bool Valid;
            public float X;
            public float Y;
            public float ClientWidth;
            public float ClientHeight;
            public bool Inside;
            public bool Left;
            public bool Right;
            public bool Middle;
            public float WheelAccum;
            public bool Locked;
        }

        /// <summary>
        /// 接收线程写入槽。用 3 个槽轮转：接收线程只写「下一个」槽，
        /// 主线程拿着「已发布」槽的引用做拷贝，一个槽被覆写至少要再过 2 次发布，
        /// 因此主线程的拷贝（几百字节）不可能读到正在被写的内存。
        /// </summary>
        private sealed class RxSlot
        {
            public byte[] Spectrum = new byte[MisideProtocol.SpectrumBins];
            public sbyte[] Waveform = new sbyte[MisideProtocol.WaveformBytes];
            public MisideFrameData Frame;
        }

        private const int RxSlotCount = 3;

        // ---------------------------------------------------------------------
        //  接收线程侧状态
        // ---------------------------------------------------------------------

        private readonly object _sync = new object();
        private readonly RxSlot[] _slots = new RxSlot[] { new RxSlot(), new RxSlot(), new RxSlot() };

        private int _publishedIndex = -1;
        private int _writeSlotIndex = RxSlotCount - 1;

        private ConfigData _config;
        private int _configVersion;
        private InputData _input;

        private int _pendingBeatCount;
        private float _pendingBeatStrength;

        private Thread _rxThread;
        private volatile bool _running;
        private UdpClient _udp;
        private int _udpRetryAt;
        private volatile bool _udpOpen;

        private byte[] _rxBuffer;
        private EndPoint _rxEndPoint;

        private long _packetsReceived;
        private int _lastPacketTick;
        private int _lastInputTick;
        private long _droppedBadMagic;
        private long _droppedBadVersion;
        private long _droppedTooShort;
        private long _droppedOther;

        private volatile string _threadError;

        // ---------------------------------------------------------------------
        //  主线程侧状态（全是预分配数组与值类型，Update 里不产生 GC）
        // ---------------------------------------------------------------------

        private readonly byte[] _stagingSpectrum = new byte[MisideProtocol.SpectrumBins];
        private readonly sbyte[] _stagingWaveform = new sbyte[MisideProtocol.WaveformBytes];
        private readonly float[] _spectrum = new float[MisideProtocol.SpectrumBins];
        private readonly float[] _waveformMin = new float[MisideProtocol.WaveformPairs];
        private readonly float[] _waveformMax = new float[MisideProtocol.WaveformPairs];

        private float _rms;
        private float _peak;
        private float _level;
        private float _bass;
        private float _mid;
        private float _treble;
        private float _bpm;
        private float _framePausedFlag;
        private uint _sampleRate;
        private uint _lastFrameSeq;

        private float _beatStrength;
        private float _beatPulse;
        private float _lastBeatTime;
        private bool _hasBeatEver;

        private int _appliedConfigVersion = -1;
        private ConfigData _appliedConfig;

        /// <summary>已经真正下发到 ScalableBufferManager 的倍率；1.0 表示当前是全分辨率。</summary>
        private float _renderScaleApplied = MisideProtocol.MaxRenderScale;

        private float _pointerX;
        private float _pointerY;
        private float _clientWidth;
        private float _clientHeight;
        private bool _pointerInside;
        private bool _pointerLeft;
        private bool _pointerRight;
        private bool _pointerMiddle;
        private float _pointerWheel;
        private bool _pointerLocked;

        private bool _loggedUdpOpen;
        private string _lastError = string.Empty;
        private int _lastPacketAgeMs = int.MaxValue;

        // ---------------------------------------------------------------------
        //  公开 API —— 数值
        // ---------------------------------------------------------------------

        /// <summary>0–1 均方根（含平滑）。</summary>
        public float Rms { get { return _rms; } }

        /// <summary>0–1 峰值（含平滑）。</summary>
        public float Peak { get { return _peak; } }

        /// <summary>0–1 平滑响度，最常用的整体强度。</summary>
        public float Level { get { return _level; } }

        /// <summary>0–1 低频能量（低音鼓 / 贝斯）。</summary>
        public float Bass { get { return _bass; } }

        /// <summary>0–1 中频能量。</summary>
        public float Mid { get { return _mid; } }

        /// <summary>0–1 高频能量（镲片 / 齿音）。</summary>
        public float Treble { get { return _treble; } }

        /// <summary>BPM，0 = 宿主还没估计出来。</summary>
        public float Bpm { get { return _bpm; } }

        /// <summary>当前采样率（帧包里的 sampleRate）。</summary>
        public uint SampleRate { get { return _sampleRate; } }

        /// <summary>最近处理过的帧序号，可用于判断数据是否在推进。</summary>
        public uint LastFrameSeq { get { return _lastFrameSeq; } }

        // ---------------------------------------------------------------------
        //  公开 API —— 频谱 / 波形
        // ---------------------------------------------------------------------

        /// <summary>
        /// 128 根对数频率柱，0–1（原协议是 0–255，这里已归一化）。
        /// 直接返回内部数组，**不要修改它**，只读使用；需要边界安全就用 <see cref="GetBand"/>。
        /// </summary>
        public float[] Spectrum { get { return _spectrum; } }

        /// <summary>取第 <paramref name="index"/> 根频谱柱（0–1），越界自动钳制。</summary>
        public float GetBand(int index)
        {
            if (index < 0) index = 0;
            if (index >= MisideProtocol.SpectrumBins) index = MisideProtocol.SpectrumBins - 1;
            return _spectrum[index];
        }

        /// <summary>
        /// 按归一化位置取样（<paramref name="normalizedIndex"/> 0–1，0 = 最低频），
        /// 在相邻两根柱之间线性插值，方便把 N 根可视化柱子均匀铺到 128 根频谱上。
        /// </summary>
        public float GetBandAt(float normalizedIndex)
        {
            if (normalizedIndex <= 0f) return _spectrum[0];
            if (normalizedIndex >= 1f) return _spectrum[MisideProtocol.SpectrumBins - 1];
            float f = normalizedIndex * (MisideProtocol.SpectrumBins - 1);
            int i = (int)f;
            float t = f - i;
            return _spectrum[i] + (_spectrum[i + 1] - _spectrum[i]) * t;
        }

        /// <summary>128 组波形的最大值包络，-1–1。直接返回内部数组，只读使用。</summary>
        public float[] WaveformMax { get { return _waveformMax; } }

        /// <summary>128 组波形的最小值包络，-1–1。直接返回内部数组，只读使用。</summary>
        public float[] WaveformMin { get { return _waveformMin; } }

        /// <summary>取第 <paramref name="index"/> 组波形的 min/max 中点（-1–1）。</summary>
        public float GetWaveform(int index)
        {
            if (index < 0) index = 0;
            if (index >= MisideProtocol.WaveformPairs) index = MisideProtocol.WaveformPairs - 1;
            return (_waveformMax[index] + _waveformMin[index]) * 0.5f;
        }

        /// <summary>取第 <paramref name="index"/> 组波形的 min/max 包络（-1–1）。</summary>
        public void GetWaveformRange(int index, out float min, out float max)
        {
            if (index < 0) index = 0;
            if (index >= MisideProtocol.WaveformPairs) index = MisideProtocol.WaveformPairs - 1;
            min = _waveformMin[index];
            max = _waveformMax[index];
        }

        // ---------------------------------------------------------------------
        //  公开 API —— 节拍
        // ---------------------------------------------------------------------

        /// <summary>本次 Update 之前是否收到了新的节拍（一帧最多触发一次）。</summary>
        public bool BeatThisFrame { get; private set; }

        /// <summary>最近一次节拍的 <see cref="Time.time"/>；从未有过节拍时为 <c>float.NegativeInfinity</c>。</summary>
        public float LastBeatTime { get { return _hasBeatEver ? _lastBeatTime : float.NegativeInfinity; } }

        /// <summary>距上次节拍的秒数；从未有过节拍时为 <c>float.PositiveInfinity</c>。</summary>
        public float TimeSinceBeat { get { return _hasBeatEver ? (Time.time - _lastBeatTime) : float.PositiveInfinity; } }

        /// <summary>最近一次节拍的强度（0–1，不衰减）。</summary>
        public float BeatStrength { get { return _beatStrength; } }

        /// <summary>节拍脉冲包络：命中瞬间为 1，随后在 <see cref="beatPulseDecay"/> 秒内线性衰减到 0。</summary>
        public float BeatPulse { get { return _beatPulse; } }

        /// <summary>命中节拍时在主线程回调，参数为强度（0–1）。</summary>
        public event Action<float> OnBeat;

        // ---------------------------------------------------------------------
        //  公开 API —— 配置
        // ---------------------------------------------------------------------

        /// <summary>宿主下发的目标帧率，0 = 不限。</summary>
        public int TargetFps { get { return _appliedConfig.TargetFps; } }

        /// <summary>
        /// 宿主下发的渲染倍率（0.25–1.0，正常只有 0.5 / 0.75 / 1.0）。
        /// 1.0 = 不降分辨率；还没收到配置包时也返回 1.0。
        /// 实际落地由 <see cref="applyRenderScale"/> 控制。
        /// </summary>
        public float RenderScale { get { return _appliedConfig.Valid ? _appliedConfig.RenderScale : MisideProtocol.MaxRenderScale; } }

        /// <summary>宿主是否把壁纸进程静音。</summary>
        public bool Muted { get { return _appliedConfig.Muted; } }

        /// <summary>宿主设置的壁纸音量 0–1。</summary>
        public float Volume { get { return _appliedConfig.Volume; } }

        /// <summary>宿主是否暂停壁纸（进程挂起 / 配置包 paused）。</summary>
        public bool Paused { get { return _appliedConfig.Paused || _framePausedFlag > 0.5f; } }

        /// <summary>宿主当前语言，如 <c>zh-CN</c> / <c>en-US</c>。</summary>
        public string Language { get { return _appliedConfig.Language ?? string.Empty; } }

        /// <summary>宿主下发的壁纸窗口屏幕位置与尺寸（像素）。</summary>
        public ScreenConfig Screen { get { return _appliedConfig.Screen; } }

        /// <summary>宿主采集增益（0.1–4），可用于自己再缩放一遍。</summary>
        public float AudioGain { get { return _appliedConfig.AudioGain; } }

        /// <summary>宿主当前采集的进程名（可能为空）。</summary>
        public string SourceProcess { get { return _appliedConfig.SourceProcess ?? string.Empty; } }

        /// <summary>收到的配置包与上一次不同时在主线程回调。</summary>
        public event Action OnConfigChanged;

        // ---------------------------------------------------------------------
        //  公开 API —— 输入（与 Unity 输入系统无关，宿主直接推给你）
        // ---------------------------------------------------------------------

        /// <summary>指针位置，**宿主口径**：壁纸窗口客户区像素，左上角为原点。</summary>
        public Vector2 PointerPosition { get { return new Vector2(_pointerX, _pointerY); } }

        /// <summary>指针位置归一化，0–1，仍是**宿主口径**（左上角为原点，y 向下）。</summary>
        public Vector2 PointerNormalized
        {
            get
            {
                float w = _clientWidth > 1f ? _clientWidth : 1f;
                float h = _clientHeight > 1f ? _clientHeight : 1f;
                return new Vector2(Mathf.Clamp01(_pointerX / w), Mathf.Clamp01(_pointerY / h));
            }
        }

        /// <summary>
        /// 指针位置归一化，已翻成 Unity 惯例（左下角为原点，y 向上），可直接喂给
        /// <c>Camera.ViewportToWorldPoint</c>。
        /// </summary>
        public Vector2 PointerViewport
        {
            get
            {
                Vector2 n = PointerNormalized;
                return new Vector2(n.x, 1f - n.y);
            }
        }

        /// <summary>指针是否在壁纸窗口客户区内。</summary>
        public bool PointerInside { get { return _pointerInside; } }

        /// <summary>左键是否按下。</summary>
        public bool PointerLeft { get { return _pointerLeft; } }

        /// <summary>右键是否按下。</summary>
        public bool PointerRight { get { return _pointerRight; } }

        /// <summary>中键是否按下。</summary>
        public bool PointerMiddle { get { return _pointerMiddle; } }

        /// <summary>本帧累计的滚轮增量（收到几个输入包就把它们的 wheel 相加）。</summary>
        public float PointerWheelDelta { get { return _pointerWheel; } }

        /// <summary>宿主是否因为“锁定输入”而不再转发真实输入。</summary>
        public bool PointerLocked { get { return _pointerLocked; } }

        // ---------------------------------------------------------------------
        //  公开 API —— 状态
        // ---------------------------------------------------------------------

        /// <summary>是否正在收到数据（在 <see cref="staleTimeoutMs"/> 内收到过包）。</summary>
        public bool IsReceiving { get; private set; }

        /// <summary>距上一个包的毫秒数；从未收到过任何包时为 <c>int.MaxValue</c>（第一个 Update 之前也是这个值）。</summary>
        public int LastPacketAgeMs { get { return _lastPacketAgeMs; } }

        /// <summary>累计收到的有效包数。</summary>
        public long PacketsReceived { get { return Interlocked.Read(ref _packetsReceived); } }

        /// <summary>被丢弃的包总数（magic / 版本 / 长度 / 其它）。</summary>
        public long PacketsDropped { get { return Interlocked.Read(ref _droppedBadMagic) + Interlocked.Read(ref _droppedBadVersion) + Interlocked.Read(ref _droppedTooShort) + Interlocked.Read(ref _droppedOther); } }

        /// <summary>因 magic 不是 "MWAV" 被丢弃的包数（通常是端口被别的程序占了）。</summary>
        public long PacketsDroppedBadMagic { get { return Interlocked.Read(ref _droppedBadMagic); } }

        /// <summary>因协议版本不符被丢弃的包数。</summary>
        public long PacketsDroppedBadVersion { get { return Interlocked.Read(ref _droppedBadVersion); } }

        /// <summary>因长度不足被丢弃的包数。</summary>
        public long PacketsDroppedTooShort { get { return Interlocked.Read(ref _droppedTooShort); } }

        /// <summary>最近一次错误文本，空串 = 没出错。</summary>
        public string LastError { get { return _lastError; } }

        /// <summary>UDP 是否已成功绑定端口（运行时可判断，用于自检）。</summary>
        public bool UdpBound { get { return _udpOpen; } }

        /// <summary>关闭并重新打开传输通道（改过端口或想重试时调用）。</summary>
        public void Reconnect()
        {
            StopLink();
            _lastError = string.Empty;
            _threadError = null;
            _loggedUdpOpen = false;
            _appliedConfigVersion = -1;
            lock (_sync)
            {
                _publishedIndex = -1;
                _writeSlotIndex = RxSlotCount - 1;
                _config = default(ConfigData);
                _configVersion = 0;
                _input = default(InputData);
                _pendingBeatCount = 0;
                _pendingBeatStrength = 0f;
            }
            Interlocked.Exchange(ref _packetsReceived, 0);
            Interlocked.Exchange(ref _droppedBadMagic, 0);
            Interlocked.Exchange(ref _droppedBadVersion, 0);
            Interlocked.Exchange(ref _droppedTooShort, 0);
            Interlocked.Exchange(ref _droppedOther, 0);
            if (isActiveAndEnabled) StartLink();
        }

        // ---------------------------------------------------------------------
        //  生命周期
        // ---------------------------------------------------------------------

        private void Awake()
        {
            if (_rxBuffer == null) _rxBuffer = new byte[4096];
            if (_rxEndPoint == null) _rxEndPoint = new IPEndPoint(IPAddress.Any, 0);
        }

        private void OnEnable()
        {
            Awake();
            StartLink();
        }

        private void OnDisable()
        {
            StopLink();
        }

        private void OnDestroy()
        {
            StopLink();
            OnBeat = null;
            OnConfigChanged = null;
        }

        private void StartLink()
        {
            if (_running) return;
            _running = true;
            _udpOpen = false;
            _threadError = null;
            _udpRetryAt = 0;
            Thread thread = new Thread(ReceiveLoop);
            thread.IsBackground = true;
            thread.Name = "MisideAudioLink RX";
            _rxThread = thread;
            thread.Start();
        }

        private void StopLink()
        {
            if (!_running && _rxThread == null) return;
            _running = false;

            UdpClient udp = _udp;
            _udp = null;
            if (udp != null)
            {
                // 关掉 socket 让可能阻塞在 ReceiveFrom 上的线程立刻返回。
                try { udp.Close(); }
                catch (Exception) { }
            }

            Thread thread = _rxThread;
            _rxThread = null;
            if (thread != null && thread.IsAlive)
            {
                try { thread.Join(250); }
                catch (ThreadStateException) { }
                catch (ThreadInterruptedException) { }
            }

            _udpOpen = false;
        }

        // ---------------------------------------------------------------------
        //  接收线程
        // ---------------------------------------------------------------------

        private void ReceiveLoop()
        {
            int sleepMs = receivePollIntervalMs;
            if (sleepMs < 0) sleepMs = 0;
            if (sleepMs > 20) sleepMs = 20;

            while (_running)
            {
                bool didWork = false;

                if (enableUdp)
                {
                    if (_udp == null) TryOpenUdp();
                    UdpClient udp = _udp;
                    if (udp != null)
                    {
                        try
                        {
                            while (_running && udp.Available > 0)
                            {
                                int count = ReceiveDatagram(udp);
                                if (count <= 0) break;
                                HandleDatagram(_rxBuffer, count);
                                didWork = true;
                            }
                        }
                        catch (ObjectDisposedException)
                        {
                            break; // socket 被主线程关掉了
                        }
                        catch (SocketException ex)
                        {
                            if (ex.SocketErrorCode == SocketError.MessageSize)
                            {
                                // 数据报比缓冲区大：丢掉，但不要打断循环
                                Interlocked.Increment(ref _droppedOther);
                            }
                            else if (ex.SocketErrorCode == SocketError.TimedOut)
                            {
                                // 空闲超时，正常现象
                            }
                            else if (ex.SocketErrorCode == SocketError.Interrupted || ex.SocketErrorCode == SocketError.OperationAborted)
                            {
                                break;
                            }
                            else
                            {
                                ReportThreadError("UDP 接收失败：" + ex.SocketErrorCode + "（" + ex.Message + "）");
                            }
                        }
                        catch (InvalidOperationException)
                        {
                            // socket 已关闭
                        }
                    }
                }
                else
                {
                    CloseUdp();
                }

                if (!didWork && sleepMs > 0) Thread.Sleep(sleepMs);
            }

            CloseUdp();
            _udpOpen = false;
        }

        private void TryOpenUdp()
        {
            int now = Environment.TickCount;
            if (unchecked(now - _udpRetryAt) < 0) return; // 退避中

            try
            {
                UdpClient udp = new UdpClient(udpPort);
                udp.Client.ReceiveTimeout = 200; // 万一 Available 误报，最多阻塞 200ms
                try { udp.Client.ReceiveBufferSize = 1 << 16; }
                catch (SocketException) { }
                _udp = udp;
                _udpOpen = true;
                _udpRetryAt = 0;
            }
            catch (SocketException ex)
            {
                _udpOpen = false;
                _udpRetryAt = unchecked(now + 2000);
                ReportThreadError("UDP 端口 " + udpPort + " 绑定失败：" + ex.SocketErrorCode
                    + "。可能被其它程序（或另一个壁纸实例）占用，改端口或在宿主里换一个 udpPort。");
            }
            catch (Exception ex)
            {
                _udpOpen = false;
                _udpRetryAt = unchecked(now + 2000);
                ReportThreadError("UDP 端口 " + udpPort + " 绑定失败：" + ex.Message);
            }
        }

        private void CloseUdp()
        {
            UdpClient udp = _udp;
            _udp = null;
            if (udp != null)
            {
                try { udp.Close(); }
                catch (Exception) { }
            }
            _udpOpen = false;
        }

        /// <summary>
        /// 用底层 Socket 把数据报读进预分配缓冲区（UdpClient.Receive 会每包 new 一个 byte[]，
        /// 60Hz 下就是持续 GC，所以这里用 UdpClient.Client.ReceiveFrom）。
        /// </summary>
        private int ReceiveDatagram(UdpClient udp)
        {
            EndPoint endPoint = _rxEndPoint;
            return udp.Client.ReceiveFrom(_rxBuffer, 0, _rxBuffer.Length, SocketFlags.None, ref endPoint);
        }

        private void HandleDatagram(byte[] buffer, int length)
        {
            if (length < 6)
            {
                Interlocked.Increment(ref _droppedTooShort);
                return;
            }
            if (!MisideProtocol.HasMagic(buffer, length))
            {
                Interlocked.Increment(ref _droppedBadMagic);
                return;
            }
            if (buffer[MisideProtocol.FrameOffVersion] != MisideProtocol.Version)
            {
                Interlocked.Increment(ref _droppedBadVersion);
                return;
            }

            byte type = buffer[MisideProtocol.FrameOffType];
            if (type == MisideProtocol.PacketTypeFrame)
            {
                HandleFramePacket(buffer, length);
            }
            else if (type == MisideProtocol.PacketTypeConfig)
            {
                HandleConfigPacket(buffer, length);
            }
            else if (type == MisideProtocol.PacketTypeInput)
            {
                HandleInputPacket(buffer, length);
            }
            else
            {
                Interlocked.Increment(ref _droppedOther);
            }
        }

        private void HandleFramePacket(byte[] buffer, int length)
        {
            if (length < MisideProtocol.FramePacketSize)
            {
                Interlocked.Increment(ref _droppedTooShort);
                return;
            }

            // 轮转到下一个写入槽；同一帧内收到多个帧包时，只有最后一个会被发布。
            _writeSlotIndex = (_writeSlotIndex + 1) % RxSlotCount;
            RxSlot slot = _slots[_writeSlotIndex];
            MisideFrameData frame = default(MisideFrameData);
            if (!MisideProtocol.TryParseFramePacket(buffer, length, ref frame, slot.Spectrum, slot.Waveform))
            {
                Interlocked.Increment(ref _droppedOther);
                return;
            }
            slot.Frame = frame;
            lock (_sync) { _publishedIndex = _writeSlotIndex; }

            TouchPacket();
            LatchBeat(frame);
        }

        private void HandleConfigPacket(byte[] buffer, int length)
        {
            int jsonStart;
            int jsonLength;
            if (!MisideProtocol.TryParseJsonHeader(buffer, length, MisideProtocol.PacketTypeConfig, out jsonStart, out jsonLength))
            {
                if (length < MisideProtocol.JsonHeaderSize) Interlocked.Increment(ref _droppedTooShort);
                else Interlocked.Increment(ref _droppedOther);
                return;
            }

            ConfigData config = _config; // 缺字段时保留上一次的值
            int intValue;
            float floatValue;
            bool boolValue;

            if (MisideProtocol.TryGetJsonInt(buffer, jsonStart, jsonLength, "targetFps", out intValue)) config.TargetFps = intValue;
            // renderScale：缺字段（老版本宿主）或数值非法时一律回退到 1.0 = 不降分辨率，
            // 绝不把 0/NaN 这种值传到 ScalableBufferManager 去。
            if (MisideProtocol.TryGetJsonFloat(buffer, jsonStart, jsonLength, "renderScale", out floatValue))
                config.RenderScale = MisideProtocol.SanitizeRenderScale(floatValue);
            else
                config.RenderScale = MisideProtocol.MaxRenderScale;
            if (MisideProtocol.TryGetJsonBool(buffer, jsonStart, jsonLength, "muted", out boolValue)) config.Muted = boolValue;
            if (MisideProtocol.TryGetJsonFloat(buffer, jsonStart, jsonLength, "volume", out floatValue)) config.Volume = MisideProtocol.Clamp01(floatValue);
            if (MisideProtocol.TryGetJsonBool(buffer, jsonStart, jsonLength, "paused", out boolValue)) config.Paused = boolValue;
            if (MisideProtocol.TryGetJsonFloat(buffer, jsonStart, jsonLength, "audioGain", out floatValue)) config.AudioGain = floatValue;
            string text = MisideProtocol.GetJsonString(buffer, jsonStart, jsonLength, "language", null);
            if (!string.IsNullOrEmpty(text)) config.Language = text;

            int objectStart;
            int objectLength;
            if (MisideProtocol.TryGetJsonObjectRange(buffer, jsonStart, jsonLength, "screen", out objectStart, out objectLength))
            {
                if (MisideProtocol.TryGetJsonInt(buffer, objectStart, objectLength, "x", out intValue)) config.Screen.x = intValue;
                if (MisideProtocol.TryGetJsonInt(buffer, objectStart, objectLength, "y", out intValue)) config.Screen.y = intValue;
                if (MisideProtocol.TryGetJsonInt(buffer, objectStart, objectLength, "width", out intValue)) config.Screen.width = intValue;
                if (MisideProtocol.TryGetJsonInt(buffer, objectStart, objectLength, "height", out intValue)) config.Screen.height = intValue;
            }
            if (MisideProtocol.TryGetJsonObjectRange(buffer, jsonStart, jsonLength, "source", out objectStart, out objectLength))
            {
                if (MisideProtocol.TryGetJsonInt(buffer, objectStart, objectLength, "pid", out intValue)) config.SourcePid = intValue;
                text = MisideProtocol.GetJsonString(buffer, objectStart, objectLength, "process", null);
                if (text != null) config.SourceProcess = text;
            }

            config.Valid = true;
            lock (_sync)
            {
                _config = config;
                _configVersion++;
            }
            TouchPacket();
        }

        private void HandleInputPacket(byte[] buffer, int length)
        {
            int jsonStart;
            int jsonLength;
            if (!MisideProtocol.TryParseJsonHeader(buffer, length, MisideProtocol.PacketTypeInput, out jsonStart, out jsonLength))
            {
                if (length < MisideProtocol.JsonHeaderSize) Interlocked.Increment(ref _droppedTooShort);
                else Interlocked.Increment(ref _droppedOther);
                return;
            }

            InputData input = _input;
            input.Valid = true;
            int objectStart;
            int objectLength;
            float floatValue;
            bool boolValue;

            if (MisideProtocol.TryGetJsonObjectRange(buffer, jsonStart, jsonLength, "pointer", out objectStart, out objectLength))
            {
                if (MisideProtocol.TryGetJsonFloat(buffer, objectStart, objectLength, "x", out floatValue)) input.X = floatValue;
                if (MisideProtocol.TryGetJsonFloat(buffer, objectStart, objectLength, "y", out floatValue)) input.Y = floatValue;
                if (MisideProtocol.TryGetJsonBool(buffer, objectStart, objectLength, "inside", out boolValue)) input.Inside = boolValue;
                if (MisideProtocol.TryGetJsonBool(buffer, objectStart, objectLength, "left", out boolValue)) input.Left = boolValue;
                if (MisideProtocol.TryGetJsonBool(buffer, objectStart, objectLength, "right", out boolValue)) input.Right = boolValue;
                if (MisideProtocol.TryGetJsonBool(buffer, objectStart, objectLength, "middle", out boolValue)) input.Middle = boolValue;
                if (MisideProtocol.TryGetJsonFloat(buffer, objectStart, objectLength, "wheel", out floatValue)) input.WheelAccum += floatValue;
            }
            if (MisideProtocol.TryGetJsonObjectRange(buffer, jsonStart, jsonLength, "client", out objectStart, out objectLength))
            {
                if (MisideProtocol.TryGetJsonFloat(buffer, objectStart, objectLength, "width", out floatValue)) input.ClientWidth = floatValue;
                if (MisideProtocol.TryGetJsonFloat(buffer, objectStart, objectLength, "height", out floatValue)) input.ClientHeight = floatValue;
            }
            if (MisideProtocol.TryGetJsonBool(buffer, jsonStart, jsonLength, "locked", out boolValue)) input.Locked = boolValue;

            lock (_sync) { _input = input; }
            Volatile.Write(ref _lastInputTick, Environment.TickCount);
            TouchPacket();
        }

        private void TouchPacket()
        {
            Volatile.Write(ref _lastPacketTick, Environment.TickCount);
            Interlocked.Increment(ref _packetsReceived);
        }

        private void LatchBeat(MisideFrameData frame)
        {
            if (!frame.HasBeat) return;
            float strength = frame.Beat;
            if (strength <= 0f) strength = 1f;
            lock (_sync)
            {
                _pendingBeatCount++;
                if (strength > _pendingBeatStrength) _pendingBeatStrength = strength;
            }
        }

        /// <summary>只记录错误文本，真正的 Debug.Log 交给主线程做（避免后台线程刷屏）。</summary>
        private void ReportThreadError(string message)
        {
            _threadError = message;
        }

        // ---------------------------------------------------------------------
        //  主线程
        // ---------------------------------------------------------------------

        private void Update()
        {
            float delta = Time.unscaledDeltaTime;
            if (float.IsNaN(delta) || delta <= 0f) delta = 1f / 60f;
            if (delta > 0.25f) delta = 0.25f;

            MisideFrameData frame;
            ConfigData config;
            InputData input;
            int configVersion;
            bool hasFrame;
            float wheelAccum;
            int pendingBeatCount;
            float pendingBeatStrength;

            lock (_sync)
            {
                int published = _publishedIndex;
                hasFrame = published >= 0;
                if (hasFrame)
                {
                    RxSlot slot = _slots[published];
                    frame = slot.Frame;
                    Buffer.BlockCopy(slot.Spectrum, 0, _stagingSpectrum, 0, MisideProtocol.SpectrumBins);
                    Buffer.BlockCopy(slot.Waveform, 0, _stagingWaveform, 0, MisideProtocol.WaveformBytes);
                }
                else
                {
                    frame = default(MisideFrameData);
                }
                config = _config;
                configVersion = _configVersion;
                input = _input;
                wheelAccum = _input.WheelAccum;
                _input.WheelAccum = 0f;
                pendingBeatCount = _pendingBeatCount;
                pendingBeatStrength = _pendingBeatStrength;
                _pendingBeatCount = 0;
                _pendingBeatStrength = 0f;
            }

            ApplyFrame(frame, hasFrame, delta);
            ApplyBeats(pendingBeatCount, pendingBeatStrength);
            ApplyConfigIfChanged(configVersion, config);
            SyncRenderScale();
            ApplyInput(input, wheelAccum);
            UpdateReceiveState();
            ReportStateChanges();
        }

        private void ApplyFrame(MisideFrameData frame, bool hasFrame, float delta)
        {
            if (!hasFrame || !frame.Valid) return;

            _lastFrameSeq = frame.Seq;
            _sampleRate = frame.SampleRate;
            _framePausedFlag = frame.Paused ? 1f : 0f;

            float levelK = SmoothingFactor(delta, levelSmoothingHalfLife);
            float spectrumK = SmoothingFactor(delta, spectrumSmoothingHalfLife);

            _rms = Mathf.Lerp(_rms, MisideProtocol.Clamp01(frame.Rms), levelK);
            _peak = Mathf.Lerp(_peak, MisideProtocol.Clamp01(frame.Peak), levelK);
            _level = Mathf.Lerp(_level, MisideProtocol.Clamp01(frame.Level), levelK);
            _bass = Mathf.Lerp(_bass, MisideProtocol.Clamp01(frame.Bass), levelK);
            _mid = Mathf.Lerp(_mid, MisideProtocol.Clamp01(frame.Mid), levelK);
            _treble = Mathf.Lerp(_treble, MisideProtocol.Clamp01(frame.Treble), levelK);
            float bpm = frame.Bpm;
            if (bpm < 0f) bpm = 0f;
            _bpm = Mathf.Lerp(_bpm, bpm, levelK);

            for (int i = 0; i < MisideProtocol.SpectrumBins; i++)
            {
                float target = _stagingSpectrum[i] * (1f / 255f);
                _spectrum[i] = Mathf.Lerp(_spectrum[i], target, spectrumK);
            }
            for (int i = 0; i < MisideProtocol.WaveformBytes; i += 2)
            {
                int bucket = i >> 1;
                float min = _stagingWaveform[i] * (1f / 127f);
                float max = _stagingWaveform[i + 1] * (1f / 127f);
                if (min < -1f) min = -1f;
                if (min > 1f) min = 1f;
                if (max < -1f) max = -1f;
                if (max > 1f) max = 1f;
                _waveformMin[bucket] = Mathf.Lerp(_waveformMin[bucket], min, spectrumK);
                _waveformMax[bucket] = Mathf.Lerp(_waveformMax[bucket], max, spectrumK);
            }
        }

        private void ApplyBeats(int pendingCount, float pendingStrength)
        {
            bool beat = pendingCount > 0;
            BeatThisFrame = beat;
            if (beat)
            {
                float strength = pendingStrength > 0f ? pendingStrength : 1f;
                if (strength > 1f) strength = 1f;
                _beatStrength = strength;
                _beatPulse = 1f;
                _lastBeatTime = Time.time;
                _hasBeatEver = true;

                Action<float> handler = OnBeat;
                if (handler != null)
                {
                    try { handler(strength); }
                    catch (Exception ex) { Log(MisideLogLevel.Error, "OnBeat 回调抛异常：" + ex.Message); }
                }
            }
            if (_beatPulse > 0f)
            {
                float decay = beatPulseDecay > 0.001f ? beatPulseDecay : 0.001f;
                _beatPulse = Mathf.Clamp01(_beatPulse - Time.unscaledDeltaTime / decay);
            }
        }

        private void ApplyConfigIfChanged(int configVersion, ConfigData config)
        {
            if (configVersion == _appliedConfigVersion) return;
            _appliedConfigVersion = configVersion;
            if (!config.Valid) return;
            _appliedConfig = config;

            if (applyTargetFramerate && config.TargetFps > 0 && Application.targetFrameRate != config.TargetFps)
            {
                Application.targetFrameRate = config.TargetFps;
                Log(MisideLogLevel.Verbose, "已把 Application.targetFrameRate 设为 " + config.TargetFps + "。");
            }
            if (pauseAudioListenerWhenPaused) AudioListener.pause = Paused;

            // 注意：先判断级别再拼字符串，否则每个配置包都会白白产生一次字符串分配。
            if (logLevel >= MisideLogLevel.Verbose)
            {
                Log(MisideLogLevel.Verbose, "配置包：targetFps=" + config.TargetFps + " renderScale=" + config.RenderScale.ToString("0.00")
                    + " muted=" + config.Muted
                    + " volume=" + config.Volume.ToString("0.00") + " paused=" + config.Paused
                    + " language=" + Language + " screen=" + config.Screen.width + "x" + config.Screen.height
                    + " @" + config.Screen.x + "," + config.Screen.y);
            }

            Action handler = OnConfigChanged;
            if (handler != null)
            {
                try { handler(); }
                catch (Exception ex) { Log(MisideLogLevel.Error, "OnConfigChanged 回调抛异常：" + ex.Message); }
            }
        }

        /// <summary>
        /// 把（宿主下发的）渲染倍率同步到 ScalableBufferManager。
        /// 每帧只做一次 float 比较，**只有倍率真的变了**才会去碰相机和渲染缓冲，
        /// 所以这个调用点放在 Update 里也不会遍历相机、不会产生 GC。
        /// </summary>
        private void SyncRenderScale()
        {
            if (!applyRenderScale)
            {
                // 运行期把开关关掉时，如果之前降过分辨率就复位一次，别让画面一直糊着；
                // 本来就没人动过（_renderScaleApplied == 1.0）则什么都不做。
                if (!Mathf.Approximately(_renderScaleApplied, MisideProtocol.MaxRenderScale))
                {
                    ApplyRenderScale(MisideProtocol.MaxRenderScale);
                }
                return;
            }

            float target = RenderScale;
            if (Mathf.Approximately(target, _renderScaleApplied)) return;
            ApplyRenderScale(target);
        }

        /// <summary>真正执行一次倍率切换（只在倍率变化时被调用）。</summary>
        private void ApplyRenderScale(float scale)
        {
            scale = MisideProtocol.SanitizeRenderScale(scale);
            _renderScaleApplied = scale;

#if UNITY_2019_3_OR_NEWER
            bool reduced = scale < 0.999f;

            if (reduced)
            {
                // 动态分辨率必须由相机自己允许：Camera.allowDynamicResolution = true。
                // 只在倍率变化时扫一次相机，绝不要把这个循环放进 Update。
                // 注意：倍率回到 1.0 时这里**不会**把 allowDynamicResolution 关掉 ——
                // 值为 false 可能是你自己设的，而倍率 1.0 时这个标志没有任何视觉效果。
                Camera[] cameras = Camera.allCameras;
                for (int i = 0; i < cameras.Length; i++)
                {
                    Camera camera = cameras[i];
                    if (camera == null) continue;
                    camera.allowDynamicResolution = true;
                }

#if UNITY_STANDALONE_WIN
                // Unity 官方手册：Windows 独立平台的动态分辨率只在 DirectX 12 下支持
                // （macOS 需要 Metal，Android 需要 Vulkan）。DX11 下改了也看不出效果，
                // 所以这里明确警告一次，省得以为是脚本没生效。
                if (SystemInfo.graphicsDeviceType == UnityEngine.Rendering.GraphicsDeviceType.Direct3D11)
                {
                    Log(MisideLogLevel.Warning, "当前图形 API 是 Direct3D 11，而 Windows 上的动态分辨率只在 DirectX 12 下生效，"
                        + "renderScale 可能看不到效果。可以在 Player Settings → Graphics APIs 里启用 D3D12，"
                        + "或改用渲染管线自带的 Render Scale（URP Asset / HDRP），也可以把 applyRenderScale 关掉自己控制。");
                }
#endif
            }

            // 降的是"可缩放缓冲"的内部分辨率，窗口本身照旧铺满整块显示器。
            ScalableBufferManager.ResizeBuffers(scale, scale);

            if (logLevel >= MisideLogLevel.Info)
            {
                int width = Mathf.CeilToInt(ScalableBufferManager.widthScaleFactor * UnityEngine.Screen.width);
                int height = Mathf.CeilToInt(ScalableBufferManager.heightScaleFactor * UnityEngine.Screen.height);
                Log(MisideLogLevel.Info, "渲染倍率已设为 " + scale.ToString("0.00")
                    + "，内部分辨率约 " + width + "x" + height + "（窗口仍铺满，由 GPU 上采样回全屏）。");
            }
#else
            // Unity 2019.3 以下没有 ScalableBufferManager / Camera.allowDynamicResolution 这条路，
            // 为了能编译通过，这里只提示不做事。
            if (logLevel >= MisideLogLevel.Warning)
            {
                Log(MisideLogLevel.Warning, "当前 Unity 版本不支持 ScalableBufferManager（需要 2019.3 及以上），"
                    + "renderScale = " + scale.ToString("0.00") + " 不会生效。");
            }
#endif
        }

        private void ApplyInput(InputData input, float wheelAccum)
        {
            int inputTick = Volatile.Read(ref _lastInputTick);
            bool fresh = input.Valid && unchecked(Environment.TickCount - inputTick) <= (int)inputStaleTimeoutMs;

            _pointerWheel = wheelAccum;
            if (!fresh)
            {
                // 宿主没在转发输入（或壁纸窗口不在前台）：指针状态全部清掉。
                _pointerInside = false;
                _pointerLeft = false;
                _pointerRight = false;
                _pointerMiddle = false;
                _pointerLocked = false;
                return;
            }

            _pointerLocked = input.Locked;
            if (input.Locked) return;

            _pointerX = input.X;
            _pointerY = input.Y;
            _clientWidth = input.ClientWidth;
            _clientHeight = input.ClientHeight;
            _pointerInside = input.Inside;
            _pointerLeft = input.Left;
            _pointerRight = input.Right;
            _pointerMiddle = input.Middle;
        }

        private void UpdateReceiveState()
        {
            long received = Interlocked.Read(ref _packetsReceived);
            if (received <= 0)
            {
                _lastPacketAgeMs = int.MaxValue;
                IsReceiving = false;
                return;
            }
            int tick = Volatile.Read(ref _lastPacketTick);
            _lastPacketAgeMs = unchecked(Environment.TickCount - tick);
            int stale = staleTimeoutMs > 0f ? (int)staleTimeoutMs : 1000;
            IsReceiving = _lastPacketAgeMs <= stale;
        }

        private void ReportStateChanges()
        {
            if (_udpOpen && !_loggedUdpOpen)
            {
                _loggedUdpOpen = true;
                Log(MisideLogLevel.Info, "UDP 已在 127.0.0.1:" + udpPort + " 上监听（enableUdp = true）。");
            }
            if (!_udpOpen) _loggedUdpOpen = false;

            string error = _threadError;
            if (error == null || error == _lastError) return;
            _lastError = error;
            Log(MisideLogLevel.Warning, error);
        }

        private float SmoothingFactor(float delta, float halfLife)
        {
            if (!smoothing || halfLife <= 0.0001f) return 1f;
            return 1f - Mathf.Exp(-0.6931472f * delta / halfLife);
        }

        private void Log(MisideLogLevel level, string message)
        {
            if (logLevel < level) return;
            string text = "[MisideAudioLink] " + message;
            if (level == MisideLogLevel.Error) Debug.LogError(text);
            else if (level == MisideLogLevel.Warning) Debug.LogWarning(text);
            else Debug.Log(text);
        }
    }
}
