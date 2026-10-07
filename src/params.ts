/**
 * src/params.ts
 * 自定义参数编辑器：把 `Settings.customParams` 渲染成可增删改的列表。
 *
 * 两条数据来源：
 * 1. **设置** —— 用户填的键 / 值 / 锁定（`customParams`，随配置包每秒下发给壁纸端）；
 * 2. **壁纸端上报** —— `param_report` 命令 + `wp://param-report` 事件给出的
 *    「这个参数有哪些可选值、当前实际生效的是哪个」。
 *
 * 为什么值控件不写死成输入框：`costume` 的可选服装只有壁纸端知道（它在运行时
 * 扫描 Resources/Clothes），所以控制端**不能**硬编码枚举。上报里带了 options
 * 就用下拉，没带就退回自由输入 —— 老版本壁纸端（不上报）也能正常用。
 */
import { api, EV, safeListen, type CustomParam, type ParamReport } from "./api";
import { getSettings, patchSettings, t } from "./state";
import { h, icon, modalConfirm, modalPrompt } from "./ui";

/** 预置参数键（与 Rust 侧 `prefs.rs` 的常量一致） */
export const PARAM_BEAT = "beat";
export const PARAM_COSTUME = "costume";

/** 哪些键算"预置项"（不可删除）。大小写不敏感比较，与 Rust 侧一致。 */
const PRESET_KEYS = [PARAM_BEAT, PARAM_COSTUME];

export function isPresetKey(key: string): boolean {
  return PRESET_KEYS.some((known) => known.toLowerCase() === key.toLowerCase());
}

/** 参数键合法字符：字母、数字、下划线（保留大小写，与 Rust 侧清洗规则一致） */
const KEY_PATTERN = /^[A-Za-z0-9_]+$/;

/** 键的展示名：预置键走 i18n，未知键原样显示 */
function keyLabel(key: string): string {
  const lower = key.toLowerCase();
  if (lower === PARAM_BEAT) return t("params.key.beat");
  if (lower === PARAM_COSTUME) return t("params.key.costume");
  return key;
}

/** 值的展示名：预置键的已知取值走 i18n */
function valueLabel(key: string, value: string): string {
  if (key.toLowerCase() === PARAM_COSTUME && value === "random") return t("params.value.random");
  return value;
}

export interface ParamsEditor {
  /** 列表 + 状态行（标题/描述由外层卡片负责） */
  element: HTMLElement;
  /** 「添加参数」按钮，交给外层卡片放在头部 */
  addButton: HTMLElement;
  /** 拉一次上报（打开页面 / 收到事件时调） */
  refresh(): void;
  /** 语言切换时刷新文案 */
  renderStatic(): void;
  destroy(): void;
}

export function createParamsEditor(): ParamsEditor {
  /** 壁纸端上报的能力，按 key 索引 */
  let reports = new Map<string, ParamReport>();
  let reportClient = "";
  let reportAgeMs: number | null = null;
  let listening = false;

  const listEl = h("div", { cls: "params" });
  const statusEl = h("div", { cls: "params__status muted" });
  const addBtn = h("button", { cls: "btn btn--sm", type: "button" });

  /* ------------------------------------------------------------ 上报订阅 */

  async function fetchReport(): Promise<void> {
    const res = await api.paramReport();
    if (!res.ok) return; // 后端未就绪 / 没跑壁纸：保持空，不打扰用户
    const snapshot = res.value;
    reportClient = snapshot.client;
    reportAgeMs = snapshot.ageMs;
    // 同时按原样与小写建索引：键名大小写不一致时也能命中
    reports = new Map();
    for (const p of snapshot.params) {
      reports.set(p.key, p);
      reports.set(p.key.toLowerCase(), p);
    }
    render();
  }

  async function listenReport(): Promise<void> {
    if (listening) return;
    listening = true;
    await safeListen(EV.paramReport, () => {
      void fetchReport();
    });
  }

  /* ------------------------------------------------------------ 改值 */

  function commit(next: CustomParam[]): void {
    patchSettings({ customParams: next });
  }

  /**
   * 改某一行的值。
   */
  function setValue(index: number, value: string): void {
    const next = getSettings().customParams.map((p) => ({ ...p }));
    const target = next[index];
    if (!target) return;

    target.value = value;
    commit(next);
  }

  function setLocked(index: number, locked: boolean): void {
    const next = getSettings().customParams.map((p) => ({ ...p }));
    const target = next[index];
    if (!target) return;
    // 预置的节拍参数不允许解锁：它的「锁定」是契约的一部分
    if (target.key.toLowerCase() === PARAM_BEAT) return;
    target.locked = locked;
    commit(next);
  }

  function removeAt(index: number): void {
    const params = getSettings().customParams;
    const target = params[index];
    // 预置项不给删（删了 Rust 侧也会补回来，界面会「删不掉」显得像 bug）
    if (target && isPresetKey(target.key)) return;
    commit(params.filter((_, i) => i !== index));
  }

  /* ------------------------------------------------------------ 行渲染 */

  function buildRow(param: CustomParam, index: number): HTMLElement {
    // 上报是壁纸端按自己的键名发的，大小写未必与控制端设置完全一致
    const report = reports.get(param.key) ?? reports.get(param.key.toLowerCase());
    const isPreset = isPresetKey(param.key);
    const isBeat = param.key.toLowerCase() === PARAM_BEAT;
    // 预置键的锁定状态由契约固定：beat 必锁
    const isLocked = isBeat ? true : param.locked;

    const keyEl = h("div", { cls: "params__key" }, h("span", { text: keyLabel(param.key) }));
    if (isPreset) {
      keyEl.append(h("span", { cls: "badge", text: t("params.preset") }));
    }

    /* ---- 值控件：有上报的可选项就下拉；否则自由文本 ---- */
    let valueControl: HTMLElement;
    const known = report?.options ?? [];
    if (known.length > 0) {
      const select = h("select", { cls: "select params__value" });
      // 上报的当前值可能不在选项里（例如壁纸端换了服装表）—— 补一项免得显示空白
      const values = known.map((o) => o.value);
      if (param.value !== "" && !values.includes(param.value)) {
        const extra = h("option", { text: valueLabel(param.key, param.value) });
        extra.value = param.value;
        select.append(extra);
      }
      for (const option of known) {
        const opt = h("option", { text: valueLabel(param.key, option.value) || option.label });
        opt.value = option.value;
        select.append(opt);
      }
      select.value = param.value;
      select.disabled = isLocked;
      select.addEventListener("change", () => setValue(index, select.value));
      valueControl = select;
    } else {
      const input = h("input", { cls: "input params__value", type: "text" });
      input.value = param.value;
      input.disabled = isLocked;
      input.placeholder = t("params.valuePlaceholder");
      // 输入框改的是「最终字符串」，逐字符落盘会吵；失焦 / 回车再提交
      input.addEventListener("change", () => setValue(index, input.value));
      valueControl = input;
    }

    /* ---- 锁定 ---- */
    const lockBox = h("input", { type: "checkbox" });
    lockBox.checked = isLocked;
    lockBox.disabled = isBeat; // 节拍锁定是强制的
    lockBox.title = isBeat ? t("params.beatLockHint") : t("params.lockHint");
    lockBox.addEventListener("change", () => setLocked(index, lockBox.checked));
    const lockWrap = h(
      "label",
      { cls: "params__lock", title: lockBox.title },
      lockBox,
      h("span", { text: t("params.locked") }),
    );

    /* ---- 删除 ---- */
    const removeBtn = h("button", { cls: "btn btn--ghost btn--icon", type: "button" });
    removeBtn.innerHTML = icon("trash", 14);
    removeBtn.title = isPreset ? t("params.presetHint") : t("params.remove");
    removeBtn.setAttribute("aria-label", t("params.remove"));
    removeBtn.disabled = isPreset;
    removeBtn.addEventListener("click", () => {
      void confirmRemove(index);
    });

    const cells: Node[] = [keyEl, valueControl];
    cells.push(lockWrap, removeBtn);
    return h("div", { cls: "params__row" }, ...cells);
  }

  /* ------------------------------------------------------------ 增删 */

  /** 新增一条参数：键名让用户填，校验通过才落盘 */
  async function addParam(): Promise<void> {
    const key = await modalPrompt({
      title: t("params.add"),
      message: t("params.addPrompt"),
      value: "",
      placeholder: PARAM_COSTUME,
      confirmText: t("params.add"),
      cancelText: t("common.cancel"),
      // 校验失败时模态不关，把原因写在输入框下面 —— 比 alert 顺手
      validate: (raw) => {
        const cleaned = raw.trim().replace(/[^A-Za-z0-9_]/g, "");
        if (cleaned === "" || !KEY_PATTERN.test(cleaned)) return t("params.badKey");
        // 去重按大小写不敏感，与 Rust 侧一致
        if (
          getSettings().customParams.some(
            (p) => p.key.toLowerCase() === cleaned.toLowerCase(),
          )
        ) {
          return t("params.duplicateKey");
        }
        return null;
      },
    });
    if (key === null) return;
    // 保留用户输入的大小写（camelCase 键名有意义），只剔除非法字符
    const cleaned = key.trim().replace(/[^A-Za-z0-9_]/g, "");
    commit([...getSettings().customParams, { key: cleaned, value: "", locked: false }]);
  }

  /** 删一条参数（预置项不可删，到不了这里） */
  async function confirmRemove(index: number): Promise<void> {
    const target = getSettings().customParams[index];
    if (!target) return;
    const yes = await modalConfirm({
      title: t("params.remove"),
      message: t("params.removeConfirm", [target.key]),
      confirmText: t("params.remove"),
      cancelText: t("common.cancel"),
      danger: true,
    });
    if (yes) removeAt(index);
  }

  /* ------------------------------------------------------------ 渲染 */

  function renderStatus(): void {
    if (reportAgeMs === null) {
      statusEl.textContent = t("params.noReport");
      statusEl.classList.remove("is-stale");
      return;
    }
    // 5 秒内算新鲜：壁纸端每秒都会随配置包回一发
    const stale = reportAgeMs > 5000;
    const seconds = (reportAgeMs / 1000).toFixed(1);
    statusEl.textContent = reportClient
      ? t("params.reportAt", [reportClient, seconds])
      : t("params.reportAtNoClient", [seconds]);
    statusEl.classList.toggle("is-stale", stale);
    if (stale) statusEl.textContent += ` · ${t("params.staleHint")}`;
  }

  function render(): void {
    const params = getSettings().customParams;
    const rows = params.map((param, index) => buildRow(param, index));
    listEl.replaceChildren(...rows);
    if (rows.length === 0) {
      listEl.append(h("div", { cls: "muted", text: t("params.empty") }));
    }
    renderStatus();
  }

  /* ------------------------------------------------------------ 组装 */

  const root = h("div", { cls: "params-editor" }, listEl, statusEl);

  addBtn.addEventListener("click", () => {
    void addParam();
  });

  function renderStatic(): void {
    addBtn.textContent = t("params.add");
    render();
  }

  renderStatic();
  void fetchReport();
  void listenReport();

  return {
    element: root,
    addButton: addBtn,
    refresh(): void {
      void fetchReport();
    },
    renderStatic,
    destroy(): void {
      // safeListen 返回的 unlisten 没留存（只订阅一次、随页面存活），这里只清引用
      reports.clear();
    },
  };
}
