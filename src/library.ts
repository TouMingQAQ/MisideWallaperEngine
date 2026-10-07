/**
 * src/library.ts
 * 设置弹窗「壁纸库」tab：导入 / 搜索 / 卡片网格 / 每张卡片的操作。
 * 拖放导入的整窗高亮由 main.ts 负责，进度条 DOM 在这里。
 */
import { api, EV, safeListen, type ImportProgress, type WallpaperEntry } from "./api";
import {
  getImportProgress,
  getLibraryDir,
  getRuntime,
  lang,
  onLangChange,
  patchSettings,
  reportFailure,
  setImportProgress,
  store,
  subscribeImportProgress,
  subscribeLibraryDir,
  subscribeRuntime,
  subscribeSettings,
  t,
  toast,
} from "./state";
import { clear, fmtBytes, fmtRelative, h, icon, modalConfirm, modalPrompt } from "./ui";
import type { Tab } from "./tabs";

export function createLibraryTab(): Tab {
  let entries: WallpaperEntry[] = [];
  let filter = "";
  let importing = false;
  let loaded = false;
  /** 上次渲染的数据指纹，避免无意义的整表重建 */
  let lastKey = "";

  /* ------------------------------------------------------------ 结构 */

  const importBtn = h("button", { cls: "btn btn--primary" });
  const refreshBtn = h("button", { cls: "btn" });
  const dirBtn = h("button", { cls: "btn" });
  const searchInput = h("input", {
    cls: "input",
    type: "search",
    attrs: { style: "width:220px" },
  });

  const toolbar = h(
    "div",
    { cls: "toolbar" },
    importBtn,
    refreshBtn,
    dirBtn,
    h("div", { cls: "toolbar__spacer" }),
    searchInput,
  );

  const dirLine = h("div", { cls: "muted mono" });

  const progressPhase = h("span", { text: "" });
  const progressPct = h("span", { cls: "mono", text: "" });
  const progressCurrent = h("div", { cls: "muted", text: "", attrs: { style: "margin-top:6px" } });
  const progressFill = h("div", { cls: "progress__fill" });
  const progressBox = h(
    "div",
    { cls: "progress" },
    h("div", { cls: "progress__top" }, progressPhase, progressPct),
    h("div", { cls: "progress__track" }, progressFill),
    progressCurrent,
  );
  progressBox.hidden = true;

  const summary = h("div", { cls: "muted" });
  const grid = h("div", { cls: "lib-grid" });
  const emptyBox = h("div", { cls: "empty" });
  emptyBox.hidden = true;

  // tab 名就是「壁纸库」，不再自带标题，直接上工具条
  const body = h(
    "div",
    { cls: "page" },
    toolbar,
    dirLine,
    progressBox,
    summary,
    h("div", {}, grid, emptyBox),
  );

  /* ------------------------------------------------------------ 渲染 */

  function visibleEntries(): WallpaperEntry[] {
    const q = filter.trim().toLowerCase();
    if (!q) return entries;
    return entries.filter((e) => e.name.toLowerCase().includes(q));
  }

  function renderStatic(): void {
    importBtn.innerHTML = `${icon("download")}<span>${
      importing ? t("library.importing") : t("library.import")
    }</span>`;
    importBtn.disabled = importing;
    refreshBtn.innerHTML = `${icon("reload")}<span>${t("library.refresh")}</span>`;
    dirBtn.innerHTML = `${icon("folderOpen")}<span>${t("library.pickDir")}</span>`;
    searchInput.placeholder = t("library.searchPlaceholder");
  }

  function renderGrid(force: boolean): void {
    const list = visibleEntries();
    const activeId = getRuntime().wallpaperId;
    const key = `${lang()}|${filter}|${activeId}|${list
      .map((e) => `${e.id}:${e.name}:${e.broken ? 1 : 0}:${e.missingExe ? 1 : 0}:${e.sizeBytes}`)
      .join(",")}`;
    if (!force && key === lastKey) return;
    lastKey = key;

    clear(grid);
    clear(emptyBox);

    if (list.length === 0) {
      grid.hidden = true;
      emptyBox.hidden = false;
      if (filter.trim()) {
        emptyBox.append(
          h("div", { cls: "empty__title", text: t("library.emptyFiltered", [filter.trim()]) }),
        );
      } else {
        emptyBox.append(
          h("div", { cls: "empty__icon", html: icon("download", 30) }),
          h("div", { cls: "empty__title", text: t("library.emptyTitle") }),
          h("div", { text: t("library.emptyHint") }),
        );
      }
      return;
    }

    grid.hidden = false;
    emptyBox.hidden = true;
    for (const entry of list) grid.append(renderCard(entry, entry.id === activeId));
  }

  function renderCard(entry: WallpaperEntry, isActive: boolean): HTMLElement {
    const logo = h("div", {
      cls: `wp-card__logo${entry.unity ? " wp-card__logo--unity" : ""}`,
      text: entry.unity ? "U" : "?",
      title: entry.unity ? t("library.card.unityUnknown") : t("library.card.notUnity"),
    });

    const meta = h("div", {
      cls: "wp-card__meta",
      text: `${fmtBytes(entry.sizeBytes)} · ${t("library.card.imported", [
        fmtRelative(entry.importedAt, lang()),
      ])}`,
    });

    const top = h(
      "div",
      { cls: "wp-card__top" },
      logo,
      h(
        "div",
        { style: "min-width:0;flex:1" },
        h("div", { cls: "wp-card__name", text: entry.name, title: entry.dir }),
        meta,
      ),
    );

    const badges = h("div", { cls: "wp-card__badges" });
    if (entry.unity) {
      badges.append(
        h("span", {
          cls: "badge badge--unity",
          text: entry.unityVersion
            ? t("library.card.unity", [entry.unityVersion])
            : t("library.card.unityUnknown"),
        }),
      );
    } else {
      badges.append(h("span", { cls: "badge", text: t("library.card.notUnity") }));
    }
    if (entry.broken) {
      badges.append(h("span", { cls: "badge badge--danger", text: t("library.card.broken") }));
    }
    if (entry.missingExe) {
      badges.append(h("span", { cls: "badge badge--warn", text: t("library.card.missingExe") }));
    }
    if (isActive) {
      badges.append(h("span", { cls: "badge badge--ok", text: t("library.card.active") }));
    }
    badges.append(h("span", { cls: "badge", text: t("library.card.files", [entry.fileCount]) }));

    const runnable = !entry.broken && !entry.missingExe && entry.exe !== "";

    const applyBtn = h("button", {
      cls: "btn btn--sm btn--primary",
      html: `${icon("desktop", 14)}<span>${t("action.apply")}</span>`,
      title: runnable ? t("action.apply") : t("library.card.noExe"),
    });
    applyBtn.disabled = !runnable;
    applyBtn.addEventListener("click", () => {
      void withBusy(applyBtn, async () => {
        const state = await store.apply(entry.id);
        if (state) toast("ok", t("library.applied", [entry.name]));
      });
    });

    const previewBtn = h("button", {
      cls: "btn btn--sm",
      html: `${icon("play", 14)}<span>${t("action.preview")}</span>`,
      title: runnable ? t("action.preview") : t("library.card.noExe"),
    });
    previewBtn.disabled = !runnable;
    previewBtn.addEventListener("click", () => {
      void withBusy(previewBtn, async () => {
        const state = await store.preview(entry.id);
        if (state) toast("ok", t("library.previewing", [entry.name]));
      });
    });

    const revealBtn = h("button", {
      cls: "btn btn--sm btn--ghost btn--icon",
      html: icon("folderOpen", 14),
      title: t("library.card.openDir"),
    });
    revealBtn.addEventListener("click", () => {
      void withBusy(revealBtn, async () => {
        const res = await api.revealWallpaper(entry.id);
        if (res.ok) toast("info", t("library.revealed"));
        else reportFailure(res, t("library.card.openDir"));
      });
    });

    const renameBtn = h("button", {
      cls: "btn btn--sm btn--ghost btn--icon",
      html: icon("pencil", 14),
      title: t("library.card.rename"),
    });
    renameBtn.addEventListener("click", () => {
      void (async () => {
        const name = await modalPrompt({
          title: t("library.renameTitle"),
          message: t("library.renameMessage", [entry.name]),
          value: entry.name,
          placeholder: t("library.renamePrompt"),
          confirmText: t("common.confirm"),
          cancelText: t("common.cancel"),
          validate: (value) => {
            if (value === "") return t("library.renameEmpty");
            if (value === entry.name) return t("library.renameSame");
            return null;
          },
        });
        if (name === null || name === entry.name) return;
        await withBusy(renameBtn, async () => {
          const res = await api.renameWallpaper(entry.id, name);
          if (!res.ok) {
            reportFailure(res, t("library.card.rename"));
            return;
          }
          toast("ok", t("library.renamed", [res.value.name]));
          await refresh();
        });
      })();
    });
    const removeBtn = h("button", {
      cls: "btn btn--sm btn--ghost btn--icon btn--danger",
      html: icon("trash", 14),
      title: t("library.card.remove"),
    });
    removeBtn.addEventListener("click", () => {
      void (async () => {
        const running = getRuntime().wallpaperId === entry.id;
        const lines = [t("library.removeMessage", [entry.name])];
        if (running) lines.push(t("library.removeActiveHint"));
        const yes = await modalConfirm({
          title: t("library.removeTitle"),
          message: lines.join("\n\n"),
          confirmText: t("common.delete"),
          cancelText: t("common.cancel"),
          danger: true,
        });
        if (!yes) return;
        await withBusy(removeBtn, async () => {
          // 正在使用这个壁纸时先停掉，避免文件被占用删不干净
          if (getRuntime().wallpaperId === entry.id) {
            const stopped = await store.stop();
            if (stopped === null) return; // 停止失败已在 store 里提示过
          }
          const res = await api.removeWallpaper(entry.id);
          if (!res.ok) {
            reportFailure(res, t("library.card.remove"));
            return;
          }
          entries = res.value;
          toast("ok", t("library.removed", [entry.name]));
          renderAll(true);
        });
      })();
    });

    return h(
      "div",
      { cls: `wp-card${isActive ? " is-active" : ""}` },
      top,
      badges,
      h(
        "div",
        { cls: "wp-card__actions" },
        applyBtn,
        previewBtn,
        revealBtn,
        renameBtn,
        removeBtn,
      ),
    );
  }

  /** 按钮忙碌态包装：禁用 → 执行 → 恢复；异常也不能冒泡 */
  async function withBusy(btn: HTMLButtonElement, fn: () => Promise<void>): Promise<void> {
    btn.disabled = true;
    try {
      await fn();
    } catch (err) {
      console.error("[wp] library action failed", err);
      toast("error", String(err));
    } finally {
      btn.disabled = false;
    }
  }

  function renderSummary(): void {
    const total = entries.reduce(
      (sum, e) => sum + (Number.isFinite(e.sizeBytes) ? e.sizeBytes : 0),
      0,
    );
    summary.textContent = t("library.count", [entries.length, fmtBytes(total)]);
  }

  function renderDir(): void {
    dirLine.textContent = t("library.dir", [getLibraryDir() || t("common.builtin")]);
  }

  /** 卡片内部文案（徽标、meta 里的相对时间）在语言切换后要整表重画 */
  function renderLang(): void {
    renderStatic();
    renderSummary();
    renderGrid(true);
  }

  function renderProgress(p: ImportProgress | null): void {
    if (!p) {
      progressBox.hidden = true;
      return;
    }
    progressBox.hidden = false;
    const labels: Record<ImportProgress["phase"], string> = {
      reading: t("library.progress.reading"),
      extracting: t("library.progress.extracting"),
      finalizing: t("library.progress.finalizing"),
      done: t("library.progress.done"),
    };
    const pct = Number.isFinite(p.percent) ? Math.min(100, Math.max(0, p.percent)) : 0;
    const phase = labels[p.phase] ?? p.phase;
    progressPhase.textContent = phase;
    const counts =
      p.totalFiles > 0 ? ` · ${t("library.progress.files", [p.files, p.totalFiles])}` : "";
    progressPct.textContent = `${pct.toFixed(0)}%${counts}`;
    progressCurrent.textContent = p.current || "";
    progressCurrent.hidden = p.current === "";
    progressFill.style.width = `${pct}%`;
  }

  function renderAll(force = false): void {
    renderStatic();
    renderDir();
    renderSummary();
    renderGrid(force);
  }

  /* ------------------------------------------------------------ 动作 */

  async function refresh(): Promise<void> {
    const res = await api.libraryList();
    if (!res.ok) {
      loaded = true;
      reportFailure(res, t("library.refresh"));
      return;
    }
    entries = res.value;
    loaded = true;
    renderAll(true);
  }

  async function doImport(zipPath: string): Promise<void> {
    importing = true;
    renderStatic();
    try {
      const res = await api.importZip(zipPath);
      if (!res.ok) {
        reportFailure(res, t("library.import"));
        return;
      }
      toast("ok", t("library.imported", [res.value.name]));
      await refresh();
    } finally {
      importing = false;
      setImportProgress(null);
      renderStatic();
    }
  }

  importBtn.addEventListener("click", () => {
    void withBusy(importBtn, async () => {
      const picked = await api.pickZipPath();
      if (!picked.ok) {
        reportFailure(picked, t("library.import"));
        return;
      }
      if (picked.value === null) return; // 用户取消
      await doImport(picked.value);
    });
  });

  refreshBtn.addEventListener("click", () => {
    void withBusy(refreshBtn, () => refresh());
  });

  dirBtn.addEventListener("click", () => {
    void withBusy(dirBtn, async () => {
      const picked = await api.pickLibraryDir();
      if (!picked.ok) {
        reportFailure(picked, t("library.pickDir"));
        return;
      }
      if (picked.value === null) {
        toast("info", t("library.pickDirCanceled"));
        return;
      }
      // 库目录本身是设置项：走设置管线（300ms 防抖保存 + 后端广播）
      patchSettings({ libraryDir: picked.value });
      toast("ok", t("library.pickDirDone", [picked.value]));
      await refresh();
    });
  });

  searchInput.addEventListener("input", () => {
    filter = searchInput.value;
    renderGrid(false);
  });

  /* ------------------------------------------------------------ 订阅 */

  const unsubs: Array<() => void> = [];
  unsubs.push(
    onLangChange(() => renderLang()),
    subscribeRuntime(() => renderGrid(false)),
    subscribeSettings(() => renderGrid(false)),
    subscribeLibraryDir(() => {
      renderDir();
      void refresh();
    }),
    subscribeImportProgress(() => renderProgress(getImportProgress())),
  );
  void safeListen<WallpaperEntry[]>(EV.library, (payload) => {
    if (!Array.isArray(payload)) return;
    entries = payload;
    loaded = true;
    renderAll(true);
  }).then((un) => unsubs.push(un));

  renderAll(true);
  // 切页回来时把正在跑的导入进度也画上
  renderProgress(getImportProgress());

  return {
    element: body,
    onShow(): void {
      renderAll(true);
      renderProgress(getImportProgress());
      if (!loaded) void refresh();
    },
    destroy(): void {
      for (const un of unsubs) un();
      unsubs.length = 0;
    },
  };
}
