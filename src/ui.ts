/**
 * src/ui.ts
 * 极简 DOM 助手 + 内联 SVG 图标 + 格式化工具。
 * 不引任何框架/依赖，图标全部内联，保证离线可用。
 */

export type Child = Node | string | null | undefined | false;

export interface ElOptions {
  cls?: string;
  text?: string;
  html?: string;
  id?: string;
  title?: string;
  type?: string;
  value?: string;
  placeholder?: string;
  style?: string;
  attrs?: Record<string, string>;
  on?: Partial<Record<keyof HTMLElementEventMap, (ev: Event) => void>>;
}

/** 建元素：h("div", { cls: "card" }, child, child) */
export function h<K extends keyof HTMLElementTagNameMap>(
  tag: K,
  opts: ElOptions = {},
  ...children: Child[]
): HTMLElementTagNameMap[K] {
  const el = document.createElement(tag);
  if (opts.cls) el.className = opts.cls;
  if (opts.id) el.id = opts.id;
  if (opts.title) el.title = opts.title;
  if (opts.text !== undefined) el.textContent = opts.text;
  if (opts.html !== undefined) el.innerHTML = opts.html;
  if (opts.type !== undefined) el.setAttribute("type", opts.type);
  if (opts.value !== undefined) el.setAttribute("value", opts.value);
  if (opts.placeholder !== undefined) el.setAttribute("placeholder", opts.placeholder);
  if (opts.style !== undefined) el.setAttribute("style", opts.style);
  if (opts.attrs) {
    for (const [k, v] of Object.entries(opts.attrs)) el.setAttribute(k, v);
  }
  if (opts.on) {
    for (const [name, fn] of Object.entries(opts.on)) {
      if (fn) el.addEventListener(name, fn as EventListener);
    }
  }
  for (const child of children) {
    if (child === null || child === undefined || child === false) continue;
    el.append(typeof child === "string" ? document.createTextNode(child) : child);
  }
  return el;
}

/** 清空一个容器 */
export function clear(el: HTMLElement): void {
  while (el.firstChild) el.removeChild(el.firstChild);
}

/* ---------------------------------------------------------------- 图标 */

const ICON_PATHS: Record<string, string> = {
  library:
    "M4 5.5A1.5 1.5 0 0 1 5.5 4H9v16H5.5A1.5 1.5 0 0 1 4 18.5v-13Z M10.5 4h3.5v16h-3.5z M16.2 4.4l3.4.9-4.1 14.4-3.4-.9z",
  audio: "M9 18V6l10-2v12M9 18a3 3 0 1 1-6 0 3 3 0 0 1 6 0Zm10-2a3 3 0 1 1-6 0 3 3 0 0 1 6 0Z",
  settings:
    "M12 15.5a3.5 3.5 0 1 0 0-7 3.5 3.5 0 0 0 0 7Z M19.4 15a1.7 1.7 0 0 0 .34 1.87l.06.06a2 2 0 1 1-2.83 2.83l-.06-.06a1.7 1.7 0 0 0-2.87 1.2V21a2 2 0 1 1-4 0v-.09A1.7 1.7 0 0 0 7.1 19.7l-.06.06a2 2 0 1 1-2.83-2.83l.06-.06A1.7 1.7 0 0 0 3 14.1H3a2 2 0 1 1 0-4h.09A1.7 1.7 0 0 0 4.3 7.1l-.06-.06A2 2 0 1 1 7.07 4.2l.06.06A1.7 1.7 0 0 0 9 4.6h.1A1.7 1.7 0 0 0 10.8 3V3a2 2 0 1 1 4 0v.09a1.7 1.7 0 0 0 2.87 1.2l.06-.06a2 2 0 1 1 2.83 2.83l-.06.06a1.7 1.7 0 0 0 1.2 2.87H21a2 2 0 1 1 0 4h-.09a1.7 1.7 0 0 0-1.57 1.01Z",
  info: "M12 21a9 9 0 1 0 0-18 9 9 0 0 0 0 18Zm0-13.2v.2m0 3.2v6",
  play: "M7 4.5 19.5 12 7 19.5Z",
  stop: "M6 6h12v12H6z",
  reload: "M20 11a8 8 0 1 0-2.3 5.7M20 5v6h-6",
  pause: "M8 5h3v14H8zM13 5h3v14h-3z",
  mute: "M11 5 6.5 9H3v6h3.5L11 19V5Z M17 9l4 6m0-6-4 6",
  volume: "M11 5 6.5 9H3v6h3.5L11 19V5Z M15.5 8.5a5 5 0 0 1 0 7M18 6a8.5 8.5 0 0 1 0 12",
  folderOpen:
    "M3 7.5A1.5 1.5 0 0 1 4.5 6h4.2l2 2.4h8.8a1.5 1.5 0 0 1 1.5 1.5v1H7.4L4 19.5h-.5A1.5 1.5 0 0 1 2 18Z M2.4 19.5 6 12h16l-3.7 7.4a1.5 1.5 0 0 1-1.3.8H3.7a1.5 1.5 0 0 1-1.3-1.4",
  trash: "M4 7h16M9 7V4.5h6V7m-8 0 .9 13.2A1.5 1.5 0 0 0 9.4 21.5h5.2a1.5 1.5 0 0 0 1.5-1.3L17 7M10 11v6M14 11v6",
  pencil: "M4 20h4L20 8l-4-4L4 16ZM14.5 5.5l4 4",
  desktop:
    "M3 5.5A1.5 1.5 0 0 1 4.5 4h15A1.5 1.5 0 0 1 21 5.5v9A1.5 1.5 0 0 1 19.5 16h-15A1.5 1.5 0 0 1 3 14.5ZM9 20h6M12 16v4",
  check: "M5 12.5 10 17.5 19.5 6.5",
  download: "M12 3.5v11m-5-5 5 5 5-5M4.5 19.5h15",
  wave: "M3 12h2.5l2-6 3 15 3-12 2 5h5.5",
  chip: "M8 8h8v8H8z M5.5 5.5h13v13h-13z M9 2.5v3m6-3v3M9 18.5v3m6-3v3M2.5 9h3m-3 6h3M18.5 9h3m-3 6h3",
  bolt: "M13.5 2.5 5 13.5h6l-.5 8L19 10.5h-6Z",
  search: "M10.5 17a6.5 6.5 0 1 0 0-13 6.5 6.5 0 0 0 0 13Zm4.8-1.7L20.5 20.5",
  file: "M6 3h7.5L18 7.5V21H6Z M13.5 3v4.5H18",
  monitor:
    "M3 5.5A1.5 1.5 0 0 1 4.5 4h15A1.5 1.5 0 0 1 21 5.5v9A1.5 1.5 0 0 1 19.5 16h-15A1.5 1.5 0 0 1 3 14.5ZM9 20h6M12 16v4",
  keyboard:
    "M3 7.5h18v9H3z M6 10.5h1m2.5 0h1m2.5 0h1m2.5 0h1M6 13.5h9m2.5 0h1",
  bell: "M12 3.5a5.5 5.5 0 0 0-5.5 5.5v4L5 16h14l-1.5-3V9A5.5 5.5 0 0 0 12 3.5ZM10 19a2 2 0 0 0 4 0",
  close: "M6 6l12 12M18 6 6 18",
};

/**
 * 内联 SVG 图标。图标名不可用时返回空串（不抛错）。
 * `@param name` 见 ICON_PATHS 的键
 */
export function icon(name: string, size = 16, cls = "ic"): string {
  const d = ICON_PATHS[name];
  if (!d) return "";
  return (
    `<svg class="${cls}" width="${size}" height="${size}" viewBox="0 0 24 24" fill="none" ` +
    `stroke="currentColor" stroke-width="1.7" stroke-linecap="round" stroke-linejoin="round" ` +
    `aria-hidden="true"><path d="${d}"/></svg>`
  );
}

/* ---------------------------------------------------------------- 格式化 */

/** 1.4 GB / 863 MB */
export function fmtBytes(bytes: number): string {
  if (!Number.isFinite(bytes) || bytes <= 0) return "0 B";
  const units = ["B", "KB", "MB", "GB", "TB"];
  let value = bytes;
  let i = 0;
  while (value >= 1024 && i < units.length - 1) {
    value /= 1024;
    i += 1;
  }
  const digits = i === 0 ? 0 : value < 10 ? 2 : value < 100 ? 1 : 0;
  return `${value.toFixed(digits)} ${units[i]}`;
}

/** ISO8601 → 本地「2024-05-01 19:20」 */
export function fmtDateTime(iso: string, langTag: string): string {
  if (!iso) return "—";
  const ms = Date.parse(iso);
  if (!Number.isFinite(ms)) return iso;
  const d = new Date(ms);
  const pad = (n: number): string => String(n).padStart(2, "0");
  const date = `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())}`;
  const time = `${pad(d.getHours())}:${pad(d.getMinutes())}`;
  return langTag === "en-US" ? `${date} ${time}` : `${date} ${time}`;
}

/** 相对时间：刚刚 / 3 分钟前 / 2 天前 */
export function fmtRelative(iso: string, langTag: string): string {
  const ms = Date.parse(iso);
  if (!Number.isFinite(ms)) return "—";
  const diff = Date.now() - ms;
  if (diff < 0) return fmtDateTime(iso, langTag);
  const min = Math.floor(diff / 60000);
  if (min < 1) return langTag === "en-US" ? "just now" : "刚刚";
  if (min < 60) return langTag === "en-US" ? `${min} min ago` : `${min} 分钟前`;
  const hours = Math.floor(min / 60);
  if (hours < 24) return langTag === "en-US" ? `${hours} h ago` : `${hours} 小时前`;
  const days = Math.floor(hours / 24);
  if (days < 30) return langTag === "en-US" ? `${days} d ago` : `${days} 天前`;
  return fmtDateTime(iso, langTag);
}

/** 时间戳（ms）→ HH:MM:SS */
export function fmtClock(ms: number): string {
  if (!Number.isFinite(ms) || ms <= 0) return "--:--:--";
  const d = new Date(ms);
  const pad = (n: number): string => String(n).padStart(2, "0");
  return `${pad(d.getHours())}:${pad(d.getMinutes())}:${pad(d.getSeconds())}`;
}

/** 秒 → 1:23:45 / 12:30 */
export function fmtDuration(seconds: number): string {
  if (!Number.isFinite(seconds) || seconds <= 0) return "—";
  const total = Math.floor(seconds);
  const s = total % 60;
  const m = Math.floor(total / 60) % 60;
  const hr = Math.floor(total / 3600);
  const pad = (n: number): string => String(n).padStart(2, "0");
  return hr > 0 ? `${hr}:${pad(m)}:${pad(s)}` : `${m}:${pad(s)}`;
}

/** 数字输入的安全解析 */
export function num(value: string, fallback: number): number {
  const v = Number.parseFloat(value);
  return Number.isFinite(v) ? v : fallback;
}

/** 夹取 */
export function clamp(value: number, min: number, max: number): number {
  return Math.min(max, Math.max(min, value));
}

/** 带样式的文本输入 */
export function textInput(
  value: string,
  onChange: (value: string) => void,
  opts: { placeholder?: string; width?: number; mono?: boolean } = {},
): HTMLInputElement {
  const el = h("input", {
    cls: `input${opts.mono ? " mono" : ""}`,
    type: "text",
    placeholder: opts.placeholder,
  });
  el.value = value;
  if (opts.width) el.style.width = `${opts.width}px`;
  el.addEventListener("input", () => onChange(el.value));
  return el;
}

/** 数字输入（含最小值/最大值/步长） */
export function numberInput(
  value: number,
  onChange: (value: number) => void,
  opts: { min?: number; max?: number; step?: number; width?: number } = {},
): HTMLInputElement {
  const el = h("input", { cls: "input input--num", type: "number" });
  el.value = String(value);
  if (opts.min !== undefined) el.min = String(opts.min);
  if (opts.max !== undefined) el.max = String(opts.max);
  el.step = String(opts.step ?? 1);
  if (opts.width) el.style.width = `${opts.width}px`;
  el.addEventListener("input", () => {
    const parsed = Number.parseFloat(el.value);
    if (Number.isFinite(parsed)) onChange(parsed);
  });
  return el;
}

/** 布尔开关 */
export function toggle(
  checked: boolean,
  onChange: (checked: boolean) => void,
  disabled = false,
): HTMLElement {
  const input = h("input", { type: "checkbox" });
  input.checked = checked;
  input.disabled = disabled;
  input.addEventListener("change", () => onChange(input.checked));
  return h("label", { cls: "switch" }, input, h("span", { cls: "switch__track" }));
}

/** 分段控件（单选按钮组） */
export interface SegmentedItem<T extends string> {
  value: T;
  label: string;
}

export function segmented<T extends string>(
  items: Array<SegmentedItem<T>>,
  value: T,
  onChange: (value: T) => void,
): { element: HTMLElement; setValue(next: T): void; setLabels(labels: string[]): void } {
  const buttons: HTMLButtonElement[] = [];
  const wrap = h("div", { cls: "seg" });
  items.forEach((item, index) => {
    const btn = h("button", { cls: "seg__btn", type: "button", text: item.label });
    btn.addEventListener("click", () => {
      setValue(item.value);
      onChange(item.value);
    });
    buttons.push(btn);
    wrap.append(btn);
    if (index === items.length - 1) return;
  });

  function setValue(next: T): void {
    items.forEach((item, index) => {
      buttons[index]?.classList.toggle("is-active", item.value === next);
    });
  }
  setValue(value);

  return {
    element: wrap,
    setValue,
    setLabels(labels: string[]): void {
      labels.forEach((label, index) => {
        const btn = buttons[index];
        if (btn) btn.textContent = label;
      });
    },
  };
}

/** 设置项一行：左边标签 + 说明，右边控件 */
export function settingRow(
  label: string,
  hint: string,
  control: HTMLElement | HTMLElement[],
): { row: HTMLElement; setText(label: string, hint: string): void } {
  const labelEl = h("div", { cls: "setting__label", text: label });
  const hintEl = h("div", { cls: "setting__hint", text: hint });
  const controlEl = h("div", { cls: "setting__control" });
  const list = Array.isArray(control) ? control : [control];
  for (const c of list) controlEl.append(c);
  const row = h("div", { cls: "setting" }, h("div", {}, labelEl, hintEl), controlEl);
  return {
    row,
    setText(nextLabel: string, nextHint: string): void {
      labelEl.textContent = nextLabel;
      hintEl.textContent = nextHint;
      hintEl.hidden = nextHint === "";
    },
  };
}

/** 分组卡片 */
export function card(
  title: string,
  desc: string,
  body: HTMLElement,
  extra?: HTMLElement,
): { element: HTMLElement; setText(title: string, desc: string): void } {
  const titleEl = h("h2", { cls: "card__title", text: title });
  const descEl = h("p", { cls: "card__desc", text: desc });
  descEl.hidden = desc === "";
  const head = h(
    "div",
    { cls: "card__head" },
    h("div", {}, titleEl, descEl),
    extra ?? h("div", {}),
  );
  const element = h("div", { cls: "card" }, head, h("div", { cls: "card__body" }, body));
  return {
    element,
    setText(nextTitle: string, nextDesc: string): void {
      titleEl.textContent = nextTitle;
      descEl.textContent = nextDesc;
      descEl.hidden = nextDesc === "";
    },
  };
}

/* ======================================================================== *
 * 页内模态对话框
 *
 * WebView2 里原生脚本对话框不能依赖：wry 让 WebView2 走默认脚本对话框，而
 * AreDefaultScriptDialogsEnabled 关闭时 prompt 直接返回 null、confirm 返回 false，
 * 用户点了等于什么都没发生。所以确认类交互一律自己画 DOM。
 * ======================================================================== */

/** 模态层容器 id */
const MODAL_LAYER_ID = "modal-layer";

export interface ConfirmOptions {
  title: string;
  message?: string;
  confirmText: string;
  cancelText: string;
  /** 危险操作：确认按钮用警示色 */
  danger?: boolean;
}

export interface PromptOptions {
  title: string;
  message?: string;
  /** 输入框初始值 */
  value: string;
  placeholder?: string;
  confirmText: string;
  cancelText: string;
  /** 返回非空字符串 = 校验失败，显示该文案并阻止关闭 */
  validate?: (value: string) => string | null;
}

/**
 * 所有模态共用一层容器（`#modal-layer`），遮罩按打开顺序叠放。
 * 这样嵌套模态（设置弹窗里再弹确认框）天然是「后开的在上面」，
 * 也不需要改动 `#app` 的 inert —— 早先用 inert 时，内层模态关闭会把 inert
 * 恢复成 false，导致还开着的设置弹窗不再挡点击。
 */
function ensureModalLayer(): HTMLElement {
  const existing = document.getElementById(MODAL_LAYER_ID);
  if (existing) return existing;
  const layer = h("div", { cls: "modal-layer", id: MODAL_LAYER_ID });
  document.body.append(layer);
  return layer;
}

/** 按打开顺序排列的模态栈，栈顶 = 最上层；`refocus` 用于焦点归还 */
const modalStack: Array<{ close: () => void; refocus: () => void }> = [];

/** 当前是否有任何模态开着（主界面据此决定要不要继续轮询缩略图） */
export function modalsOpen(): boolean {
  return modalStack.length > 0;
}

interface ModalOptions {
  title: string;
  message?: string;
  confirmText: string;
  cancelText: string;
  danger?: boolean;
}

interface ModalParts {
  mask: HTMLElement;
  dialog: HTMLElement;
  body: HTMLElement;
  error: HTMLElement;
  confirmBtn: HTMLButtonElement;
  cancelBtn: HTMLButtonElement;
}

function buildModal(opts: ModalOptions, widget: HTMLElement | null): ModalParts {
  const titleEl = h("h2", { cls: "modal__title", id: "modal-title", text: opts.title });
  const body = h("div", { cls: "modal__body" });
  if (opts.message) body.append(h("p", { cls: "modal__message", text: opts.message }));
  if (widget) body.append(widget);

  const error = h("p", { cls: "modal__error" });
  error.hidden = true;

  const cancelBtn = h("button", { cls: "btn", type: "button", text: opts.cancelText });
  const confirmBtn = h("button", {
    cls: `btn ${opts.danger ? "btn--danger-solid" : "btn--primary"}`,
    type: "button",
    text: opts.confirmText,
  });

  const dialog = h(
    "div",
    {
      cls: `modal${opts.danger ? " modal--danger" : ""}`,
      attrs: { role: "dialog", "aria-modal": "true", "aria-labelledby": "modal-title" },
    },
    h("div", { cls: "modal__head" }, titleEl),
    body,
    error,
    h("div", { cls: "modal__foot" }, cancelBtn, confirmBtn),
  );

  const mask = h("div", { cls: "modal-mask" }, dialog);
  return { mask, dialog, body, error, confirmBtn, cancelBtn };
}

/**
 * 把模态挂进 DOM 并接管键盘 / 焦点。
 *
 * **必须返回两个函数**：`close` 负责真正收起遮罩，`focus` 只负责首次聚焦。
 * 早先这里只返回了聚焦函数，调用方却把它当 `close` 用 —— 结果点「确定 / 取消」后
 * Promise 正常 resolve（动作确实执行了），但遮罩留在页面上把整个窗口的点击吃掉，
 * 用户看到的就是「按钮点不了」。
 *
 * 设置弹窗（settings-dialog.ts）复用的就是这一套，不要再写第二份关闭逻辑。
 */
export function presentModal(
  mask: HTMLElement,
  dialog: HTMLElement,
  onKeydown: (ev: KeyboardEvent) => void,
): { close: () => void; focus: (focusTarget?: HTMLElement) => void } {
  ensureModalLayer().append(mask);

  const focusables = (): HTMLElement[] =>
    Array.from(
      dialog.querySelectorAll<HTMLElement>(
        'button:not([disabled]), input:not([disabled]), textarea:not([disabled]), select:not([disabled]), [tabindex]:not([tabindex="-1"])',
      ),
    ).filter((el) => el.offsetParent !== null);

  let closed = false;

  /** 只有栈顶模态处理键盘与焦点，避免嵌套时 Esc 一次关两层 */
  const isTop = (): boolean => modalStack[modalStack.length - 1]?.close === close;

  const focus = (focusTarget?: HTMLElement): void => {
    window.requestAnimationFrame(() => {
      const target = focusTarget ?? focusables()[0] ?? dialog;
      target.focus();
      if (target instanceof HTMLInputElement) target.select();
    });
  };

  const onFocusIn = (ev: FocusEvent): void => {
    if (closed || !isTop()) return;
    const target = ev.target as Node | null;
    if (target && dialog.contains(target)) return;
    focusables()[0]?.focus();
  };

  const onKeydownCapture = (ev: KeyboardEvent): void => {
    if (!isTop()) return;
    onKeydown(ev);
  };

  const close = (): void => {
    if (closed) return;
    closed = true;
    document.removeEventListener("keydown", onKeydownCapture, true);
    document.removeEventListener("focusin", onFocusIn, true);
    const index = modalStack.findIndex((entry) => entry.close === close);
    if (index >= 0) modalStack.splice(index, 1);
    mask.remove();
    // 关掉内层后把焦点还给新的栈顶（外层模态），否则焦点会掉到 body 上
    const top = modalStack[modalStack.length - 1];
    if (top) window.requestAnimationFrame(() => top.refocus());
  };

  modalStack.push({ close, refocus: () => focus() });
  document.addEventListener("keydown", onKeydownCapture, true);
  document.addEventListener("focusin", onFocusIn, true);

  return { close, focus };
}

/** 确认框：取消 → false，确认 → true。Enter 确认 / Esc 取消 / 点遮罩取消 */
export function modalConfirm(opts: ConfirmOptions): Promise<boolean> {
  return new Promise<boolean>((resolve) => {
    const parts = buildModal(opts, null);
    let settled = false;

    const finish = (result: boolean): void => {
      if (settled) return;
      settled = true;
      close();
      resolve(result);
    };

    const onKeydown = (ev: KeyboardEvent): void => {
      if (ev.key === "Escape") {
        ev.preventDefault();
        ev.stopPropagation();
        finish(false);
        return;
      }
      if (ev.key === "Enter") {
        // 焦点在按钮上时交给按钮自己的 click，避免触发两次
        if (ev.target instanceof HTMLButtonElement || ev.target instanceof HTMLTextAreaElement) {
          return;
        }
        ev.preventDefault();
        finish(true);
      }
    };

    const { close, focus } = presentModal(parts.mask, parts.dialog, onKeydown);
    parts.mask.addEventListener("mousedown", (ev) => {
      if (ev.target === parts.mask) finish(false);
    });
    parts.cancelBtn.addEventListener("click", () => finish(false));
    parts.confirmBtn.addEventListener("click", () => finish(true));
    focus(parts.confirmBtn);
  });
}

/** 输入框：取消 → null；确认 → trim 后的字符串（validate 返回非空则不关闭） */
export function modalPrompt(opts: PromptOptions): Promise<string | null> {
  return new Promise<string | null>((resolve) => {
    const input = h("input", {
      cls: "input modal__input",
      type: "text",
      placeholder: opts.placeholder,
    });
    input.value = opts.value;

    const parts = buildModal(opts, input);
    let settled = false;

    const finish = (result: string | null): void => {
      if (settled) return;
      settled = true;
      close();
      resolve(result);
    };

    const showError = (message: string | null): boolean => {
      if (message === null) {
        parts.error.hidden = true;
        parts.error.textContent = "";
        return true;
      }
      parts.error.textContent = message;
      parts.error.hidden = false;
      input.focus();
      input.select();
      return false;
    };

    const submit = (): void => {
      const value = input.value.trim();
      const problem = opts.validate ? opts.validate(value) : null;
      if (!showError(problem)) return;
      finish(value);
    };

    const onKeydown = (ev: KeyboardEvent): void => {
      if (ev.key === "Escape") {
        ev.preventDefault();
        ev.stopPropagation();
        finish(null);
        return;
      }
      if (ev.key === "Enter" && !(ev.target instanceof HTMLTextAreaElement)) {
        ev.preventDefault();
        submit();
      }
    };

    // 重新输入时清掉上一次的错误
    input.addEventListener("input", () => {
      parts.error.hidden = true;
      parts.error.textContent = "";
    });

    const { close, focus } = presentModal(parts.mask, parts.dialog, onKeydown);
    parts.mask.addEventListener("mousedown", (ev) => {
      if (ev.target === parts.mask) finish(null);
    });
    parts.cancelBtn.addEventListener("click", () => finish(null));
    parts.confirmBtn.addEventListener("click", submit);
    focus(input);
  });
}

