/**
 * src/visualizer.ts
 * 音频可视化两大块：
 *  1. 频谱 —— 128 柱对数频率（20Hz → Nyquist）+ 峰顶保持线；
 *  2. 波形 —— `waveform` 里 128 组交错 (min, max) 画成的峰谷包络。
 * 另有 RMS / 峰值电平表、低中高三条小电平条、BPM 徽标与节拍闪烁描边。
 *
 * 只在收到新的音频帧时重绘（后端已限流 ≤30fps），空闲 IDLE_MS 后自然衰减归零。
 * 两个 canvas 各自用 devicePixelRatio + ResizeObserver 处理清晰度与尺寸。
 */
import type { AudioFramePayload } from "./api";
import { t } from "./i18n";
import { clamp, h } from "./ui";

export interface Visualizer {
  /** 挂到页面上（一个 flex column：频谱 + 波形 + 电平表） */
  element: HTMLElement;
  /** 把一帧数据压进缓冲区（只记录，下一帧 requestAnimationFrame 绘制） */
  push(frame: AudioFramePayload): void;
  /** 清掉数据（例如关闭音频时） */
  reset(): void;
  /** 语言切换后重刷所有文案 */
  refreshTexts(): void;
}

const BAR_COUNT = 128;
/** waveform 是 128 组 (min, max) = 256 个数 */
const WAVE_PAIRS = 128;
const IDLE_MS = 1500;
/** 整体增益，让普通音乐音量下柱子也看得见 */
const VISUAL_GAIN = 1.35;

interface Meter {
  fill: HTMLElement;
  value: HTMLElement;
}

interface MeterParts {
  wrap: HTMLElement;
  meter: Meter;
  labelEl: HTMLElement;
}

function makeMeter(label: string, cls = ""): MeterParts {
  const fill = h("div", { cls: `meter__fill ${cls}`.trim() });
  const track = h("div", { cls: "meter__track" }, fill);
  const value = h("span", { cls: "mono", text: "0.00" });
  const labelEl = h("span", { text: label });
  const wrap = h("div", { cls: "meter" }, labelEl, track, value);
  return { wrap, meter: { fill, value }, labelEl };
}

export function createVisualizer(): Visualizer {
  /* ---------------- 结构 ---------------- */

  const spectrumCanvas = h("canvas", { cls: "viz__spectrum" });
  const spectrumCtx = spectrumCanvas.getContext("2d");
  const waveCanvas = h("canvas", { cls: "viz__wave" });
  const waveCtx = waveCanvas.getContext("2d");

  const rmsMeter = makeMeter(t("audio.rms"));
  const peakMeter = makeMeter(t("audio.peak"), "meter__fill--peak");
  const bassMeter = makeMeter(t("audio.bass"));
  const midMeter = makeMeter(t("audio.mid"));
  const trebleMeter = makeMeter(t("audio.treble"));

  const beatDot = h("span", { cls: "beat__dot" });
  const beatLabel = h("span", { text: t("audio.beat") });
  const beatEl = h("span", { cls: "beat" }, beatDot, beatLabel);
  const bpmEl = h("span", { cls: "badge", text: t("audio.bpmUnknown") });

  const hint = h("div", { cls: "muted", text: t("audio.noFrame") });

  const meters = h(
    "div",
    { cls: "viz__meters" },
    rmsMeter.wrap,
    peakMeter.wrap,
    bassMeter.wrap,
    midMeter.wrap,
    trebleMeter.wrap,
    beatEl,
    bpmEl,
  );

  const element = h("div", { cls: "viz" }, spectrumCanvas, waveCanvas, meters, hint);

  /* ---------------- 数据缓冲 ---------------- */

  let spectrum: number[] = new Array<number>(BAR_COUNT).fill(0);
  let displayed: number[] = new Array<number>(BAR_COUNT).fill(0);
  let peakHold: number[] = new Array<number>(BAR_COUNT).fill(0);
  /** 交错 min/max，已归一到 -1..1 */
  let wave: number[] = new Array<number>(WAVE_PAIRS * 2).fill(0);
  let rms = 0;
  let peak = 0;
  let bass = 0;
  let mid = 0;
  let treble = 0;
  let bpm = 0;
  let lastFrameAt = 0;
  let beatUntil = 0;
  let hasFrame = false;
  let rafId = 0;

  const spectrumSize = { w: 0, h: 0 };
  const waveSize = { w: 0, h: 0 };

  function fitCanvas(canvas: HTMLCanvasElement, ctx: CanvasRenderingContext2D | null, size: { w: number; h: number }): void {
    const dpr = window.devicePixelRatio || 1;
    const rect = canvas.getBoundingClientRect();
    const cssW = Math.max(1, Math.floor(rect.width));
    const cssH = Math.max(1, Math.floor(rect.height));
    size.w = cssW;
    size.h = cssH;
    const pixelW = Math.max(1, Math.floor(cssW * dpr));
    const pixelH = Math.max(1, Math.floor(cssH * dpr));
    if (canvas.width !== pixelW || canvas.height !== pixelH) {
      canvas.width = pixelW;
      canvas.height = pixelH;
    }
    ctx?.setTransform(dpr, 0, 0, dpr, 0, 0);
  }

  function cssVar(name: string, fallback: string): string {
    const v = getComputedStyle(document.documentElement).getPropertyValue(name).trim();
    return v || fallback;
  }

  /** 铺底色：先把变换重置为单位矩阵，避免 dpr 缩放把填充区域算错 */
  function clearCanvas(
    canvas: HTMLCanvasElement,
    ctx: CanvasRenderingContext2D,
    bg: string,
  ): void {
    ctx.setTransform(1, 0, 0, 1, 0, 0);
    ctx.fillStyle = bg;
    ctx.fillRect(0, 0, canvas.width, canvas.height);
    const dpr = window.devicePixelRatio || 1;
    ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
  }

  function drawSpectrum(now: number, idle: boolean, plotH: number, accent: string, grid: string, textDim: string): void {
    if (!spectrumCtx) return;
    const width = spectrumSize.w;
    const height = spectrumSize.h;
    spectrumCtx.clearRect(0, 0, width, height);

    const padTop = 8;
    const padBottom = 16;
    const usable = Math.max(10, height - padTop - padBottom);

    spectrumCtx.strokeStyle = grid;
    spectrumCtx.lineWidth = 1;
    for (let g = 1; g <= 3; g += 1) {
      const y = padTop + (usable * g) / 4;
      spectrumCtx.beginPath();
      spectrumCtx.moveTo(0, y);
      spectrumCtx.lineTo(width, y);
      spectrumCtx.stroke();
    }

    // 柱高按可用高度的比例缩放，撑满就算 plotH 比 usable 大也只会更满
    const scaleH = Math.max(usable, plotH * 0.86);
    const gap = 1;
    const barW = Math.max(1, (width - (BAR_COUNT - 1) * gap) / BAR_COUNT);
    for (let i = 0; i < BAR_COUNT; i += 1) {
      const v = clamp((displayed[i] ?? 0) / 255, 0, 1);
      const barH = Math.max(v > 0.001 ? 2 : 0, v * scaleH * VISUAL_GAIN);
      const x = i * (barW + gap);
      const y = padTop + usable - Math.min(barH, usable);
      const ratio = i / (BAR_COUNT - 1);
      spectrumCtx.globalAlpha = 0.45 + 0.55 * (1 - ratio);
      spectrumCtx.fillStyle = accent;
      spectrumCtx.fillRect(x, y, barW, Math.min(barH, usable));

      const holdV = clamp((peakHold[i] ?? 0) / 255, 0, 1);
      if (holdV > 0.02) {
        spectrumCtx.globalAlpha = 0.9;
        spectrumCtx.fillStyle = textDim;
        spectrumCtx.fillRect(x, padTop + usable - holdV * scaleH * VISUAL_GAIN - 2, barW, 1.5);
      }
    }
    spectrumCtx.globalAlpha = 1;

    // 频率轴标注
    spectrumCtx.fillStyle = textDim;
    spectrumCtx.font = '10px "Microsoft YaHei UI", "Segoe UI", system-ui, sans-serif';
    spectrumCtx.textBaseline = "bottom";
    spectrumCtx.textAlign = "left";
    spectrumCtx.fillText("20 Hz", 2, height - 3);
    spectrumCtx.textAlign = "right";
    spectrumCtx.fillText("Nyquist", width - 2, height - 3);

    if (!idle && now < beatUntil) {
      spectrumCtx.fillStyle = cssVar("--accent-soft", "rgba(76,141,255,0.14)");
      spectrumCtx.fillRect(0, 0, width, height);
      spectrumCtx.strokeStyle = accent;
      spectrumCtx.lineWidth = 2;
      spectrumCtx.strokeRect(1, 1, width - 2, height - 2);
    }
  }

  function drawWave(idle: boolean, accent: string, grid: string, textDim: string): void {
    if (!waveCtx) return;
    const width = waveSize.w;
    const height = waveSize.h;
    waveCtx.clearRect(0, 0, width, height);

    const mid = height / 2;
    waveCtx.strokeStyle = grid;
    waveCtx.lineWidth = 1;
    waveCtx.beginPath();
    waveCtx.moveTo(0, mid);
    waveCtx.lineTo(width, mid);
    waveCtx.stroke();

    if (idle) return;

    const step = width / WAVE_PAIRS;
    const amp = Math.max(2, height / 2 - 3);
    waveCtx.strokeStyle = accent;
    waveCtx.globalAlpha = 0.85;
    waveCtx.lineWidth = Math.max(1, Math.min(2, step * 0.8));
    waveCtx.beginPath();
    for (let i = 0; i < WAVE_PAIRS; i += 1) {
      const min = clamp(wave[i * 2] ?? 0, -1, 1);
      const max = clamp(wave[i * 2 + 1] ?? 0, -1, 1);
      const x = i * step + step / 2;
      waveCtx.moveTo(x, mid - max * amp);
      waveCtx.lineTo(x, mid - min * amp);
    }
    waveCtx.stroke();
    waveCtx.globalAlpha = 1;

    waveCtx.fillStyle = textDim;
    waveCtx.font = '10px "Microsoft YaHei UI", "Segoe UI", system-ui, sans-serif';
    waveCtx.textBaseline = "bottom";
    waveCtx.textAlign = "left";
    waveCtx.fillText(t("audio.waveform"), 4, height - 3);
  }

  function draw(): void {
    rafId = 0;
    if (spectrumSize.w === 0) fitCanvas(spectrumCanvas, spectrumCtx, spectrumSize);
    if (waveSize.w === 0) fitCanvas(waveCanvas, waveCtx, waveSize);

    const now = performance.now();
    const idle = !hasFrame || now - lastFrameAt > IDLE_MS;
    const target = idle ? new Array<number>(BAR_COUNT).fill(0) : spectrum;

    // 柱体平滑：上升快、回落慢
    for (let i = 0; i < BAR_COUNT; i += 1) {
      const want = target[i] ?? 0;
      const prev = displayed[i] ?? 0;
      displayed[i] = want > prev ? want : prev * 0.86 + want * 0.14;
      peakHold[i] = Math.max((peakHold[i] ?? 0) * 0.94, displayed[i] ?? 0);
    }
    if (idle) {
      // 慢慢把波形收平
      for (let i = 0; i < wave.length; i += 1) wave[i] = (wave[i] ?? 0) * 0.8;
    }

    const bg = cssVar("--bg-input", "#10151c");
    const grid = cssVar("--border-soft", "#1e242e");
    const accent = cssVar("--accent", "#4c8dff");
    const textDim = cssVar("--text-mute", "#6d7788");

    if (spectrumCtx) {
      clearCanvas(spectrumCanvas, spectrumCtx, bg);
    }
    drawSpectrum(now, idle, spectrumSize.h, accent, grid, textDim);

    if (waveCtx) {
      clearCanvas(waveCanvas, waveCtx, bg);
    }
    drawWave(idle, accent, grid, textDim);

    // 电平表
    const eRms = idle ? 0 : rms;
    const ePeak = idle ? 0 : peak;
    rmsMeter.meter.fill.style.width = `${(clamp(eRms, 0, 1) * 100).toFixed(1)}%`;
    rmsMeter.meter.value.textContent = eRms.toFixed(2);
    peakMeter.meter.fill.style.width = `${(clamp(ePeak, 0, 1) * 100).toFixed(1)}%`;
    peakMeter.meter.value.textContent = ePeak.toFixed(2);

    const bands: Array<[Meter, number]> = [
      [bassMeter.meter, idle ? 0 : bass],
      [midMeter.meter, idle ? 0 : mid],
      [trebleMeter.meter, idle ? 0 : treble],
    ];
    for (const [meter, v] of bands) {
      meter.fill.style.width = `${(clamp(v, 0, 1) * 100).toFixed(1)}%`;
      meter.value.textContent = v.toFixed(2);
    }

    beatEl.classList.toggle("is-hit", !idle && now < beatUntil);
    bpmEl.textContent = !idle && bpm > 0 ? t("audio.bpm", [bpm.toFixed(0)]) : t("audio.bpmUnknown");
    hint.hidden = hasFrame;

    // 还有东西在动就继续画
    const moving = displayed.some((v) => v > 0.5) || wave.some((v) => Math.abs(v) > 0.02);
    if (!idle || moving) rafId = window.requestAnimationFrame(draw);
  }

  function kick(): void {
    if (rafId === 0) rafId = window.requestAnimationFrame(draw);
  }

  const observer = new ResizeObserver(() => {
    fitCanvas(spectrumCanvas, spectrumCtx, spectrumSize);
    fitCanvas(waveCanvas, waveCtx, waveSize);
    kick();
  });
  observer.observe(spectrumCanvas);
  observer.observe(waveCanvas);

  function refreshTexts(): void {
    rmsMeter.labelEl.textContent = t("audio.rms");
    peakMeter.labelEl.textContent = t("audio.peak");
    bassMeter.labelEl.textContent = t("audio.bass");
    midMeter.labelEl.textContent = t("audio.mid");
    trebleMeter.labelEl.textContent = t("audio.treble");
    beatLabel.textContent = t("audio.beat");
    hint.textContent = t("audio.noFrame");
    bpmEl.textContent = bpm > 0 ? t("audio.bpm", [bpm.toFixed(0)]) : t("audio.bpmUnknown");
    kick();
  }

  refreshTexts();

  return {
    element,
    push(frame: AudioFramePayload): void {
      hasFrame = true;
      lastFrameAt = performance.now();

      if (Array.isArray(frame.spectrum)) {
        spectrum = new Array<number>(BAR_COUNT).fill(0);
        const n = Math.min(BAR_COUNT, frame.spectrum.length);
        for (let i = 0; i < n; i += 1) {
          const v = Number(frame.spectrum[i]);
          spectrum[i] = Number.isFinite(v) ? clamp(v, 0, 255) : 0;
        }
      }

      if (Array.isArray(frame.waveform)) {
        const next = new Array<number>(WAVE_PAIRS * 2).fill(0);
        const n = Math.min(next.length, frame.waveform.length);
        for (let i = 0; i < n; i += 1) {
          const v = Number(frame.waveform[i]);
          next[i] = Number.isFinite(v) ? clamp(v / 127, -1, 1) : 0;
        }
        wave = next;
      }

      rms = clamp(Number(frame.rms) || 0, 0, 1);
      peak = clamp(Number(frame.peak) || 0, 0, 1);
      bass = clamp(Number(frame.bass) || 0, 0, 1);
      mid = clamp(Number(frame.mid) || 0, 0, 1);
      treble = clamp(Number(frame.treble) || 0, 0, 1);
      bpm = Number(frame.bpm) || 0;
      if ((Number(frame.beat) || 0) >= 0.5) beatUntil = performance.now() + 110;
      kick();
    },
    reset(): void {
      hasFrame = false;
      spectrum = new Array<number>(BAR_COUNT).fill(0);
      wave = new Array<number>(WAVE_PAIRS * 2).fill(0);
      lastFrameAt = 0;
      beatUntil = 0;
      kick();
    },
    refreshTexts,
  };
}
