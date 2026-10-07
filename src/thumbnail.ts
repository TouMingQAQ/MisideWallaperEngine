/**
 * src/thumbnail.ts
 * 「当前壁纸」实时缩略图：命令 `wallpaper_thumbnail` 返回裸二进制
 * （`[width u32 LE][height u32 LE][RGBA 自上而下]`），0 字节 = 当前拿不到预览。
 *
 * 这个命令不走 api.ts 的 JSON Result 包装（返回的是 ArrayBuffer），所以单独处理错误：
 * 失败一律当成「这一帧拿不到」，只 console.debug，不弹提示、不抛异常。
 */
import { invoke } from "@tauri-apps/api/core";

/** 一帧缩略图 */
export interface ThumbFrame {
  width: number;
  height: number;
  image: ImageData;
}

/** 轮询用的最小间隔（毫秒） */
export const THUMB_INTERVAL_MS = 700;

function toBuffer(raw: unknown): ArrayBuffer | null {
  if (raw instanceof ArrayBuffer) return raw;
  if (ArrayBuffer.isView(raw)) {
    const view = raw as ArrayBufferView;
    return view.buffer.slice(view.byteOffset, view.byteOffset + view.byteLength) as ArrayBuffer;
  }
  if (Array.isArray(raw)) return new Uint8Array(raw as number[]).buffer;
  return null;
}

/**
 * 抓一帧缩略图。拿不到（空响应 / 后端未就绪 / 尺寸非法）返回 null。
 * 任何异常都被吞掉——缩略图只是锦上添花，不能影响主界面。
 */
export async function fetchThumbnail(): Promise<ThumbFrame | null> {
  let raw: unknown;
  try {
    raw = await invoke("wallpaper_thumbnail");
  } catch (err) {
    console.debug("[wp] wallpaper_thumbnail failed", err);
    return null;
  }

  const buf = toBuffer(raw);
  // 8 字节头都凑不齐 = 没有预览
  if (!buf || buf.byteLength < 8) return null;

  const view = new DataView(buf);
  const width = view.getUint32(0, true);
  const height = view.getUint32(4, true);
  if (width < 2 || height < 2) return null;
  const need = 8 + width * height * 4;
  if (buf.byteLength < need) {
    console.warn(
      `[wp] thumbnail payload too short: ${buf.byteLength} < ${need} (${width}x${height})`,
    );
    return null;
  }

  const rgba = new Uint8ClampedArray(buf, 8, width * height * 4);
  let image: ImageData;
  try {
    image = new ImageData(rgba, width, height);
  } catch (err) {
    console.warn("[wp] ImageData 构造失败", err);
    return null;
  }
  return { width, height, image };
}

/** 把一帧按「等比铺满 + 留黑边」画进 canvas（canvas 的 CSS 尺寸由布局决定） */
export function drawThumbnail(canvas: HTMLCanvasElement, frame: ThumbFrame): void {
  const ctx = canvas.getContext("2d");
  if (!ctx) return;

  const dpr = window.devicePixelRatio || 1;
  const cssW = Math.max(1, canvas.clientWidth);
  const cssH = Math.max(1, canvas.clientHeight);
  const pixelW = Math.max(1, Math.round(cssW * dpr));
  const pixelH = Math.max(1, Math.round(cssH * dpr));
  if (canvas.width !== pixelW || canvas.height !== pixelH) {
    canvas.width = pixelW;
    canvas.height = pixelH;
  }

  // 背景（letterbox 的"黑边"）由 CSS 变量控制，跟随深浅色主题
  const css = getComputedStyle(document.documentElement);
  const bg = css.getPropertyValue("--bg-input").trim() || "#10151c";
  ctx.setTransform(1, 0, 0, 1, 0, 0);
  ctx.fillStyle = bg;
  ctx.fillRect(0, 0, pixelW, pixelH);

  const scale = Math.min(pixelW / frame.width, pixelH / frame.height);
  const drawW = Math.max(1, Math.floor(frame.width * scale));
  const drawH = Math.max(1, Math.floor(frame.height * scale));
  const dx = Math.floor((pixelW - drawW) / 2);
  const dy = Math.floor((pixelH - drawH) / 2);

  // 源尺寸和 ImageData 一致时直接 putImageData（省一次缩放），否则用中间位图缩放
  if (drawW === frame.width && drawH === frame.height) {
    ctx.putImageData(frame.image, dx, dy);
    return;
  }
  // 先把原图放到一个临时 canvas，再 drawImage 缩放，避免逐像素插值
  const tmp = document.createElement("canvas");
  tmp.width = frame.width;
  tmp.height = frame.height;
  const tmpCtx = tmp.getContext("2d");
  if (!tmpCtx) {
    ctx.putImageData(frame.image, dx, dy);
    return;
  }
  tmpCtx.putImageData(frame.image, 0, 0);
  ctx.imageSmoothingEnabled = true;
  ctx.imageSmoothingQuality = "medium";
  ctx.drawImage(tmp, dx, dy, drawW, drawH);
}

/** 清空 canvas（切到占位状态时用） */
export function clearThumbnail(canvas: HTMLCanvasElement): void {
  const ctx = canvas.getContext("2d");
  if (!ctx) return;
  const css = getComputedStyle(document.documentElement);
  const bg = css.getPropertyValue("--bg-input").trim() || "#10151c";
  ctx.setTransform(1, 0, 0, 1, 0, 0);
  ctx.fillStyle = bg;
  ctx.fillRect(0, 0, canvas.width, canvas.height);
}

/**
 * 轮询控制器：`start()` 才会每 700ms 抓一帧，`stop()` 立刻停。
 * 页面不可见 / 未运行 / 已暂停时由调用方保证不会 start。
 */
export interface ThumbnailSource {
  start(): void;
  stop(): void;
  /** 不等轮询，立刻抓一帧（尺寸变化、切换壁纸后用） */
  request(): void;
}

export function createThumbnailSource(
  onFrame: (frame: ThumbFrame) => void,
  onMiss: () => void,
): ThumbnailSource {
  let timer = 0;
  let active = false;
  let inFlight = false;

  const tick = async (): Promise<void> => {
    if (!active || inFlight) return;
    inFlight = true;
    try {
      const frame = await fetchThumbnail();
      if (!active) return;
      if (frame) onFrame(frame);
      else onMiss();
    } catch (err) {
      // fetchThumbnail 自己已经兜底，这里只是最后一道保险
      console.debug("[wp] thumbnail tick failed", err);
      onMiss();
    } finally {
      inFlight = false;
    }
  };

  return {
    start(): void {
      if (active) return;
      active = true;
      void tick();
      timer = window.setInterval(() => void tick(), THUMB_INTERVAL_MS);
    },
    stop(): void {
      active = false;
      if (timer !== 0) {
        window.clearInterval(timer);
        timer = 0;
      }
    },
    request(): void {
      void tick();
    },
  };
}
