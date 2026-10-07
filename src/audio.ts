/**
 * src/audio.ts
 * 设置弹窗「音频」tab：采集总开关、传输方式（本地 UDP）、UDP 地址与端口、
 * 增益 / 推流帧率、采集状态卡、实时可视化。
 *
 * 「音频源」的选择已经搬到主界面仪表盘（那里有带峰值条的进程列表），
 * 这里只显示当前源并给一句指路文案，避免同一件事有两个入口。
 */
import {
  api,
  DEFAULT_SETTINGS,
  EV,
  safeListen,
  type AudioFramePayload,
  type AudioStatus,
} from "./api";
import {
  getAudioStatus,
  getSettings,
  onLangChange,
  patchSettings,
  reportFailure,
  store,
  subscribeAudioStatus,
  subscribeSettings,
  t,
} from "./state";
import { card, h, numberInput, num, settingRow, textInput, toggle } from "./ui";
import { createVisualizer } from "./visualizer";
import type { Tab } from "./tabs";

export function createAudioTab(): Tab {
  /* ------------------------------------------------------------ 总开关 */

  const enabledToggle = toggle(getSettings().audioEnabled, (checked) => {
    void store.setAudioEnabled(checked);
  });
  const enabledRow = settingRow(t("audio.enabled"), t("audio.enabledHint"), enabledToggle);

  /* ------------------------------------------------------------ 音频源（只读） */

  const sourceHint = h("div", { cls: "mono" });
  const sourceHintRow = settingRow(t("audio.source"), t("audio.sourceDashboardHint"), [
    sourceHint,
  ]);

  /* ------------------------------------------------------------ 传输（固定本地 UDP） */

  const udpHostInput = textInput(getSettings().udpHost, (value) =>
    patchSettings({ udpHost: value.trim() }),
  );
  udpHostInput.style.width = "150px";

  const udpPortInput = numberInput(
    getSettings().udpPort,
    (value) => patchSettings({ udpPort: Math.round(value) }),
    { min: 1, max: 65535, width: 110 },
  );

  const gainRange = h("input", { type: "range" });
  gainRange.min = "0.1";
  gainRange.max = "4";
  gainRange.step = "0.05";
  gainRange.style.width = "150px";
  const gainValue = h("span", { cls: "mono", attrs: { style: "width:38px" } });
  gainRange.addEventListener("input", () => {
    const v = num(gainRange.value, DEFAULT_SETTINGS.audioGain);
    gainValue.textContent = v.toFixed(2);
    patchSettings({ audioGain: v });
  });

  const fpsUnlimited = h("input", { type: "checkbox" });
  const fpsInput = numberInput(
    getSettings().audioFrameRate,
    (value) => patchSettings({ audioFrameRate: Math.max(0, Math.round(value)) }),
    { min: 0, max: 240, width: 96 },
  );
  const fpsLabel = h("span", { cls: "muted", text: t("audio.frameRateUnlimited") });
  const fpsWrap = h("div", { cls: "row" }, fpsInput, fpsUnlimited, fpsLabel);
  fpsUnlimited.addEventListener("change", () => {
    const unlimited = fpsUnlimited.checked;
    fpsInput.disabled = unlimited;
    if (unlimited) patchSettings({ audioFrameRate: 0 });
    else patchSettings({ audioFrameRate: num(fpsInput.value, 60) });
  });

  /* ------------------------------------------------------------ 设置卡片 */

  const settingsBody = h(
    "div",
    {},
    enabledRow.row,
    sourceHintRow.row,
    settingRow(t("audio.transport"), "", [h("span", { cls: "mono", text: t("audio.transportUdp") })]).row,
    settingRow(t("audio.udpHost"), "", [udpHostInput]).row,
    settingRow(t("audio.udpPort"), "", [udpPortInput]).row,
    settingRow(t("audio.gain"), "", [gainRange, gainValue]).row,
    settingRow(t("audio.frameRate"), "", [fpsWrap]).row,
  );
  const settingsCard = card(t("audio.settingsTitle"), t("audio.settingsDesc"), settingsBody);

  /* ------------------------------------------------------------ 状态卡 */

  const stateBadge = h("span", { cls: "badge" });
  const runningBadge = h("span", { cls: "badge" });
  const messageLine = h("div", { cls: "dim" });
  const packetsValue = h("div", { cls: "kv__v mono" });
  const dllValue = h("div", { cls: "kv__v mono" });
  const sourceValue = h("div", { cls: "kv__v mono" });
  const transportValue = h("div", { cls: "kv__v mono" });

  const statusBody = h(
    "div",
    {},
    h("div", { cls: "row", attrs: { style: "margin-bottom:10px" } }, stateBadge, runningBadge),
    messageLine,
    h(
      "div",
      { cls: "kv", attrs: { style: "margin-top:12px" } },
      h("div", { cls: "kv__k", text: t("audio.source") }),
      sourceValue,
      h("div", { cls: "kv__k", text: t("audio.transport") }),
      transportValue,
      h("div", { cls: "kv__k", text: t("audio.packets") }),
      packetsValue,
      h("div", { cls: "kv__k", text: t("audio.dll") }),
      dllValue,
    ),
  );
  const statusCard = card(t("audio.statusTitle"), "", statusBody);

  /* ------------------------------------------------------------ 可视化 */

  const visualizer = createVisualizer();
  const vizCard = card(t("audio.visualizer"), "", visualizer.element);

  const body = h(
    "div",
    { cls: "page tab-page" },
    h("div", { cls: "grid-2" }, settingsCard.element, statusCard.element),
    vizCard.element,
  );

  /* ------------------------------------------------------------ 渲染 */

  function sourceLabel(status: AudioStatus): string {
    if (status.source === "process") return status.processName || t("common.unknown");
    if (status.source === "auto") return t("audio.sourceAuto");
    return t("audio.sourceOff");
  }

  function renderStatus(): void {
    const status = getAudioStatus();
    const stateLabels: Record<AudioStatus["state"], string> = {
      idle: t("audio.state.idle"),
      waiting: t("audio.state.waiting"),
      capturing: t("audio.state.capturing"),
      error: t("audio.state.error"),
    };
    stateBadge.textContent = stateLabels[status.state] ?? status.state;
    stateBadge.className =
      "badge" +
      (status.state === "capturing"
        ? " badge--ok"
        : status.state === "error"
          ? " badge--danger"
          : status.state === "waiting"
            ? " badge--warn"
            : "");

    runningBadge.textContent = status.running ? t("audio.running") : t("audio.notRunning");
    runningBadge.className = `badge${status.running ? " badge--ok" : ""}`;

    messageLine.textContent = status.message || "";
    messageLine.hidden = status.message === "";

    const name = sourceLabel(status);
    sourceValue.textContent = status.pid > 0 ? `${name} · PID ${status.pid}` : name;
    sourceHint.textContent = status.pid > 0 ? `${name} · PID ${status.pid}` : name;

    const transportLabel = `${t("audio.transportUdp")} · ${status.udpHost}:${status.udpPort}`;
    transportValue.textContent = transportLabel;

    packetsValue.textContent = status.packetsSent.toLocaleString();
    dllValue.textContent = status.dllOk
      ? t("audio.dllOk", [status.dllVersion])
      : t("audio.dllMissing");
    dllValue.style.color = status.dllOk ? "var(--ok)" : "var(--danger)";
  }

  /** 把设置里的值刷回控件（避免切走再回来时是旧值） */
  function syncControls(): void {
    const s = getSettings();
    const enabledInput = enabledToggle.querySelector("input");
    if (enabledInput) enabledInput.checked = s.audioEnabled;
    if (udpHostInput.value !== s.udpHost) udpHostInput.value = s.udpHost;
    if (udpPortInput.value !== String(s.udpPort)) udpPortInput.value = String(s.udpPort);
    if (gainRange.value !== String(s.audioGain)) gainRange.value = String(s.audioGain);
    gainValue.textContent = s.audioGain.toFixed(2);
    const unlimited = s.audioFrameRate === 0;
    fpsUnlimited.checked = unlimited;
    fpsInput.disabled = unlimited;
    if (!unlimited && fpsInput.value !== String(s.audioFrameRate)) {
      fpsInput.value = String(s.audioFrameRate);
    }
    renderStatus();
  }

  /** 语言切换后重刷本 tab 所有文案 */
  function renderLang(): void {
    enabledRow.setText(t("audio.enabled"), t("audio.enabledHint"));
    sourceHintRow.setText(t("audio.source"), t("audio.sourceDashboardHint"));
    settingsCard.setText(t("audio.settingsTitle"), t("audio.settingsDesc"));
    statusCard.setText(t("audio.statusTitle"), "");
    vizCard.setText(t("audio.visualizer"), "");
    fpsLabel.textContent = t("audio.frameRateUnlimited");
    visualizer.refreshTexts();
    renderStatus();
  }

  /* ------------------------------------------------------------ 订阅 */

  const unsubs: Array<() => void> = [];
  unsubs.push(
    onLangChange(() => renderLang()),
    subscribeSettings(() => {
      const s = getSettings();
      const enabledInput = enabledToggle.querySelector("input");
      if (enabledInput && enabledInput.checked !== s.audioEnabled) {
        enabledInput.checked = s.audioEnabled;
      }
      renderStatus();
    }),
    subscribeAudioStatus(() => renderStatus()),
  );

  void safeListen<AudioFramePayload>(EV.audioFrame, (payload) => visualizer.push(payload));
  void safeListen<AudioStatus>(EV.audioStatus, (payload) => {
    if (!payload) return;
    renderStatus();
  });

  syncControls();
  void api.audioStatus().then((res) => {
    if (res.ok) renderStatus();
    else reportFailure(res);
  });

  return {
    element: body,
    onShow(): void {
      syncControls();
    },
    destroy(): void {
      for (const un of unsubs) un();
      unsubs.length = 0;
      visualizer.reset();
    },
  };
}
