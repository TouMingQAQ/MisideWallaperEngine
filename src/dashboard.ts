/**
 * src/dashboard.ts
 * 主界面仪表盘：左边音频节奏（频谱 + 波形 + 监听进程），右边当前壁纸预览（实时缩略图 + 操作）。
 * 设置全部收进 settings-dialog.ts 的弹窗，这里只放"看"和"最常用的操作"。
 */
import {
  api,
  EV,
  safeListen,
  type AudioFramePayload,
  type AudioStatus,
  type AudioTarget,
  type ImportProgress,
  type RuntimeState,
  type WallpaperEntry,
} from "./api";
import {
  getAudioStatus,
  getImportProgress,
  getRuntime,
  getSettings,
  isBackendReady,
  lang,
  onLangChange,
  reportFailure,
  store,
  subscribeAudioStatus,
  subscribeImportProgress,
  subscribeRuntime,
  subscribeSettings,
  t,
} from "./state";
import { clamp, clear, fmtBytes, fmtDateTime, h, icon } from "./ui";
import type { TabId } from "./tabs";
import type { SettingsDialog } from "./settings-dialog";
import { createVisualizer } from "./visualizer";
import {
  clearThumbnail,
  createThumbnailSource,
  drawThumbnail,
  type ThumbFrame,
} from "./thumbnail";

/** 音频目标列表轮询间隔 */
const TARGET_POLL_MS = 1200;
/** 峰值条的保持 / 衰减刷新间隔 */
const PEAK_TICK_MS = 160;
const PEAK_DECAY = 0.88;

export interface Dashboard {
  element: HTMLElement;
  /** 语言切换等需要整体重刷时调用 */
  refresh(): void;
  /** 首次挂载后调用：拉库列表、起轮询 */
  init(): void;
  /** 打开设置弹窗（main.ts / 空状态按钮用） */
  openSettings(tab?: TabId): void;
}

export function createDashboard(dialog: SettingsDialog): Dashboard {
  /* ==================================================================== *
   * 状态
   * ==================================================================== */

  let library: WallpaperEntry[] = [];
  let targets: AudioTarget[] = [];
  let selectedProcess = "";
  let targetPollTimer = 0;
  let peakTimer = 0;
  let targetsLoading = false;
  const peakHold = new Map<string, number>();
  /** 每行峰值条的填充元素，按进程名索引（空串 = 自动） */
  const peakBars = new Map<string, HTMLElement>();
  let lastFrame: ThumbFrame | null = null;
  let previewState: "ok" | "stopped" | "paused" | "unavailable" = "stopped";

  /* ==================================================================== *
   * 左栏：可视化
   * ==================================================================== */

  const visualizer = createVisualizer();

  const vizCard = h(
    "section",
    { cls: "card dash__viz" },
    h(
      "div",
      { cls: "card__head" },
      h(
        "div",
        {},
        h("h2", { cls: "card__title", text: t("dash.visualizer") }),
        h("p", { cls: "card__desc", text: t("dash.visualizerHint") }),
      ),
    ),
    h("div", { cls: "card__body dash__viz-body" }, visualizer.element),
  );

  /* ==================================================================== *
   * 左栏下半：当前监听进程
   * ==================================================================== */

  const audioToggleInput = h("input", { type: "checkbox" });
  audioToggleInput.checked = getSettings().audioEnabled;
  const audioToggle = h(
    "label",
    { cls: "switch", title: t("audio.enabled") },
    audioToggleInput,
    h("span", { cls: "switch__track" }),
  );
  audioToggleInput.addEventListener("change", () => {
    void store.setAudioEnabled(audioToggleInput.checked);
  });

  const openAudioSettingsBtn = h("button", {
    cls: "btn btn--sm btn--ghost",
    html: `${icon("audio", 14)}<span>${t("dialog.tabAudio")}</span>`,
  });
  openAudioSettingsBtn.addEventListener("click", () => dialog.open("audio"));

  const sourceBadge = h("span", { cls: "badge" });
  const targetList = h("div", { cls: "target-list" });
  const targetEmpty = h("div", { cls: "muted target-empty" });

  const listenBody = h(
    "div",
    { cls: "dash__listen" },
    h(
      "div",
      { cls: "dash__listen-head" },
      audioToggle,
      h("span", { cls: "dash__listen-label", text: t("audio.enabled") }),
      sourceBadge,
      h("div", { cls: "toolbar__spacer" }),
      openAudioSettingsBtn,
    ),
    h("div", { cls: "setting__hint", text: t("dash.targetHint") }),
    targetList,
    targetEmpty,
  );

  const listenCard = h(
    "section",
    { cls: "card dash__listen-card" },
    h(
      "div",
      { cls: "card__head" },
      h(
        "div",
        {},
        h("h2", { cls: "card__title", text: t("dash.listening") }),
        h("p", { cls: "card__desc", text: t("dash.targetHint") }),
      ),
    ),
    h("div", { cls: "card__body" }, listenBody),
  );

  /* ==================================================================== *
   * 右栏：当前壁纸预览
   * ==================================================================== */

  const thumbCanvas = h("canvas", { cls: "thumb__canvas" });
  const thumbInner = h("div", { cls: "thumb__inner" }, thumbCanvas);
  const thumbPlaceholder = h("div", { cls: "thumb__placeholder" });
  const thumbBox = h("div", { cls: "thumb" }, thumbInner, thumbPlaceholder);

  const wpName = h("div", { cls: "dash__wp-name" });
  const wpMeta = h("div", { cls: "dash__wp-meta" });
  const wpBadges = h("div", { cls: "row", attrs: { style: "gap:6px;flex-wrap:wrap" } });
  const wpEmpty = h("div", { cls: "dash__wp-empty" });

  const previewBtn = h("button", { cls: "btn" });
  const applyBtn = h("button", { cls: "btn btn--primary" });
  const stopBtn = h("button", { cls: "btn" });
  const reloadBtn = h("button", { cls: "btn" });
  const pauseBtn = h("button", { cls: "btn" });
  const actions = h(
    "div",
    { cls: "dash__actions" },
    previewBtn,
    applyBtn,
    stopBtn,
    reloadBtn,
    pauseBtn,
  );

  const importBtn = h("button", { cls: "btn btn--sm" });
  importBtn.addEventListener("click", () => dialog.open("library"));

  // 导入进度（拖 zip 进窗口时主界面上也要有反馈，否则用户只看到窗口"没反应"）
  const importPhase = h("span", { cls: "dash__import-phase" });
  const importPercent = h("span", { cls: "mono" });
  const importCurrent = h("div", { cls: "dash__import-current" });
  const importChip = h(
    "div",
    { cls: "dash__import" },
    h("div", { cls: "progress__top" }, importPhase, importPercent),
    h("div", { cls: "progress__track" }, h("div", { cls: "progress__fill" })),
    importCurrent,
  );
  importChip.hidden = true;

  const wallpaperCard = h(
    "section",
    { cls: "card dash__wallpaper" },
    h(
      "div",
      { cls: "card__head" },
      h(
        "div",
        {},
        h("h2", { cls: "card__title", text: t("dash.wallpaper") }),
        h("p", { cls: "card__desc", text: t("dash.previewTitle") }),
      ),
    ),
    h(
      "div",
      { cls: "card__body dash__wallpaper-body" },
      importChip,
      thumbBox,
      wpName,
      wpBadges,
      wpMeta,
      wpEmpty,
      actions,
    ),
  );

  /* ==================================================================== *
   * 页面骨架
   * ==================================================================== */

  const grid = h(
    "div",
    { cls: "dash__grid" },
    h("div", { cls: "dash__col-left" }, vizCard, listenCard),
    wallpaperCard,
  );
  const element = h("main", { cls: "dash" }, grid);

  /* ==================================================================== *
   * 缩略图
   * ==================================================================== */

  const thumbs = createThumbnailSource(
    (frame) => {
      lastFrame = frame;
      previewState = "ok";
      thumbBox.classList.remove("is-empty");
      thumbPlaceholder.hidden = true;
      drawThumbnail(thumbCanvas, frame);
    },
    () => {
      lastFrame = null;
      previewState = previewMissReason();
      showThumbnailPlaceholder();
    },
  );

  function previewMissReason(): "stopped" | "paused" | "unavailable" {
    const state = getRuntime();
    if (state.mode === "stopped" || state.pid === 0) return "stopped";
    if (state.paused || state.userPaused || state.autoPaused) return "paused";
    return "unavailable";
  }

  function showThumbnailPlaceholder(): void {
    thumbBox.classList.add("is-empty");
    clearThumbnail(thumbCanvas);
    thumbPlaceholder.hidden = false;
    clear(thumbPlaceholder);
    const text =
      previewState === "stopped"
        ? t("dash.previewStopped")
        : previewState === "paused"
          ? t("dash.previewPaused")
          : t("dash.previewUnavailable");
    thumbPlaceholder.append(
      h("div", { cls: "thumb__icon", html: icon(previewState === "paused" ? "pause" : "monitor", 22) }),
      h("div", { cls: "thumb__text", text }),
    );
  }

  /** 是否该轮询缩略图：页面可见 + 未暂停 + 有在跑的壁纸 + 没有模态盖着 */
  function shouldPoll(): boolean {
    if (document.visibilityState !== "visible") return false;
    if (dialog.isOpen()) return false;
    const state = getRuntime();
    if (state.mode === "stopped" || state.pid === 0) return false;
    if (state.paused || state.userPaused || state.autoPaused) return false;
    return true;
  }

  function syncThumbnailPolling(): void {
    if (shouldPoll()) {
      thumbs.start();
    } else {
      thumbs.stop();
      // 停掉轮询时立刻给出对应占位文案，别留一帧旧图
      previewState = previewMissReason();
      showThumbnailPlaceholder();
    }
  }

  document.addEventListener("visibilitychange", () => syncThumbnailPolling());

  const thumbObserver = new ResizeObserver(() => {
    if (lastFrame) drawThumbnail(thumbCanvas, lastFrame);
    else clearThumbnail(thumbCanvas);
  });
  thumbObserver.observe(thumbCanvas);

  /* ==================================================================== *
   * 音频目标列表
   * ==================================================================== */

  function targetStatusLabel(target: AudioTarget): string {
    const playing = target.sessionState === "active" && target.sessionPeak > 0.001;
    return playing ? t("audio.targetPlaying") : t("audio.targetSilent");
  }

  function renderTargets(): void {
    clear(targetList);
    peakBars.clear();
    const status = getAudioStatus();
    const rows: HTMLElement[] = [];

    // 「自动」一行
    rows.push(
      makeTargetRow({
        key: "",
        title: t("dash.targetAuto"),
        subtitle: t("audio.targetAutoHint"),
        badge: "",
        peak: status.source === "auto" ? Math.max(status.running ? 0.35 : 0, 0) : 0,
        checked: selectedProcess === "",
      }),
    );

    for (const target of targets) {
      const playing = target.sessionState === "active" && target.sessionPeak > 0.001;
      const media = target.mediaTitle
        ? `♪ ${target.mediaTitle}${target.mediaArtist ? ` - ${target.mediaArtist}` : ""}`
        : target.title;
      rows.push(
        makeTargetRow({
          key: target.processName,
          title: target.processName || `PID ${target.pid}`,
          subtitle: `${media || t("common.none")} · ${t("audio.targetPid", [target.pid])}`,
          badge: targetStatusLabel(target),
          badgeWarn: !playing,
          peak: playing ? target.sessionPeak : 0,
          checked: selectedProcess === target.processName,
        }),
      );
    }

    targetList.append(...rows);
    targetEmpty.hidden = targets.length > 0;
    targetEmpty.textContent = t("dash.targetsEmpty");

    const sourceName =
      status.source === "process"
        ? status.processName || t("common.unknown")
        : status.source === "auto"
          ? t("audio.sourceAuto")
          : t("audio.sourceOff");
    sourceBadge.textContent = status.pid > 0 ? `${sourceName} · PID ${status.pid}` : sourceName;
    sourceBadge.className = `badge${status.running ? " badge--ok" : ""}`;
  }

  interface TargetRowOptions {
    key: string;
    title: string;
    subtitle: string;
    badge: string;
    badgeWarn?: boolean;
    peak: number;
    checked: boolean;
  }

  function makeTargetRow(opts: TargetRowOptions): HTMLElement {
    const input = h("input", { type: "radio", attrs: { name: "audio-target" } });
    input.checked = opts.checked;
    input.addEventListener("change", () => {
      if (!input.checked) return;
      void selectTarget(opts.key);
    });

    const badge = opts.badge
      ? h("span", { cls: `badge${opts.badgeWarn ? "" : " badge--ok"}`, text: opts.badge })
      : null;

    const fill = h("div", { cls: "meter__fill" });
    fill.style.width = `${(clamp(opts.peak, 0, 1) * 100).toFixed(1)}%`;
    const peakBar = h(
      "div",
      { cls: "target__peak", title: t("dash.targetPeak") },
      h("div", { cls: "meter__track target__peak-track" }, fill),
    );
    if (opts.key) peakHold.set(opts.key, Math.max(peakHold.get(opts.key) ?? 0, opts.peak));
    peakBars.set(opts.key, fill);

    return h(
      "label",
      { cls: `target-row${opts.checked ? " is-active" : ""}` },
      input,
      h(
        "div",
        { cls: "target-row__text" },
        h("div", { cls: "target-row__name", text: opts.title }),
        h("div", { cls: "target-row__sub", text: opts.subtitle }),
      ),
      badge,
      peakBar,
    );
  }

  async function selectTarget(processName: string): Promise<void> {
    if (processName === selectedProcess) return;
    selectedProcess = processName;
    renderTargets();
    await store.setAudioTarget(processName);
  }

  async function refreshTargets(): Promise<void> {
    if (targetsLoading) return;
    targetsLoading = true;
    try {
      const res = await api.listAudioTargets();
      if (res.ok) {
        targets = res.value;
        renderTargets();
      } else if (!res.canceled && !res.unavailable) {
        reportFailure(res);
      }
    } finally {
      targetsLoading = false;
    }
  }

  function tickPeaks(): void {
    for (const [key, fill] of peakBars) {
      const current = peakHold.get(key) ?? 0;
      const next = current * PEAK_DECAY;
      peakHold.set(key, next);
      fill.style.width = `${(clamp(next, 0, 1) * 100).toFixed(1)}%`;
    }
  }

  /* ==================================================================== *
   * 导入进度
   * ==================================================================== */

  function renderImportProgress(): void {
    const progress = getImportProgress();
    if (!progress) {
      importChip.hidden = true;
      return;
    }
    importChip.hidden = false;
    const labels: Record<ImportProgress["phase"], string> = {
      reading: t("library.progress.reading"),
      extracting: t("library.progress.extracting"),
      finalizing: t("library.progress.finalizing"),
      done: t("library.progress.done"),
    };
    const pct = Number.isFinite(progress.percent)
      ? Math.min(100, Math.max(0, progress.percent))
      : 0;
    importPhase.textContent = labels[progress.phase] ?? progress.phase;
    const counts =
      progress.totalFiles > 0
        ? ` · ${t("library.progress.files", [progress.files, progress.totalFiles])}`
        : "";
    importPercent.textContent = `${pct.toFixed(0)}%${counts}`;
    importCurrent.textContent = progress.current || "";
    importCurrent.hidden = progress.current === "";
    const fill = importChip.querySelector<HTMLElement>(".progress__fill");
    if (fill) fill.style.width = `${pct}%`;
  }

  /* ==================================================================== *
   * 壁纸卡片
   * ==================================================================== */

  function activeEntry(): WallpaperEntry | null {
    const id = getRuntime().wallpaperId || getSettings().activeWallpaper;
    if (!id) return null;
    return library.find((item) => item.id === id) ?? null;
  }

  function activeId(): string {
    return getRuntime().wallpaperId || getSettings().activeWallpaper;
  }

  function renderWallpaper(): void {
    const entry = activeEntry();
    const id = activeId();
    const state = getRuntime();
    const backendUp = isBackendReady();
    const running = state.mode !== "stopped" && state.pid !== 0;

    wpName.textContent = entry ? entry.name : id || t("dash.noWallpaper");
    clear(wpBadges);
    if (entry) {
      if (entry.unity) {
        wpBadges.append(
          h("span", {
            cls: "badge badge--unity",
            text: entry.unityVersion
              ? t("library.card.unity", [entry.unityVersion])
              : t("library.card.unityUnknown"),
          }),
        );
      } else {
        wpBadges.append(h("span", { cls: "badge", text: t("library.card.notUnity") }));
      }
      if (entry.broken) {
        wpBadges.append(h("span", { cls: "badge badge--danger", text: t("library.card.broken") }));
      }
      wpBadges.append(
        h("span", { cls: "badge", text: t("library.card.files", [entry.fileCount]) }),
        h("span", { cls: "badge", text: fmtBytes(entry.sizeBytes) }),
        h("span", { cls: "badge", text: t("library.card.imported", [fmtDateTime(entry.importedAt, lang())]) }),
      );
    }
    wpMeta.hidden = !entry;

    const empty = !entry;
    wpEmpty.hidden = !empty;
    clear(wpEmpty);
    if (empty) {
      const openLibrary = h("button", {
        cls: "btn btn--sm",
        html: `${icon("library", 14)}<span>${t("dash.openLibrary")}</span>`,
      });
      openLibrary.addEventListener("click", () => dialog.open("library"));
      wpEmpty.append(
        h("div", { text: t("dash.noWallpaperHint") }),
        h("div", { cls: "row", attrs: { style: "margin-top:10px" } }, importBtn, openLibrary),
      );
    }

    const runnable = !!entry && !entry.broken && !entry.missingExe;
    previewBtn.innerHTML = `${icon("play", 14)}<span>${t("action.preview")}</span>`;
    previewBtn.disabled = !runnable || state.mode === "preview" || !backendUp;
    applyBtn.innerHTML = `${icon("desktop", 14)}<span>${t("action.apply")}</span>`;
    applyBtn.disabled = !runnable || state.mode === "desktop" || !backendUp;
    stopBtn.innerHTML = `${icon("stop", 14)}<span>${t("action.stop")}</span>`;
    stopBtn.disabled = !running || !backendUp;
    reloadBtn.innerHTML = `${icon("reload", 14)}<span>${t("action.reload")}</span>`;
    reloadBtn.disabled = !running || !backendUp;
    const paused = state.userPaused || state.autoPaused || state.paused;
    pauseBtn.innerHTML = paused
      ? `${icon("play", 14)}<span>${t("action.resume")}</span>`
      : `${icon("pause", 14)}<span>${t("action.pause")}</span>`;
    pauseBtn.disabled = !running || !backendUp;

    // 停止 / 重载 / 暂停直接执行（契约里的 stop_wallpaper 后端自己会停进程，不吞并壁纸库数据）
    stopBtn.onclick = () => void store.stop();
    reloadBtn.onclick = () => void store.reload();
    pauseBtn.onclick = () => void store.togglePause();
    previewBtn.onclick = () => {
      if (id) void store.preview(id);
    };
    applyBtn.onclick = () => {
      if (id) void store.apply(id);
    };

    // 缩略图占位（未运行 / 暂停 / 抓不到）
    if (!shouldPoll()) {
      previewState = previewMissReason();
      showThumbnailPlaceholder();
    }
  }

  /* ==================================================================== *
   * 文案刷新
   * ==================================================================== */

  function refresh(): void {
    const audioOn = getSettings().audioEnabled;
    audioToggleInput.checked = audioOn;
    const vizTitle = vizCard.querySelector(".card__title");
    const vizDesc = vizCard.querySelector(".card__desc");
    if (vizTitle) vizTitle.textContent = t("dash.visualizer");
    if (vizDesc) vizDesc.textContent = audioOn ? t("dash.visualizerHint") : t("dash.audioOff");
    const listenTitle = listenCard.querySelector(".card__title");
    const listenDesc = listenCard.querySelector(".card__desc");
    if (listenTitle) listenTitle.textContent = t("dash.listening");
    if (listenDesc) listenDesc.textContent = t("dash.targetHint");
    listenBody.querySelector(".dash__listen-label")!.textContent = t("audio.enabled");
    openAudioSettingsBtn.innerHTML = `${icon("audio", 14)}<span>${t("dialog.tabAudio")}</span>`;
    const wpTitle = wallpaperCard.querySelector(".card__title");
    const wpDesc = wallpaperCard.querySelector(".card__desc");
    if (wpTitle) wpTitle.textContent = t("dash.wallpaper");
    if (wpDesc) wpDesc.textContent = t("dash.previewTitle");
    importBtn.innerHTML = `${icon("download", 14)}<span>${t("dash.import")}</span>`;
    visualizer.refreshTexts();
    renderTargets();
    renderWallpaper();
    renderImportProgress();
  }

  /* ==================================================================== *
   * 事件订阅
   * ==================================================================== */

  const unsubs: Array<() => void> = [];
  unsubs.push(
    onLangChange(() => refresh()),
    subscribeRuntime(() => {
      renderWallpaper();
      syncThumbnailPolling();
    }),
    subscribeSettings(() => {
      renderWallpaper();
      syncThumbnailPolling();
    }),
    subscribeAudioStatus(() => renderTargets()),
    subscribeImportProgress(() => renderImportProgress()),
    dialog.onOpenChange(() => syncThumbnailPolling()),
  );

  void safeListen<WallpaperEntry[]>(EV.library, (payload) => {
    if (!Array.isArray(payload)) return;
    library = payload;
    renderWallpaper();
  });
  void safeListen<AudioFramePayload>(EV.audioFrame, (payload) => visualizer.push(payload));
  void safeListen<AudioStatus>(EV.audioStatus, (payload) => {
    if (payload) renderTargets();
  });
  void safeListen<RuntimeState>(EV.state, () => syncThumbnailPolling());

  /* ==================================================================== *
   * 轮询
   * ==================================================================== */

  function startPolling(): void {
    if (targetPollTimer === 0) {
      void refreshTargets();
      targetPollTimer = window.setInterval(() => void refreshTargets(), TARGET_POLL_MS);
    }
    if (peakTimer === 0) {
      peakTimer = window.setInterval(() => tickPeaks(), PEAK_TICK_MS);
    }
  }

  function stopPolling(): void {
    if (targetPollTimer !== 0) {
      window.clearInterval(targetPollTimer);
      targetPollTimer = 0;
    }
    if (peakTimer !== 0) {
      window.clearInterval(peakTimer);
      peakTimer = 0;
    }
  }

  document.addEventListener("visibilitychange", () => {
    if (document.visibilityState === "visible") {
      startPolling();
      void refreshTargets();
    } else {
      stopPolling();
    }
  });

  /* ==================================================================== *
   * 初始化
   * ==================================================================== */

  function init(): void {
    selectedProcess = getSettings().audioSource === "process" ? getSettings().audioProcess : "";
    refresh();
    startPolling();
    syncThumbnailPolling();
    void api.libraryList().then((res) => {
      if (res.ok) {
        library = res.value;
        renderWallpaper();
      }
    });
  }

  return {
    element,
    refresh,
    init,
    openSettings(tab?: TabId): void {
      dialog.open(tab);
    },
  };
}
