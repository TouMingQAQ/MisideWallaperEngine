/**
 * src/main.ts
 * 入口：单页仪表盘 + 设置弹窗 + 拖放导入 + 吐司。
 * 左侧导航已取消，所有设置都在 settings-dialog.ts 的弹窗里（壁纸库 / 音频 / 通用 / 关于）。
 */
import { api, EV, safeListen, type AppInfo, type DragDropPayload } from "./api";
import {
  getRuntime,
  initStore,
  isBackendReady,
  lang,
  onLangChange,
  setImportProgress,
  setToastSink,
  store,
  subscribeRuntime,
  subscribeSettings,
  t,
  type Toast,
} from "./state";
import { clear, h, icon } from "./ui";
import { createDashboard } from "./dashboard";
import { createSettingsDialog } from "./settings-dialog";

/* ======================================================================== *
 * 顶栏：应用名 + 版本 · 状态胶囊 · 暂停/静音/音量 · 设置按钮
 * ======================================================================== */

function createTopBar(onOpenSettings: () => void): {
  element: HTMLElement;
  update(): void;
  setVersion(version: string): void;
} {
  const dot = h("span", { cls: "status__dot" });
  const modeText = h("span");
  const statusBits = h("span", { cls: "row", attrs: { style: "gap:6px" } });
  const chip = h("div", { cls: "status" }, dot, modeText, statusBits);

  const brandName = h("div", { cls: "brand__name" });
  const brandVersion = h("div", { cls: "brand__version" });
  const brand = h(
    "div",
    { cls: "brand" },
    h("div", { cls: "brand__badge", text: "M" }),
    h("div", {}, brandName, brandVersion),
  );

  const pauseBtn = h("button", { cls: "btn btn--icon" });
  const muteBtn = h("button", { cls: "btn btn--icon" });
  const settingsBtn = h("button", { cls: "btn" });
  settingsBtn.addEventListener("click", onOpenSettings);

  const volumeRange = h("input", { type: "range" });
  volumeRange.min = "0";
  volumeRange.max = "1";
  volumeRange.step = "0.01";
  volumeRange.style.width = "110px";

  let draggingVolume = false;
  volumeRange.addEventListener("pointerdown", () => {
    draggingVolume = true;
  });
  volumeRange.addEventListener("pointerup", () => {
    draggingVolume = false;
  });
  volumeRange.addEventListener("input", () => {
    void store.setVolume(Number(volumeRange.value));
  });

  const volumeBox = h("div", { cls: "topbar__volume" }, pauseBtn, muteBtn, volumeRange);
  const element = h(
    "header",
    { cls: "topbar" },
    brand,
    chip,
    h("div", { cls: "topbar__actions" }, volumeBox, settingsBtn),
  );

  let version = "0.0.2";

  function renderStatic(): void {
    brandName.textContent = t("app.title");
    brandVersion.textContent = t("app.version", [version]);
    settingsBtn.innerHTML = `${icon("settings", 15)}<span>${t("app.settings")}</span>`;
    settingsBtn.title = t("app.settings");
  }

  function renderStatus(): void {
    const state = getRuntime();
    dot.className =
      state.mode === "desktop"
        ? "status__dot status__dot--desktop"
        : state.mode === "preview"
          ? "status__dot status__dot--preview"
          : "status__dot";
    modeText.textContent =
      state.mode === "desktop"
        ? t("status.desktop")
        : state.mode === "preview"
          ? t("status.preview")
          : t("status.stopped");

    clear(statusBits);
    if (state.mode === "stopped" || state.pid === 0) return;
    statusBits.append(h("span", { cls: "status__pill", text: t("status.pid", [state.pid]) }));
    if (state.userPaused) {
      statusBits.append(
        h("span", { cls: "status__pill status__pill--warn", text: t("status.pausedUser") }),
      );
    } else if (state.autoPaused) {
      statusBits.append(
        h("span", { cls: "status__pill status__pill--warn", text: t("status.pausedAuto") }),
      );
    }
    if (state.occluded) {
      statusBits.append(
        h("span", { cls: "status__pill status__pill--warn", text: t("status.occluded") }),
      );
    }
    if (state.memoryMb > 0) {
      statusBits.append(
        h("span", { cls: "muted", text: t("status.memory", [Math.round(state.memoryMb)]) }),
      );
    }
  }

  function renderButtons(): void {
    const state = getRuntime();
    const running = state.mode !== "stopped" && state.pid !== 0;
    const backendUp = isBackendReady();
    const paused = state.userPaused || state.autoPaused || state.paused;

    pauseBtn.innerHTML = icon(paused ? "play" : "pause", 15);
    pauseBtn.title = paused ? t("action.resume") : t("action.pause");
    pauseBtn.disabled = !running || !backendUp;

    muteBtn.innerHTML = icon(state.muted ? "mute" : "volume", 15);
    muteBtn.title = state.muted ? t("action.unmute") : t("action.mute");

    if (!draggingVolume) volumeRange.value = String(Math.min(1, Math.max(0, state.volume || 0)));
  }

  pauseBtn.addEventListener("click", () => void store.togglePause());
  muteBtn.addEventListener("click", () => void store.setMuted(!getRuntime().muted));

  function update(): void {
    renderStatic();
    renderStatus();
    renderButtons();
  }

  return {
    element,
    update,
    setVersion(next: string): void {
      version = next || version;
      renderStatic();
    },
  };
}

/* ======================================================================== *
 * 吐司
 * ======================================================================== */

function mountToasts(host: HTMLElement): void {
  const live: HTMLDivElement[] = [];
  setToastSink((toast: Toast) => {
    const el = h("div", { cls: `toast toast--${toast.kind}`, text: toast.message });
    host.append(el);
    live.push(el);
    while (live.length > 5) live.shift()?.remove();
    window.setTimeout(() => {
      el.style.opacity = "0";
      el.style.transition = "opacity .25s";
      window.setTimeout(() => {
        el.remove();
        const idx = live.indexOf(el);
        if (idx >= 0) live.splice(idx, 1);
      }, 260);
    }, 4600);
  });
}

/** 拖放这类没有组件引用的场景用的轻量吐司 */
function rawToast(kind: "info" | "ok" | "warn" | "error", message: string): void {
  const host = document.getElementById("toasts");
  if (!host) {
    console.warn(`[wp] ${kind}: ${message}`);
    return;
  }
  const el = h("div", { cls: `toast toast--${kind}`, text: message });
  host.append(el);
  window.setTimeout(() => el.remove(), 4600);
}

/* ======================================================================== *
 * 启动
 * ======================================================================== */

function boot(): void {
  const viewHost = document.getElementById("view");
  const topbarHost = document.getElementById("topbar");
  const toastsHost = document.getElementById("toasts");
  const overlay = document.getElementById("drop-overlay");
  const overlayText = document.getElementById("drop-overlay-text");
  const hintHost = document.getElementById("tray-hint");

  if (!viewHost || !topbarHost || !toastsHost || !overlay || !overlayText) {
    console.error("[wp] 页面骨架缺失，无法启动界面");
    return;
  }

  mountToasts(toastsHost);

  /* ---- 弹窗 + 仪表盘 ---- */
  const dialog = createSettingsDialog();
  const dashboard = createDashboard(dialog);
  const topBar = createTopBar(() => dialog.open());

  topbarHost.replaceWith(topBar.element);
  viewHost.append(dashboard.element);
  dashboard.init();
  topBar.update();
  if (hintHost) hintHost.textContent = t("app.trayHint");

  /* ---- 订阅 ---- */
  subscribeRuntime(() => topBar.update());
  subscribeSettings(() => topBar.update());
  onLangChange(() => {
    if (hintHost) hintHost.textContent = t("app.trayHint");
    topBar.update();
  });

  /* ---- 拖放导入 ---- */
  let importingNow = false;

  async function importPath(path: string): Promise<void> {
    if (importingNow) return;
    if (!path.toLowerCase().endsWith(".zip")) {
      rawToast("warn", t("library.dropBad"));
      return;
    }
    importingNow = true;
    setImportProgress({
      phase: "reading",
      percent: 0,
      files: 0,
      totalFiles: 0,
      current: path,
      id: "",
    });
    try {
      const res = await api.importZip(path);
      if (!res.ok) {
        rawToast("error", res.unavailable ? t("backend.unavailable") : res.error);
        return;
      }
      rawToast("ok", t("library.imported", [res.value.name]));
      // 导入完成 → 打开设置弹窗停在「壁纸库」
      dialog.open("library");
    } catch (err) {
      console.error("[wp] drop import failed", err);
      rawToast("error", String(err));
    } finally {
      importingNow = false;
      setImportProgress(null);
    }
  }

  void safeListen<DragDropPayload>(EV.dragEnter, (payload) => {
    overlay.hidden = false;
    const count = Array.isArray(payload?.paths) ? payload.paths.length : 0;
    overlayText.textContent =
      count > 0 ? `${t("library.dropHint")}（${count}）` : t("library.dropHint");
  });
  void safeListen<DragDropPayload>(EV.dragOver, () => {
    overlay.hidden = false;
  });
  void safeListen<DragDropPayload>(EV.dragLeave, () => {
    overlay.hidden = true;
  });
  void safeListen<DragDropPayload>(EV.dragDrop, (payload) => {
    overlay.hidden = true;
    const paths = Array.isArray(payload?.paths) ? payload.paths : [];
    const zips = paths.filter((p) => typeof p === "string" && p.toLowerCase().endsWith(".zip"));
    if (zips.length === 0) {
      if (paths.length > 0) rawToast("warn", t("library.dropBad"));
      return;
    }
    void (async () => {
      for (const zip of zips) await importPath(zip);
    })();
  });

  /* ---- 初始化：拿设置 + 版本号 ---- */
  void (async () => {
    await initStore();
    topBar.update();
    dashboard.refresh();
    if (!isBackendReady()) console.warn("[wp] 后端未就绪，界面以降级模式运行");

    const info: AppInfo | null = await api.appInfo().then((res) => (res.ok ? res.value : null));
    if (info) topBar.setVersion(info.version);
  })();

  document.documentElement.lang = lang();
}

if (document.readyState === "loading") {
  document.addEventListener("DOMContentLoaded", boot);
} else {
  boot();
}
