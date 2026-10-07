/**
 * src/state.ts
 * 全局状态中枢：
 *  - 设置缓存（get_settings / save_settings，订阅 wp://settings，300ms 防抖保存）
 *  - 运行时状态订阅（wp://state / wp://monitor）
 *  - 语言与主题的应用（写 <html data-theme>、调 i18n.setLang）
 * 后端还没写完，所以初始化失败会降级到默认设置并定时重试。
 */
import {
  api,
  DEFAULT_SETTINGS,
  EMPTY_AUDIO_STATUS,
  EMPTY_STATE,
  EV,
  safeListen,
  type AudioStatus,
  type ImportProgress,
  type Language,
  type LogEntry,
  type MonitorState,
  type Result,
  type RuntimeState,
  type Settings,
  type ThemeMode,
} from "./api";
import { getLang, setLang, t, tBackendUnavailable } from "./i18n";

export type ToastKind = "info" | "ok" | "warn" | "error";

export interface Toast {
  id: number;
  kind: ToastKind;
  message: string;
}

/** 页面通过 setToastSink 注册的吐司渲染器 */
export type ToastSink = (toast: Toast) => void;

/* ======================================================================== *
 * 内部状态
 * ======================================================================== */

let settings: Settings = { ...DEFAULT_SETTINGS };
let runtime: RuntimeState = { ...EMPTY_STATE };
let monitor: MonitorState | null = null;
let audioStatus: AudioStatus = { ...EMPTY_AUDIO_STATUS };
let importProgress: ImportProgress | null = null;
const logBuffer: LogEntry[] = [];
let libraryDir = "";
let defaultLibraryDir = "";
/** 最近一次成功保存设置的时间戳，设置页用来闪一下「已保存」 */
let lastSavedAt = 0;
/** 后端设置是否已经取到（false 时界面显示「后端暂未就绪」并降级） */
let backendReady = false;
let backendNotified = false;

const settingsListeners = new Set<() => void>();
const runtimeListeners = new Set<() => void>();
const audioListeners = new Set<() => void>();
const libraryDirListeners = new Set<() => void>();
const importListeners = new Set<() => void>();
const logListeners = new Set<() => void>();
const saveStateListeners = new Set<() => void>();
const toastSinks = new Set<ToastSink>();
/** 日志最多保留 400 条 */
const MAX_LOGS = 400;

const SAVE_DEBOUNCE_MS = 300;
let saveTimer: number | null = null;
/** 最近一次提交给后端的设置，用于忽略自己触发的回声 */
let inflight: Settings | null = null;
let toastSeq = 0;

/* ======================================================================== *
 * 通知
 * ======================================================================== */

function notifyAll(handlers: Set<() => void>): void {
  for (const fn of handlers) {
    try {
      fn();
    } catch (err) {
      console.error("[wp] state listener failed", err);
    }
  }
}

/* ======================================================================== *
 * 吐司
 * ======================================================================== */

export function setToastSink(sink: ToastSink): () => void {
  toastSinks.add(sink);
  return () => toastSinks.delete(sink);
}

export function toast(kind: ToastKind, message: string): void {
  toastSeq += 1;
  const item: Toast = { id: toastSeq, kind, message };
  for (const sink of toastSinks) {
    try {
      sink(item);
    } catch (err) {
      console.error("[wp] toast sink failed", err);
    }
  }
}

/** 把一次失败的调用变成友好提示；后端未就绪时同一个提示只弹一次 */
export function reportFailure<T>(res: Result<T>, context?: string): void {
  if (res.ok || res.canceled) return;
  const detail = res.unavailable ? tBackendUnavailable() : res.error;
  const message = context ? `${context}：${detail}` : detail;
  if (res.unavailable) {
    if (backendNotified) return;
    backendNotified = true;
    window.setTimeout(() => {
      backendNotified = false;
    }, 8000);
  }
  toast("error", message);
}

/* ======================================================================== *
 * 设置
 * ======================================================================== */

export function getSettings(): Settings {
  return settings;
}

export function isBackendReady(): boolean {
  return backendReady;
}

export function subscribeSettings(fn: () => void): () => void {
  settingsListeners.add(fn);
  return () => settingsListeners.delete(fn);
}

/** 合并后端回传的设置（后端是规范化后的事实来源） */
function adoptSettings(next: Settings, fromBackend: boolean): void {
  settings = { ...DEFAULT_SETTINGS, ...next };
  applyLanguage(settings.language);
  applyTheme(settings.themeMode);
  if (fromBackend) {
    inflight = null;
  }
  notifyAll(settingsListeners);
}

function applyTheme(mode: ThemeMode): void {
  const prefersLight =
    typeof window !== "undefined" &&
    typeof window.matchMedia === "function" &&
    window.matchMedia("(prefers-color-scheme: light)").matches;
  const resolved: "dark" | "light" =
    mode === "light" ? "light" : mode === "dark" ? "dark" : prefersLight ? "light" : "dark";
  document.documentElement.dataset.theme = resolved;
  document.documentElement.style.colorScheme = resolved;
}

function applyLanguage(lang: Language): void {
  document.documentElement.lang = lang;
  setLang(lang);
}

/** 跟随系统主题时监听系统变化 */
function watchSystemTheme(): void {
  if (typeof window === "undefined" || typeof window.matchMedia !== "function") return;
  const mq = window.matchMedia("(prefers-color-scheme: light)");
  const onChange = (): void => {
    if (settings.themeMode === "system") applyTheme("system");
  };
  if (typeof mq.addEventListener === "function") mq.addEventListener("change", onChange);
}

/* ======================================================================== *
 * 运行时状态 / 音频状态
 * ======================================================================== */

export function getRuntime(): RuntimeState {
  return runtime;
}

export function getMonitor(): MonitorState | null {
  return monitor;
}

export function getAudioStatus(): AudioStatus {
  return audioStatus;
}

export function subscribeRuntime(fn: () => void): () => void {
  runtimeListeners.add(fn);
  return () => runtimeListeners.delete(fn);
}

export function subscribeAudioStatus(fn: () => void): () => void {
  audioListeners.add(fn);
  return () => audioListeners.delete(fn);
}

export function subscribeLibraryDir(fn: () => void): () => void {
  libraryDirListeners.add(fn);
  return () => libraryDirListeners.delete(fn);
}

export function getImportProgress(): ImportProgress | null {
  return importProgress;
}

export function setImportProgress(
  progress: ImportProgress | null,
  listener?: (progress: ImportProgress | null) => void,
): void {
  importProgress = progress;
  notifyAll(importListeners);
  if (listener) listener(progress);
}

export function subscribeImportProgress(fn: () => void): () => void {
  importListeners.add(fn);
  return () => importListeners.delete(fn);
}

export function getLogs(): LogEntry[] {
  return logBuffer;
}

export function clearLogs(): void {
  logBuffer.length = 0;
  notifyAll(logListeners);
}

export function subscribeLogs(fn: () => void): () => void {
  logListeners.add(fn);
  return () => logListeners.delete(fn);
}

/** 最近一次成功保存设置的时间戳 */
export function getLastSavedAt(): number {
  return lastSavedAt;
}

/** 保存状态变化（含失败），设置页用来显示反馈 */
export function subscribeSaveState(fn: () => void): () => void {
  saveStateListeners.add(fn);
  return () => saveStateListeners.delete(fn);
}

export function getLibraryDir(): string {
  return libraryDir;
}

export function getDefaultLibraryDir(): string {
  return defaultLibraryDir;
}

/** 命令返回 RuntimeState 时更新缓存 */
function adoptRuntime(next: RuntimeState | null | undefined): void {
  if (!next) return;
  runtime = { ...EMPTY_STATE, ...next };
  notifyAll(runtimeListeners);
}

function adoptAudioStatus(next: AudioStatus | null | undefined): void {
  if (!next) return;
  audioStatus = { ...EMPTY_AUDIO_STATUS, ...next };
  notifyAll(audioListeners);
}

/* ======================================================================== *
 * 命令包装：调用 + 缓存更新 + 失败提示
 * ======================================================================== */

export const store = {
  /** 停止壁纸 */
  async stop(): Promise<RuntimeState | null> {
    const res = await api.stopWallpaper();
    if (!res.ok) {
      reportFailure(res, t("action.stop"));
      return null;
    }
    adoptRuntime(res.value);
    return res.value;
  },

  async reload(): Promise<RuntimeState | null> {
    const res = await api.reloadWallpaper();
    if (!res.ok) {
      reportFailure(res, t("action.reload"));
      return null;
    }
    adoptRuntime(res.value);
    return res.value;
  },

  async apply(id: string): Promise<RuntimeState | null> {
    const res = await api.applyWallpaper(id);
    if (!res.ok) {
      reportFailure(res, t("action.apply"));
      return null;
    }
    adoptRuntime(res.value);
    return res.value;
  },

  async preview(id: string): Promise<RuntimeState | null> {
    const res = await api.previewWallpaper(id);
    if (!res.ok) {
      reportFailure(res, t("action.preview"));
      return null;
    }
    adoptRuntime(res.value);
    return res.value;
  },

  async togglePause(): Promise<RuntimeState | null> {
    const res = await api.togglePause();
    if (!res.ok) {
      reportFailure(res, t("action.pause"));
      return null;
    }
    adoptRuntime(res.value);
    return res.value;
  },

  /** 音量：先本地生效（滑块要跟手），再写后端 + 设置 */
  async setVolume(volume: number): Promise<void> {
    const clamped = Math.min(1, Math.max(0, volume));
    runtime = { ...runtime, volume: clamped };
    notifyAll(runtimeListeners);
    const res = await api.setVolume(clamped);
    if (!res.ok) {
      reportFailure(res, t("settings.volume"));
    } else {
      adoptRuntime(res.value);
    }
    patchSettings({ volume: clamped });
  },

  async setMuted(muted: boolean): Promise<void> {
    runtime = { ...runtime, muted };
    notifyAll(runtimeListeners);
    const res = await api.setMuted(muted);
    if (!res.ok) {
      reportFailure(res, t("settings.muted"));
    } else {
      adoptRuntime(res.value);
    }
    patchSettings({ muted });
  },

  async setAudioEnabled(enabled: boolean): Promise<void> {
    const res = await api.setAudioEnabled(enabled);
    if (!res.ok) {
      reportFailure(res, t("audio.enabled"));
    } else {
      adoptAudioStatus(res.value);
      toast("info", enabled ? t("audio.enableDone") : t("audio.disableDone"));
    }
    patchSettings({ audioEnabled: enabled });
  },

  async setAudioTarget(processName: string): Promise<void> {
    const res = await api.setAudioTarget(processName);
    if (!res.ok) {
      reportFailure(res, t("audio.targetProcess"));
      return;
    }
    adoptAudioStatus(res.value);
    patchSettings({
      audioSource: processName ? "process" : "auto",
      audioProcess: processName,
    });
    toast("info", processName ? t("audio.targetSet", [processName]) : t("audio.targetAuto"));
  },

  async setAutostart(enabled: boolean): Promise<void> {
    const res = await api.setAutostart(enabled);
    if (!res.ok) {
      reportFailure(res, t("settings.autoStart"));
      // 命令失败时把设置回滚，避免界面显示与系统不一致
      patchSettings({ autoStart: !enabled });
      return;
    }
    patchSettings({ autoStart: res.value });
    toast("info", res.value ? t("settings.autostartOn") : t("settings.autostartOff"));
  },

  async openLogDir(): Promise<void> {
    const res = await api.openLogDir();
    if (!res.ok) {
      reportFailure(res, t("about.openLogDir"));
      return;
    }
    toast("info", t("about.logDirOpened"));
  },

  /** 立即保存（用于 autoStart 之类需要外部动作的项，实际仍走同一保存管线） */
  flushSave(): void {
    if (saveTimer !== null) {
      window.clearTimeout(saveTimer);
      saveTimer = null;
    }
    void commitSave();
  },
};

/* ======================================================================== *
 * 设置修改 + 防抖保存
 * ======================================================================== */

/** 本地改一项设置：立刻更新界面，300ms 后落盘 */
export function patchSettings(patch: Partial<Settings>): void {
  settings = { ...settings, ...patch };
  applyLanguage(settings.language);
  applyTheme(settings.themeMode);
  notifyAll(settingsListeners);
  scheduleSave();
}

function scheduleSave(): void {
  if (saveTimer !== null) window.clearTimeout(saveTimer);
  saveTimer = window.setTimeout(() => {
    saveTimer = null;
    void commitSave();
  }, SAVE_DEBOUNCE_MS);
}

async function commitSave(): Promise<void> {
  const snapshot: Settings = { ...settings };
  inflight = snapshot;
  const res = await api.saveSettings(snapshot);
  if (!res.ok) {
    inflight = null;
    reportFailure(res, t("settings.title"));
    notifyAll(saveStateListeners);
    return;
  }
  adoptSettings(res.value, true);
  lastSavedAt = Date.now();
  notifyAll(saveStateListeners);
}

/* ======================================================================== *
 * 初始化与事件订阅
 * ======================================================================== */

let initialized = false;
let retryTimer: number | null = null;
let retryCount = 0;

export async function initStore(): Promise<void> {
  if (initialized) return;
  initialized = true;

  applyTheme(settings.themeMode);
  applyLanguage(settings.language);
  watchSystemTheme();
  subscribeBackendEvents();

  await loadSettings();
  await refreshRuntime();
  void refreshAudioStatus();
}

/** 后端没起来时每 4s 重试一次，最多 15 次 */
function scheduleBackendRetry(): void {
  if (retryTimer !== null) return;
  if (retryCount >= 15) return;
  retryTimer = window.setTimeout(() => {
    retryTimer = null;
    retryCount += 1;
    void loadSettings().then((ok) => {
      if (ok) {
        void refreshRuntime();
        void refreshAudioStatus();
      } else {
        scheduleBackendRetry();
      }
    });
  }, 4000);
}

async function loadSettings(): Promise<boolean> {
  const res = await api.getSettings();
  if (!res.ok) {
    if (!res.canceled) {
      backendReady = false;
      if (!backendNotified) {
        backendNotified = true;
        toast("warn", t("backend.settingsUnavailable"));
      }
      scheduleBackendRetry();
    }
    return false;
  }
  backendReady = true;
  retryCount = 0;
  adoptSettings(res.value, true);
  return true;
}

async function refreshRuntime(): Promise<void> {
  const res = await api.wallpaperState();
  if (res.ok) adoptRuntime(res.value);
  else reportFailure(res);
}

async function refreshAudioStatus(): Promise<void> {
  const res = await api.audioStatus();
  if (res.ok) adoptAudioStatus(res.value);
}

/** 供「关于」页刷新 app_info */
export function applyAppInfo(info: {
  libraryDir: string;
  defaultLibraryDir: string;
}): void {
  libraryDir = info.libraryDir;
  defaultLibraryDir = info.defaultLibraryDir;
  notifyAll(libraryDirListeners);
}

function subscribeBackendEvents(): void {
  void safeListen<RuntimeState>(EV.state, (payload) => adoptRuntime(payload));
  void safeListen<MonitorState>(EV.monitor, (payload) => {
    monitor = payload;
    // monitor 也带 mode/pid/paused，补进 RuntimeState，保证顶栏不落后
    runtime = {
      ...runtime,
      attached: payload.attached,
      occluded: payload.occluded,
      paused: payload.paused,
      pid: payload.pid,
      mode: payload.mode,
      monitorIndex: payload.monitorIndex,
    };
    notifyAll(runtimeListeners);
  });
  void safeListen<AudioStatus>(EV.audioStatus, (payload) => adoptAudioStatus(payload));
  void safeListen<LogEntry>(EV.log, (payload) => {
    if (!payload || typeof payload.message !== "string") return;
    logBuffer.push(payload);
    if (logBuffer.length > MAX_LOGS) logBuffer.splice(0, logBuffer.length - MAX_LOGS);
    notifyAll(logListeners);
  });
  void safeListen<Settings>(EV.settings, (payload) => {
    // 忽略自己刚提交的那一份回声，避免滑块跳回
    if (inflight && inflight.volume === payload.volume && inflight.muted === payload.muted) {
      inflight = null;
    }
    adoptSettings(payload, true);
  });
}

/* ======================================================================== *
 * 工具：给页面用的便捷包装
 * ======================================================================== */

export { t, onLangChange, type I18nKey } from "./i18n";

/** 当前语言（页面画图/格式化时间等要用） */
export function lang(): Language {
  return getLang();
}

/** 导入进度的类型别名，方便页面直接用 */
export type { ImportProgress };
