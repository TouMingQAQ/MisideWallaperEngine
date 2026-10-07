/**
 * src/about.ts
 * 设置弹窗「关于」tab：app_info 卡片、运行状态、wp://log 滚动日志（最多 400 条）、打开日志目录。
 */
import { api, type AppInfo, type LogEntry } from "./api";
import {
  applyAppInfo,
  clearLogs,
  getLogs,
  getRuntime,
  onLangChange,
  store,
  subscribeLogs,
  subscribeRuntime,
  t,
} from "./state";
import { card, clear, fmtBytes, fmtClock, h, icon } from "./ui";
import type { Tab } from "./tabs";

export function createAboutTab(): Tab {
  let info: AppInfo | null = null;
  /** 已经画进 DOM 的日志条数，用来只追加新行 */
  let drawnLogs = 0;
  let viewedLogs: LogEntry[] = [];

  /* ------------------------------------------------------------ 结构 */

  const infoBody = h("div", { cls: "kv" });
  const loadingLine = h("div", { cls: "muted", text: t("common.loading") });
  const infoCard = card(t("about.appInfo"), "", h("div", {}, infoBody, loadingLine));

  const runtimeBody = h("div", {});
  const runtimeCard = card(t("about.runtime"), "", runtimeBody);

  const logCount = h("span", { cls: "muted" });
  const clearLogBtn = h("button", {
    cls: "btn btn--sm",
    html: `${icon("trash", 14)}<span>${t("about.clearLog")}</span>`,
  });
  clearLogBtn.addEventListener("click", () => {
    clearLogs();
    drawnLogs = 0;
    viewedLogs = [];
    clear(logBox);
    renderLogFooter();
  });

  const openLogDirBtn = h("button", {
    cls: "btn btn--sm",
    html: `${icon("folderOpen", 14)}<span>${t("about.openLogDir")}</span>`,
  });
  openLogDirBtn.addEventListener("click", () => {
    void (async () => {
      openLogDirBtn.disabled = true;
      try {
        await store.openLogDir();
      } finally {
        openLogDirBtn.disabled = false;
      }
    })();
  });

  const logBox = h("div", { cls: "logbox" });
  const logEmpty = h("div", { cls: "muted", text: t("about.logEmpty") });

  const logCard = card(
    t("about.log"),
    "",
    h("div", {}, h("div", { cls: "row", attrs: { style: "margin-bottom:8px" } }, logCount, h("div", { cls: "toolbar__spacer" }), openLogDirBtn, clearLogBtn), logBox, logEmpty),
  );

  const note = h("div", { cls: "about-note", text: t("about.note") });

  const body = h(
    "div",
    { cls: "page" },
    h("div", { cls: "grid-2" }, infoCard.element, runtimeCard.element),
    logCard.element,
    note,
  );

  /* ------------------------------------------------------------ 应用信息 */

  function kv(key: string, value: string, mono = false, cls = ""): HTMLElement {
    return h(
      "div",
      { attrs: { style: "display:contents" } },
      h("div", { cls: "kv__k", text: key }),
      h("div", { cls: `kv__v${mono ? " mono" : ""} ${cls}`.trim(), text: value }),
    );
  }

  function renderInfo(): void {
    clear(infoBody);
    if (!info) {
      loadingLine.hidden = false;
      return;
    }
    loadingLine.hidden = true;
    infoBody.append(
      kv(t("about.version"), info.version || t("common.unknown"), true),
      kv(
        t("about.dll"),
        info.dllOk ? t("about.dllReady", [info.dllVersion]) : t("about.dllMissing"),
        true,
        info.dllOk ? "" : "dim",
      ),
      kv(t("about.dllPath"), info.dllPath || "—", true),
      kv(t("about.libraryDir"), info.libraryDir || t("common.builtin"), true),
      kv(t("about.defaultLibraryDir"), info.defaultLibraryDir || "—", true),
      kv(t("about.wallpaperCount"), String(info.wallpaperCount)),
      kv(t("about.totalBytes"), fmtBytes(info.totalBytes)),
      kv(t("about.autostart"), info.autostart ? t("common.yes") : t("common.no")),
      kv(t("about.envTitle"), t("about.env")),
    );
  }

  async function refreshInfo(): Promise<void> {
    const res = await api.appInfo();
    if (!res.ok) {
      if (!res.canceled) {
        info = null;
        loadingLine.hidden = false;
        loadingLine.textContent = t("backend.unavailable");
      }
      return;
    }
    info = res.value;
    applyAppInfo({ libraryDir: info.libraryDir, defaultLibraryDir: info.defaultLibraryDir });
    renderInfo();
  }

  /* ------------------------------------------------------------ 运行时 */

  function renderRuntime(): void {
    const state = getRuntime();
    clear(runtimeBody);
    const modeLabel =
      state.mode === "desktop"
        ? t("status.desktop")
        : state.mode === "preview"
          ? t("status.preview")
          : t("status.stopped");
    if (state.mode === "stopped" || state.pid === 0) {
      runtimeBody.append(h("div", { cls: "dim", text: t("about.runtimeStopped") }));
      return;
    }
    runtimeBody.append(
      h("div", {
        text: t("about.runtimePiece", [
          state.pid,
          modeLabel,
          state.memoryMb > 0 ? state.memoryMb : "—",
        ]),
      }),
      h("div", { cls: "muted mono", text: state.exe || "—" }),
      h(
        "div",
        { cls: "row", attrs: { style: "margin-top:8px" } },
        h("span", {
          cls: `badge${state.attached ? " badge--ok" : ""}`,
          text: t("status.desktop"),
        }),
        h("span", {
          cls: `badge${state.paused ? " badge--warn" : ""}`,
          text: state.userPaused
            ? t("status.pausedUser")
            : state.autoPaused
              ? t("status.pausedAuto")
              : t("action.pause"),
        }),
        state.occluded ? h("span", { cls: "badge badge--warn", text: t("status.occluded") }) : null,
      ),
    );
  }

  /* ------------------------------------------------------------ 日志 */

  function renderLogFooter(): void {
    logCount.textContent = t("about.logCount", [viewedLogs.length]);
    logEmpty.hidden = viewedLogs.length > 0;
    clearLogBtn.disabled = viewedLogs.length === 0;
  }

  function renderLogs(force: boolean): void {
    viewedLogs = getLogs();
    if (force) {
      clear(logBox);
      drawnLogs = 0;
    }
    if (viewedLogs.length < drawnLogs) {
      clear(logBox);
      drawnLogs = 0;
    }
    for (let i = drawnLogs; i < viewedLogs.length; i += 1) {
      const entry = viewedLogs[i];
      if (!entry) continue;
      const levelCls =
        entry.level === "error" ? "logline--error" : entry.level === "warn" ? "logline--warn" : "logline--info";
      logBox.append(
        h(
          "div",
          { cls: `logline ${levelCls}` },
          h("span", { cls: "logline__time", text: fmtClock(entry.at) }),
          h("span", { cls: "logline__msg", text: entry.message }),
        ),
      );
    }
    drawnLogs = viewedLogs.length;
    renderLogFooter();
    logBox.scrollTop = logBox.scrollHeight;
  }

  /* ------------------------------------------------------------ 渲染 */

  function renderStatic(): void {
    infoCard.setText(t("about.appInfo"), "");
    runtimeCard.setText(t("about.runtime"), "");
    logCard.setText(t("about.log"), "");
    openLogDirBtn.innerHTML = `${icon("folderOpen", 14)}<span>${t("about.openLogDir")}</span>`;
    clearLogBtn.innerHTML = `${icon("trash", 14)}<span>${t("about.clearLog")}</span>`;
    note.textContent = t("about.note");
    logEmpty.textContent = t("about.logEmpty");
    if (!info) loadingLine.textContent = t("common.loading");
    renderInfo();
    renderRuntime();
    renderLogs(true);
  }

  /* ------------------------------------------------------------ 订阅 */

  const unsubs: Array<() => void> = [];
  unsubs.push(
    subscribeLogs(() => renderLogs(false)),
    subscribeRuntime(() => renderRuntime()),
    onLangChange(() => renderStatic()),
  );

  renderStatic();
  void refreshInfo();

  return {
    element: body,
    onShow(): void {
      renderStatic();
      void refreshInfo();
    },
    destroy(): void {
      for (const un of unsubs) un();
      unsubs.length = 0;
    },
  };
}
