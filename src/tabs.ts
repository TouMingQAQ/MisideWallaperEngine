/**
 * src/tabs.ts
 * 设置弹窗里 tab 内容的统一契约。
 * 每个 tab 只建一次 DOM，由设置弹窗负责挂载 / 卸载与生命周期回调。
 */

export interface Tab {
  /** tab 的根元素 */
  readonly element: HTMLElement;
  /** 每次被切到这个 tab 时调用（拉数据、刷新文案） */
  onShow?(): void;
  /** 从 tab 上切走时调用（停轮询、解订阅） */
  onHide?(): void;
  /** 彻底释放（关闭弹窗） */
  destroy?(): void;
}

/** 设置弹窗的四个 tab */
export type TabId = "library" | "audio" | "general" | "about";
