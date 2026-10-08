/**
 * src/settings-panel.ts
 * 设置弹窗「通用」tab：分组卡片（外观与语言 / 壁纸进程 / 暂停条件 / 输入 / 系统），
 * 每项改动立刻 save_settings（300ms 防抖），成功后给「已保存」反馈。
 */
import { api, type MonitorInfo, type Settings, type ThemeMode } from "./api";
import {
  getDefaultLibraryDir,
  getLastSavedAt,
  getLibraryDir,
  getSettings,
  lang,
  patchSettings,
  reportFailure,
  store,
  subscribeLibraryDir,
  subscribeSaveState,
  subscribeSettings,
  t,
  toast,
} from "./state";
import {
  card,
  h,
  numberInput,
  segmented,
  settingRow,
  textInput,
  toggle,
} from "./ui";
import { createParamsEditor } from "./params";
import type { Tab } from "./tabs";

/** 保存成功后「已保存」提示显示的时长 */
const SAVED_FLASH_MS = 1400;

export function createGeneralTab(): Tab {
  let lastSavedSeen = 0;
  let flashTimer = 0;
  let monitors: MonitorInfo[] = [];

  const savedFlash = h("span", { cls: "saved-flash" });

  /* ------------------------------------------------------------ 外观与语言 */

  const themeSeg = segmented<ThemeMode>(
    [
      { value: "dark", label: t("settings.themeDark") },
      { value: "light", label: t("settings.themeLight") },
      { value: "system", label: t("settings.themeSystem") },
    ],
    getSettings().themeMode,
    (value) => patchSettings({ themeMode: value }),
  );

  const languageSelect = h("select", { cls: "select" });
  for (const value of ["zh-CN", "en-US"] as const) {
    const opt = h("option", {
      text: value === "zh-CN" ? t("settings.languageZh") : t("settings.languageEn"),
    });
    opt.value = value;
    languageSelect.append(opt);
  }
  languageSelect.addEventListener("change", () => {
    patchSettings({ language: languageSelect.value === "en-US" ? "en-US" : "zh-CN" });
  });

  const appearanceBody = h(
    "div",
    {},
    settingRow(t("settings.themeMode"), "", [themeSeg.element]).row,
    settingRow(t("settings.language"), "", [languageSelect]).row,
  );
  const appearanceCard = card(t("settings.groupAppearance"), "", appearanceBody);

  /* ------------------------------------------------------------ 壁纸进程 */

  const fpsUnlimited = h("input", { type: "checkbox" });
  const fpsInput = numberInput(getSettings().targetFps, (value) =>
    patchSettings({ targetFps: Math.max(0, Math.round(value)) }),
    { min: 0, max: 480, width: 96 },
  );
  const fpsUnlimitedLabel = h("span", { cls: "muted", text: t("common.unlimited") });
  const fpsWrap = h("div", { cls: "row" }, fpsInput, fpsUnlimited, fpsUnlimitedLabel);
  fpsUnlimited.addEventListener("change", () => {
    const unlimited = fpsUnlimited.checked;
    fpsInput.disabled = unlimited;
    patchSettings({ targetFps: unlimited ? 0 : Math.max(1, Number(fpsInput.value) || 60) });
  });

  const renderSeg = segmented<string>(
    [
      { value: "0.5", label: "0.5×" },
      { value: "0.75", label: "0.75×" },
      { value: "1", label: "1.0×" },
    ],
    String(getSettings().renderScale),
    (value) => patchSettings({ renderScale: Number(value) }),
  );

  // 图形 API：落到壁纸进程的 -force-d3d11 / -force-d3d12
  const graphicsSeg = segmented<string>(
    [
      { value: "auto", label: t("settings.graphicsApi.auto") },
      { value: "d3d11", label: t("settings.graphicsApi.d3d11") },
      { value: "d3d12", label: t("settings.graphicsApi.d3d12") },
    ],
    getSettings().graphicsApi,
    (value) => patchSettings({ graphicsApi: value as "auto" | "d3d11" | "d3d12" }),
  );

  const monitorSelect = h("select", { cls: "select", attrs: { style: "min-width:250px" } });
  monitorSelect.addEventListener("change", () => {
    patchSettings({ monitorIndex: Number(monitorSelect.value) });
  });

  const extraArgsInput = h("textarea", { cls: "input" });
  extraArgsInput.value = getSettings().extraArgs;
  extraArgsInput.rows = 3;
  extraArgsInput.placeholder = t("settings.extraArgsPlaceholder");
  extraArgsInput.style.width = "340px";
  extraArgsInput.addEventListener("input", () => {
    patchSettings({ extraArgs: extraArgsInput.value });
  });

  const volumeRange = h("input", { type: "range" });
  volumeRange.min = "0";
  volumeRange.max = "1";
  volumeRange.step = "0.01";
  volumeRange.style.width = "150px";
  const volumeValue = h("span", { cls: "mono", attrs: { style: "width:38px" } });
  volumeRange.addEventListener("input", () => {
    const v = Number(volumeRange.value);
    volumeValue.textContent = v.toFixed(2);
    void store.setVolume(v);
  });

  const mutedToggle = toggle(getSettings().muted, (checked) => {
    void store.setMuted(checked);
  });

  const processBody = h(
    "div",
    {},
    settingRow(t("settings.targetFps"), t("settings.targetFpsHint"), [fpsWrap]).row,
    settingRow(t("settings.renderScale"), t("settings.renderScaleHint"), [renderSeg.element]).row,
    settingRow(t("settings.graphicsApi"), t("settings.graphicsApiHint"), [graphicsSeg.element]).row,
    settingRow(t("settings.monitor"), t("settings.monitorHint"), [monitorSelect]).row,
    settingRow(t("settings.extraArgs"), t("settings.extraArgsHint"), [extraArgsInput]).row,
    settingRow(t("settings.volume"), "", [volumeRange, volumeValue]).row,
    settingRow(t("settings.muted"), "", [mutedToggle]).row,
  );
  const processCard = card(t("settings.groupProcess"), "", processBody);

  /* ------------------------------------------------------------ 暂停条件 */

  const pauseFullscreen = toggle(getSettings().pauseOnFullscreen, (checked) =>
    patchSettings({ pauseOnFullscreen: checked }),
  );
  const pauseBattery = toggle(getSettings().pauseOnBattery, (checked) =>
    patchSettings({ pauseOnBattery: checked }),
  );
  const pauseForeground = toggle(getSettings().pauseWhenForeground, (checked) =>
    patchSettings({ pauseWhenForeground: checked }),
  );
  const pauseBody = h(
    "div",
    {},
    settingRow(t("settings.pauseOnFullscreen"), t("settings.pauseOnFullscreenHint"), [
      pauseFullscreen,
    ]).row,
    settingRow(t("settings.pauseOnBattery"), t("settings.pauseOnBatteryHint"), [pauseBattery]).row,
    settingRow(t("settings.pauseWhenForeground"), t("settings.pauseWhenForegroundHint"), [
      pauseForeground,
    ]).row,
  );
  const pauseCard = card(t("settings.groupPause"), "", pauseBody);

  /* ------------------------------------------------------------ 输入 */

  const inputForwardToggle = toggle(getSettings().inputForward, (checked) =>
    patchSettings({ inputForward: checked }),
  );
  const inputLockedToggle = toggle(getSettings().inputLocked, (checked) =>
    patchSettings({ inputLocked: checked }),
  );
  const inputLockedRow = settingRow(t("settings.inputLocked"), t("settings.inputLockedHint"), [
    inputLockedToggle,
  ]);
  const inputBody = h(
    "div",
    {},
    settingRow(t("settings.inputForward"), t("settings.inputForwardHint"), [inputForwardToggle])
      .row,
    inputLockedRow.row,
  );
  const inputCard = card(t("settings.groupInput"), "", inputBody);

  /* ------------------------------------------------------------ 系统 */

  const autostartToggle = toggle(getSettings().autoStart, (checked) => {
    const el = autostartToggle.querySelector("input");
    if (el) el.disabled = true;
    void store.setAutostart(checked).finally(() => {
      const input = autostartToggle.querySelector("input");
      if (input) input.disabled = false;
    });
  });

  const closeToTrayToggle = toggle(getSettings().closeToTray, (checked) =>
    patchSettings({ closeToTray: checked }),
  );

  const libraryDirInput = textInput(
    getSettings().libraryDir,
    (value) => patchSettings({ libraryDir: value.trim() }),
    { mono: true, width: 300, placeholder: t("common.builtin") },
  );

  const libraryDirHint = h("div", { cls: "setting__hint" });
  const browseDirBtn = h("button", { cls: "btn btn--sm", html: t("common.browse") });
  browseDirBtn.addEventListener("click", () => {
    void (async () => {
      browseDirBtn.disabled = true;
      try {
        const res = await api.pickLibraryDir();
        if (!res.ok) {
          reportFailure(res, t("settings.libraryDir"));
          return;
        }
        if (res.value === null) return;
        libraryDirInput.value = res.value;
        patchSettings({ libraryDir: res.value });
      } finally {
        browseDirBtn.disabled = false;
      }
    })();
  });

  const resetDirBtn = h("button", { cls: "btn btn--sm", html: t("settings.libraryDirReset") });
  resetDirBtn.addEventListener("click", () => {
    libraryDirInput.value = "";
    patchSettings({ libraryDir: "" });
    toast("info", t("settings.libraryDirResetDone"));
  });

  const libraryDirRow = settingRow(t("settings.libraryDir"), "", [
    libraryDirInput,
    browseDirBtn,
    resetDirBtn,
  ]);

  const autoApplyToggle = toggle(getSettings().autoApply, (checked) =>
    patchSettings({ autoApply: checked }),
  );

  const systemBody = h(
    "div",
    {},
    settingRow(t("settings.autoStart"), t("settings.autoStartHint"), [autostartToggle]).row,
    settingRow(t("settings.closeToTray"), t("settings.closeToTrayHint"), [closeToTrayToggle]).row,
    libraryDirRow.row,
    settingRow(t("settings.autoApply"), t("settings.autoApplyHint"), [autoApplyToggle]).row,
  );
  const systemCard = card(t("settings.groupSystem"), "", systemBody);

  /* ------------------------------------------------------------ 自定义参数 */

  // 参数编辑器自带增删改 + 壁纸端上报的可选项下拉，整块塞进一张卡片
  const paramsEditor = createParamsEditor();
  const reportPortInput = numberInput(
    getSettings().reportPort,
    (value) => patchSettings({ reportPort: Math.max(0, Math.round(value)) }),
    { min: 0, max: 65535, width: 96 },
  );
  const paramsCard = card(
    t("params.title"),
    t("params.desc"),
    h(
      "div",
      {},
      paramsEditor.element,
      settingRow(t("params.reportPort"), t("params.reportPortHint"), [reportPortInput]).row,
    ),
    paramsEditor.addButton,
  );

  /* ------------------------------------------------------------ 页面骨架 */

  // tab 名已经说明这是「通用」，不再自带标题，只留「已保存」反馈
  const head = h("div", { cls: "page__head" }, savedFlash);
  const body = h(
    "div",
    { cls: "page" },
    head,
    appearanceCard.element,
    processCard.element,
    paramsCard.element,
    pauseCard.element,
    inputCard.element,
    systemCard.element,
  );

  /* ------------------------------------------------------------ 渲染 */

  function renderMonitors(): void {
    const previous = String(getSettings().monitorIndex);
    const options: HTMLOptionElement[] = [];
    const all = h("option", { text: t("settings.monitorAll") });
    all.value = "-1";
    options.push(all);
    for (const m of monitors) {
      const primary = m.primary ? ` · ${t("settings.monitorPrimary")}` : "";
      const opt = h("option", {
        text: t("settings.monitorItem", [m.name || `#${m.index}`, m.width, m.height, m.scaleFactor]) + primary,
      });
      opt.value = String(m.index);
      options.push(opt);
    }
    monitorSelect.replaceChildren(...options);
    monitorSelect.value = previous;
    if (monitorSelect.selectedIndex < 0) monitorSelect.value = "-1";
  }

  function renderStatic(): void {
    savedFlash.textContent = t("settings.savedHint");
    appearanceCard.setText(t("settings.groupAppearance"), "");
    processCard.setText(t("settings.groupProcess"), "");
    pauseCard.setText(t("settings.groupPause"), "");
    inputCard.setText(t("settings.groupInput"), "");
    systemCard.setText(t("settings.groupSystem"), "");
    paramsCard.setText(t("params.title"), t("params.desc"));
    paramsEditor.renderStatic();
    themeSeg.setLabels([t("settings.themeDark"), t("settings.themeLight"), t("settings.themeSystem")]);
    const langOptions = languageSelect.querySelectorAll("option");
    if (langOptions[0]) langOptions[0].textContent = t("settings.languageZh");
    if (langOptions[1]) langOptions[1].textContent = t("settings.languageEn");
    fpsUnlimitedLabel.textContent = t("common.unlimited");
    extraArgsInput.placeholder = t("settings.extraArgsPlaceholder");
    browseDirBtn.textContent = t("common.browse");
    resetDirBtn.textContent = t("settings.libraryDirReset");
    renderMonitors();
  }

  /** 把设置里的值同步回所有控件（不触发保存） */
  function syncControls(force = false): void {
    const s: Settings = getSettings();

    themeSeg.setValue(s.themeMode);
    languageSelect.value = s.language;

    const fpsOn = s.targetFps === 0;
    fpsUnlimited.checked = fpsOn;
    fpsInput.disabled = fpsOn;
    if (!fpsOn && (force || fpsInput.value !== String(s.targetFps))) {
      fpsInput.value = String(s.targetFps);
    }
    renderSeg.setValue(String(s.renderScale));
    monitorSelect.value = String(s.monitorIndex);
    if (monitorSelect.selectedIndex < 0) monitorSelect.value = "-1";
    if (document.activeElement !== reportPortInput && reportPortInput.value !== String(s.reportPort)) {
      reportPortInput.value = String(s.reportPort);
    }
    if (force || document.activeElement !== extraArgsInput) {
      if (extraArgsInput.value !== s.extraArgs) extraArgsInput.value = s.extraArgs;
    }
    if (document.activeElement !== volumeRange) {
      volumeRange.value = String(s.volume);
    }
    volumeValue.textContent = s.volume.toFixed(2);

    const setToggle = (wrap: HTMLElement, checked: boolean): void => {
      const input = wrap.querySelector("input");
      if (input && input.checked !== checked) input.checked = checked;
    };
    setToggle(mutedToggle, s.muted);
    setToggle(pauseFullscreen, s.pauseOnFullscreen);
    setToggle(pauseBattery, s.pauseOnBattery);
    setToggle(pauseForeground, s.pauseWhenForeground);
    setToggle(inputForwardToggle, s.inputForward);
    setToggle(inputLockedToggle, s.inputLocked);
    setToggle(autostartToggle, s.autoStart);
    setToggle(closeToTrayToggle, s.closeToTray);
    setToggle(autoApplyToggle, s.autoApply);

    // 没开转发时锁定没有意义，直接置灰
    const lockInput = inputLockedToggle.querySelector("input");
    if (lockInput && !inputForwardToggle.querySelector("input")?.disabled) {
      lockInput.disabled = !s.inputForward;
    }

    if (document.activeElement !== libraryDirInput) {
      if (libraryDirInput.value !== s.libraryDir) libraryDirInput.value = s.libraryDir;
    }
    renderLibraryDirHint();
  }

  function renderLibraryDirHint(): void {
    const resolved = getLibraryDir() || getDefaultLibraryDir();
    libraryDirHint.textContent = resolved
      ? t("library.dir", [resolved])
      : t("settings.libraryDirHint");
  }

  function flashSaved(): void {
    savedFlash.classList.add("is-on");
    if (flashTimer !== 0) window.clearTimeout(flashTimer);
    flashTimer = window.setTimeout(() => {
      savedFlash.classList.remove("is-on");
      flashTimer = 0;
    }, SAVED_FLASH_MS);
  }

  /** 保存完成 → 闪一下「已保存」；刚保存过（1.4s 内）也补一次，避免防抖期间看不到反馈 */
  function checkSaved(force: boolean): void {
    const at = getLastSavedAt();
    if (at === 0) return;
    if (force && Date.now() - at < SAVED_FLASH_MS) {
      lastSavedSeen = at;
      flashSaved();
      return;
    }
    if (at !== lastSavedSeen) {
      lastSavedSeen = at;
      flashSaved();
    }
  }

  async function refreshMonitors(): Promise<void> {
    const res = await api.listMonitors();
    if (res.ok) {
      monitors = res.value;
      renderMonitors();
      return;
    }
    // 枚举失败（后端未就绪）不打扰用户，只保留「全部显示器」选项
    monitors = [];
    renderMonitors();
  }

  /* ------------------------------------------------------------ 订阅 */

  const unsubs: Array<() => void> = [];
  let lastLang = lang();
  unsubs.push(
    subscribeSettings(() => {
      if (lang() !== lastLang) {
        lastLang = lang();
        renderStatic();
      }
      syncControls(true);
    }),
    subscribeSaveState(() => checkSaved(false)),
    subscribeLibraryDir(() => renderLibraryDirHint()),
  );

  renderStatic();
  syncControls(true);
  void refreshMonitors();

  return {
    element: body,
    onShow(): void {
      renderStatic();
      syncControls(true);
      // 从别的页面回来时，可能刚刚改过设置（比如壁纸库换了目录）
      checkSaved(true);
      void refreshMonitors();
      renderLibraryDirHint();
      // 壁纸端可能在这期间上报过新的可选值（例如换了服装表）
      paramsEditor.refresh();
    },
    destroy(): void {
      if (flashTimer !== 0) window.clearTimeout(flashTimer);
      for (const un of unsubs) un();
      unsubs.length = 0;
      paramsEditor.destroy();
    },
  };
}
