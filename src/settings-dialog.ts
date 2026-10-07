/**
 * src/settings-dialog.ts
 * 大号设置弹窗：壁纸库 / 音频 / 通用 / 关于 四个 tab。
 *
 * 遮罩、焦点陷阱、inert、Esc / 点遮罩关闭全部复用 ui.ts 里那套已经验证过的
 * `presentModal`（返回 { close, focus }），这里**不再写第二份关闭逻辑**。
 */
import { onLangChange, t } from "./state";
import { h, icon, presentModal } from "./ui";
import type { Tab, TabId } from "./tabs";
import { createLibraryTab } from "./library";
import { createAudioTab } from "./audio";
import { createGeneralTab } from "./settings-panel";
import { createAboutTab } from "./about";

export interface SettingsDialog {
  /** 打开弹窗；可指定初始 tab（例如空状态点「导入」→ library） */
  open(tab?: TabId): void;
  close(): void;
  isOpen(): boolean;
  /** 关闭状态变化（主界面据此暂停缩略图轮询之类的动作） */
  onOpenChange(fn: (open: boolean) => void): () => void;
}

const TAB_ORDER: TabId[] = ["library", "audio", "general", "about"];

export function createSettingsDialog(): SettingsDialog {
  /* ---- 四个 tab 只建一次，切换时只换 DOM，不重建 ---- */
  const tabs: Record<TabId, Tab> = {
    library: createLibraryTab(),
    audio: createAudioTab(),
    general: createGeneralTab(),
    about: createAboutTab(),
  };

  const tabLabel = (id: TabId): string => {
    switch (id) {
      case "library":
        return t("dialog.tabLibrary");
      case "audio":
        return t("dialog.tabAudio");
      case "general":
        return t("dialog.tabGeneral");
      case "about":
        return t("dialog.tabAbout");
    }
  };

  const tabBar = h("div", { cls: "dialog__tabs", attrs: { role: "tablist" } });
  const tabButtons = new Map<TabId, HTMLButtonElement>();
  for (const id of TAB_ORDER) {
    const btn = h("button", { cls: "dialog__tab", type: "button", attrs: { role: "tab" } });
    btn.addEventListener("click", () => selectTab(id));
    tabButtons.set(id, btn);
    tabBar.append(btn);
  }

  const titleEl = h("h2", { cls: "dialog__title", id: "settings-dialog-title" });
  const hintEl = h("p", { cls: "dialog__hint" });
  const bodyHost = h("div", { cls: "dialog__body" });

  const closeBtn = h("button", {
    cls: "btn btn--ghost btn--icon dialog__close",
    type: "button",
  });

  const panel = h(
    "div",
    {
      cls: "dialog",
      attrs: {
        role: "dialog",
        "aria-modal": "true",
        "aria-labelledby": "settings-dialog-title",
      },
    },
    h(
      "div",
      { cls: "dialog__head" },
      h("div", { cls: "dialog__heading" }, titleEl, hintEl),
      closeBtn,
    ),
    tabBar,
    bodyHost,
  );

  const mask = h("div", { cls: "modal-mask dialog-mask", attrs: { id: "settings-dialog-mask" } }, panel);
  mask.hidden = true;

  let currentTab: TabId = "library";
  let mounted: Tab | null = null;
  let openState = false;
  let present: { close: () => void; focus: (target?: HTMLElement) => void } | null = null;
  let firstOpen = true;
  const openListeners = new Set<(open: boolean) => void>();

  function setOpenState(next: boolean): void {
    if (openState === next) return;
    openState = next;
    for (const fn of openListeners) {
      try {
        fn(next);
      } catch (err) {
        console.error("[wp] settings dialog listener failed", err);
      }
    }
  }

  function renderTabBar(): void {
    titleEl.textContent = t("dialog.settingsTitle");
    hintEl.textContent = t("dialog.hint");
    for (const id of TAB_ORDER) {
      const btn = tabButtons.get(id);
      if (!btn) continue;
      btn.textContent = tabLabel(id);
      btn.classList.toggle("is-active", id === currentTab);
      btn.setAttribute("aria-selected", id === currentTab ? "true" : "false");
    }
    closeBtn.innerHTML = `${icon("close", 16)}<span class="sr-only">${t("action.close")}</span>`;
    closeBtn.title = t("action.close");
  }

  function mountTab(id: TabId): void {
    const next = tabs[id];
    if (mounted === next) {
      next.onShow?.();
      return;
    }
    mounted?.onHide?.();
    mounted = next;
    bodyHost.replaceChildren(next.element);
    next.onShow?.();
  }

  function selectTab(id: TabId): void {
    currentTab = id;
    renderTabBar();
    mountTab(id);
    bodyHost.scrollTop = 0;
  }

  function doClose(): void {
    if (!openState) return;
    setOpenState(false);
    mounted?.onHide?.();
    mounted = null;
    present?.close();
    present = null;
    mask.hidden = true;
  }

  function doOpen(tab?: TabId): void {
    if (openState) {
      // 已经开着就只切 tab
      if (tab) selectTab(tab);
      return;
    }
    mask.hidden = false;
    if (tab) currentTab = tab;
    renderTabBar();
    mountTab(currentTab);

    if (!present) {
      const created = presentModal(mask, panel, (ev) => {
        if (ev.key === "Escape") {
          ev.preventDefault();
          ev.stopPropagation();
          doClose();
        }
      });
      present = created;
    }
    setOpenState(true);
    // 首次打开把焦点放到 tab 栏，之后记住位置由焦点陷阱兜住
    if (firstOpen) {
      firstOpen = false;
      present.focus(tabButtons.get(currentTab) ?? undefined);
    }
  }

  /* ---- 关闭交互 ---- */
  closeBtn.addEventListener("click", () => doClose());
  mask.addEventListener("mousedown", (ev) => {
    if (ev.target === mask) doClose();
  });

  /* ---- 语言切换：tab 名与标题一起刷（各 tab 内部自己订阅了文案刷新） ---- */
  onLangChange(() => renderTabBar());

  renderTabBar();

  return {
    open(tab?: TabId): void {
      doOpen(tab);
    },
    close(): void {
      doClose();
    },
    isOpen(): boolean {
      return openState;
    },
    onOpenChange(fn: (open: boolean) => void): () => void {
      openListeners.add(fn);
      return () => openListeners.delete(fn);
    },
  };
}
