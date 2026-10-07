/**
 * src/api.ts
 * 契约层：api-contract.md 里定义的全部 Tauri 命令（invoke）与事件（listen）的 typed wrapper。
 * 所有调用都包在 try/catch 里——Rust 后端此刻还没写完，任何失败都转成 Result 交给界面提示，
 * 绝不允许抛出未捕获异常把页面搞白屏。
 */
import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";

/* ======================================================================== *
 * 1. 数据类型（严格对应 api-contract.md 第 1、2 节）
 * ======================================================================== */

export type ThemeMode = "dark" | "light" | "system";
export type Language = "zh-CN" | "en-US";
export type AudioSourceMode = "auto" | "process" | "off";
export type AudioState = "idle" | "waiting" | "capturing" | "error";
/** RuntimeState.mode，见契约 2.2 */
export type WallpaperMode = "stopped" | "preview" | "desktop";

/** 一条自定义参数：控制端下发给壁纸端的业务参数（契约 1 / 5.4）。 */
export interface CustomParam {
  /** 参数键，小写字母/数字/下划线，例：`beat` / `costume` */
  key: string;
  /** 参数值；统一按字符串传，语义由壁纸端决定 */
  value: string;
  /** 锁定：界面上不允许改值（预置的关键参数用它防误关） */
  locked: boolean;
}

/** 设置对象 `Settings`（契约 1） */
export interface Settings {
  language: Language;
  themeMode: ThemeMode;
  libraryDir: string;
  activeWallpaper: string;
  autoApply: boolean;
  audioEnabled: boolean;
  audioSource: AudioSourceMode;
  audioProcess: string;
  udpHost: string;
  udpPort: number;
  audioGain: number;
  audioFrameRate: number;
  volume: number;
  muted: boolean;
  targetFps: number;
  renderScale: number;
  monitorIndex: number;
  extraArgs: string;
  /** 壁纸进程用哪个图形 API：auto（Unity 自己挑）/ d3d11 / d3d12 */
  graphicsApi: "auto" | "d3d11" | "d3d12";
  pauseOnFullscreen: boolean;
  pauseOnBattery: boolean;
  pauseWhenForeground: boolean;
  inputForward: boolean;
  inputLocked: boolean;
  /** 透传给壁纸端的业务参数（宿主不解释语义） */
  customParams: CustomParam[];
  /** 接收壁纸端「参数能力上报」的 UDP 端口，`0` = 关闭 */
  reportPort: number;
  autoStart: boolean;
  closeToTray: boolean;
  previewWidth: number;
  previewHeight: number;
}

/** 壁纸库条目（契约 2.1） */
export interface WallpaperEntry {
  id: string;
  name: string;
  dir: string;
  exe: string;
  exeName: string;
  sizeBytes: number;
  fileCount: number;
  importedAt: string;
  unity: boolean;
  unityVersion: string;
  broken: boolean;
  missingExe: boolean;
}

/** 运行时状态（契约 2.2） */
export interface RuntimeState {
  mode: WallpaperMode;
  pid: number;
  hwnd: number;
  wallpaperId: string;
  exe: string;
  attached: boolean;
  paused: boolean;
  userPaused: boolean;
  autoPaused: boolean;
  muted: boolean;
  volume: number;
  monitorIndex: number;
  startedAtMs: number;
  memoryMb: number;
  occluded: boolean;
  inputForwarding: boolean;
  lastError: string;
}

/** 音频采集状态（契约 2.3） */
export interface AudioStatus {
  enabled: boolean;
  source: AudioSourceMode;
  processName: string;
  pid: number;
  running: boolean;
  udpHost: string;
  udpPort: number;
  packetsSent: number;
  state: AudioState;
  message: string;
  dllOk: boolean;
  dllVersion: number;
}

export type AudioSessionState = "none" | "active" | "inactive" | "expired";
export type MediaStatus =
  | "unknown"
  | "playing"
  | "paused"
  | "stopped"
  | "closed"
  | "changing"
  | "opened";

/** 音频目标（契约 2.4） */
export interface AudioTarget {
  pid: number;
  processName: string;
  title: string;
  hasWindow: boolean;
  windowVisible: boolean;
  sessionState: AudioSessionState;
  sessionPeak: number;
  mediaTitle: string;
  mediaArtist: string;
  mediaAlbum: string;
  mediaStatus: MediaStatus;
}

/** 显示器信息（契约 2.5） */
export interface MonitorInfo {
  index: number;
  name: string;
  x: number;
  y: number;
  width: number;
  height: number;
  primary: boolean;
  scaleFactor: number;
}

/** 音频帧（事件 `wp://audio-frame`，契约 2.6） */
export interface AudioFramePayload {
  pid: number;
  rms: number;
  peak: number;
  level: number;
  beat: number;
  bpm: number;
  bass: number;
  mid: number;
  treble: number;
  spectrum: number[];
  /**
   * 256 个 -127–127，128 组**交错** (min, max)：偶数索引 = min、奇数索引 = max。
   * 界面画波形包络用。
   */
  waveform: number[];
}

/** 一个参数可选值（契约 2.8） */
export interface ParamOption {
  value: string;
  label: string;
}

/** 壁纸端回报的一条参数能力（契约 2.8） */
export interface ParamReport {
  key: string;
  label: string;
  /** 该参数当前在壁纸端生效的值 */
  value: string;
  /** 可选值列表；为空表示自由填写 */
  options: ParamOption[];
  /** 壁纸端是否接受改这个参数 */
  writable: boolean;
}

/** 壁纸端上报快照（命令 `param_report`，契约 2.8） */
export interface ParamReportSnapshot {
  /** 壁纸端标识（脚本版本号） */
  client: string;
  params: ParamReport[];
  /** 距最近一次上报的毫秒数；`null` = 从未收到过 */
  ageMs: number | null;
}

/** 导入进度（事件 `wp://import-progress`，契约 4） */
export interface ImportProgress {
  phase: "reading" | "extracting" | "finalizing" | "done";
  percent: number;
  files: number;
  totalFiles: number;
  current: string;
  id: string;
}

/** 显示器 / 遮挡等监控信息（事件 `wp://monitor`，契约 4） */
export interface MonitorState {
  attached: boolean;
  occluded: boolean;
  fullscreen: boolean;
  onBattery: boolean;
  foreground: boolean;
  paused: boolean;
  pid: number;
  mode: WallpaperMode;
  monitorIndex: number;
}

/** 应用信息（命令 `app_info`，契约 3） */
export interface AppInfo {
  version: string;
  libraryDir: string;
  defaultLibraryDir: string;
  wallpaperCount: number;
  totalBytes: number;
  dllOk: boolean;
  dllVersion: number;
  dllPath: string;
  autostart: boolean;
}

export type LogLevel = "info" | "warn" | "error";

/** 日志（事件 `wp://log`，契约 4） */
export interface LogEntry {
  level: LogLevel;
  message: string;
  at: number;
}

/* ======================================================================== *
 * 2. 设置默认值（契约 1 的默认列）
 * ======================================================================== */

export const DEFAULT_SETTINGS: Settings = {
  language: "zh-CN",
  themeMode: "dark",
  libraryDir: "",
  activeWallpaper: "",
  autoApply: true,
  audioEnabled: true,
  audioSource: "auto",
  audioProcess: "",
  udpHost: "127.0.0.1",
  udpPort: 47810,
  audioGain: 1.0,
  audioFrameRate: 60,
  volume: 1.0,
  muted: false,
  targetFps: 60,
  renderScale: 1.0,
  // 与 Rust 侧默认值保持一致：0 = 只铺主显示器（-1 才是「全部显示器」）
  monitorIndex: 0,
  extraArgs: "",
  graphicsApi: "auto",
  pauseOnFullscreen: true,
  pauseOnBattery: false,
  pauseWhenForeground: false,
  inputForward: false,
  inputLocked: false,
  // 预置参数：节拍（锁定）+ 服装。与 Rust 侧 `Settings::default()` 保持一致。
  customParams: [
    { key: "beat", value: "1", locked: true },
    { key: "costume", value: "random", locked: false },
  ],
  reportPort: 47811,
  autoStart: false,
  closeToTray: true,
  previewWidth: 960,
  previewHeight: 540,
};

/** 运行时状态的空值（未运行） */
export const EMPTY_STATE: RuntimeState = {
  mode: "stopped",
  pid: 0,
  hwnd: 0,
  wallpaperId: "",
  exe: "",
  attached: false,
  paused: false,
  userPaused: false,
  autoPaused: false,
  muted: false,
  volume: 1.0,
  monitorIndex: 0,
  startedAtMs: 0,
  memoryMb: 0,
  occluded: false,
  inputForwarding: false,
  lastError: "",
};

/** 音频状态的空值 */
export const EMPTY_AUDIO_STATUS: AudioStatus = {
  enabled: false,
  source: "auto",
  processName: "",
  pid: 0,
  running: false,
  udpHost: "127.0.0.1",
  udpPort: 47810,
  packetsSent: 0,
  state: "idle",
  message: "",
  dllOk: false,
  dllVersion: 0,
};

/* ======================================================================== *
 * 3. 调用结果：永不抛出
 * ======================================================================== */

export interface ApiOk<T> {
  ok: true;
  value: T;
}
export interface ApiFail {
  ok: false;
  /** 已归一化的错误文案（后端原文，可能是中英混合的 Rust 错误串） */
  error: string;
  /** 判定为「后端命令还不存在 / webview 不在 Tauri 里」 */
  unavailable: boolean;
  /** 用户主动取消（原生对话框返回取消、命令被中断）不算错误 */
  canceled: boolean;
}
export type Result<T> = ApiOk<T> | ApiFail;

function isTauriRuntime(): boolean {
  return typeof window !== "undefined" && "__TAURI_INTERNALS__" in window;
}

const CANCEL_PATTERN = /cancel|取消|用户中止|aborted by user/i;
/** 命令没注册 / 没有 IPC 通道时的典型报错 */
const MISSING_CMD_PATTERN =
  /not found|unknown command|no such command|__tauri_invoke__|__TAURI_INTERNALS__|is not a function|undefined is not an object/i;

/** 把任意抛出物归一成 { error, unavailable, canceled } */
export function normalizeError(err: unknown): ApiFail {
  const raw =
    typeof err === "string"
      ? err
      : err instanceof Error
        ? err.message
        : (() => {
            try {
              return JSON.stringify(err);
            } catch {
              return String(err);
            }
          })();

  if (!isTauriRuntime()) {
    return {
      ok: false,
      error: "not running inside Tauri webview",
      unavailable: true,
      canceled: false,
    };
  }
  if (CANCEL_PATTERN.test(raw)) {
    return { ok: false, error: raw, unavailable: false, canceled: true };
  }
  return {
    ok: false,
    error: raw,
    unavailable: MISSING_CMD_PATTERN.test(raw),
    canceled: false,
  };
}

/**
 * 统一的命令调用入口。宁可少写一层类型推断的魔法，也要保证「不抛异常」。
 */
async function call<T>(cmd: string, args?: Record<string, unknown>): Promise<Result<T>> {
  try {
    const value = await invoke<T>(cmd, args);
    return { ok: true, value };
  } catch (err) {
    return normalizeError(err);
  }
}

/* ======================================================================== *
 * 4. 全部命令（契约 3，命令名 snake_case、参数名 camelCase）
 * ======================================================================== */

export const api = {
  getSettings: () => call<Settings>("get_settings"),

  saveSettings: (settings: Settings) => call<Settings>("save_settings", { settings }),

  appInfo: () => call<AppInfo>("app_info"),

  listMonitors: () => call<MonitorInfo[]>("list_monitors"),

  libraryList: () => call<WallpaperEntry[]>("library_list"),

  pickZipPath: () => call<string | null>("pick_zip_path"),

  pickLibraryDir: () => call<string | null>("pick_library_dir"),

  importZip: (zipPath: string, name?: string) =>
    call<WallpaperEntry>("import_zip", name === undefined ? { zipPath } : { zipPath, name }),

  removeWallpaper: (id: string) => call<WallpaperEntry[]>("remove_wallpaper", { id }),

  renameWallpaper: (id: string, name: string) =>
    call<WallpaperEntry>("rename_wallpaper", { id, name }),

  revealWallpaper: (id: string) => call<void>("reveal_wallpaper", { id }),

  previewWallpaper: (id: string) => call<RuntimeState>("preview_wallpaper", { id }),

  applyWallpaper: (id: string) => call<RuntimeState>("apply_wallpaper", { id }),

  stopWallpaper: () => call<RuntimeState>("stop_wallpaper"),

  reloadWallpaper: () => call<RuntimeState>("reload_wallpaper"),

  togglePause: () => call<RuntimeState>("toggle_pause"),

  setVolume: (volume: number) => call<RuntimeState>("set_volume", { volume }),

  setMuted: (muted: boolean) => call<RuntimeState>("set_muted", { muted }),

  wallpaperState: () => call<RuntimeState>("wallpaper_state"),

  listAudioTargets: () => call<AudioTarget[]>("list_audio_targets"),

  audioStatus: () => call<AudioStatus>("audio_status"),

  /** 壁纸端上报的参数能力快照（服装列表等） */
  paramReport: () => call<ParamReportSnapshot>("param_report"),

  /** 空串 → 回到 auto */
  setAudioTarget: (processName: string) =>
    call<AudioStatus>("set_audio_target", { processName }),

  setAudioEnabled: (enabled: boolean) => call<AudioStatus>("set_audio_enabled", { enabled }),

  setAutostart: (enabled: boolean) => call<boolean>("set_autostart", { enabled }),

  openLogDir: () => call<void>("open_log_dir"),
};

/* ======================================================================== *
 * 5. 事件名常量（契约 4）
 * ======================================================================== */

export const EV = {
  state: "wp://state",
  library: "wp://library",
  importProgress: "wp://import-progress",
  audioFrame: "wp://audio-frame",
  audioStatus: "wp://audio-status",
  /** 壁纸端上报了参数能力（服装列表等），界面据此刷新可选项 */
  paramReport: "wp://param-report",
  settings: "wp://settings",
  monitor: "wp://monitor",
  log: "wp://log",
  /** Tauri 自带：文件拖入窗口 */
  dragEnter: "tauri://drag-enter",
  dragOver: "tauri://drag-over",
  dragDrop: "tauri://drag-drop",
  dragLeave: "tauri://drag-leave",
} as const;

/** 拖放事件负载（Tauri 2 原生） */
export interface DragDropPayload {
  paths: string[];
  position: { x: number; y: number };
}

/**
 * 统一的事件订阅：失败（后端还没 emit、事件系统不可用等）只 console.warn，
 * 返回一个永远安全的取消订阅函数。
 */
export async function safeListen<T>(
  event: string,
  handler: (payload: T) => void,
  onFail?: (fail: ApiFail) => void,
): Promise<UnlistenFn> {
  try {
    return await listen<T>(event, (e) => {
      try {
        handler(e.payload);
      } catch (err) {
        // 回调自身的异常也不能冒泡到事件系统
        console.error(`[wp] handler for ${event} failed`, err);
      }
    });
  } catch (err) {
    const fail = normalizeError(err);
    console.warn(`[wp] listen ${event} failed: ${fail.error}`);
    onFail?.(fail);
    return () => {};
  }
}
