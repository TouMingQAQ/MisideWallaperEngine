//! 系统托盘图标 —— 程序常驻后台时的总控入口。
//!
//! * **左键**：显示 / 隐藏主界面；
//! * **右键**：应用/停止壁纸、暂停、静音、自动跟随音频源、退出。
//!
//! 菜单文字与勾选由扫描线程（1.2s）经 [`sync`] 刷新，所以不管从界面还是托盘改的状态，
//! 托盘上看到的都是最新的那一份。

use std::sync::Mutex;

use tauri::menu::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Manager, Wry};

use crate::commands::log;
use crate::win::control;

const TRAY_ID: &str = "miside-wallpaper-engine-tray";
const APP_NAME: &str = "miside-wallpaper-engine";

/// 菜单项句柄：建好之后还要改文字 / 勾选。
pub struct TrayHandles {
    tray: TrayIcon<Wry>,
    show_main: MenuItem<Wry>,
    apply_active: MenuItem<Wry>,
    toggle_pause: MenuItem<Wry>,
    muted: CheckMenuItem<Wry>,
    auto_audio: CheckMenuItem<Wry>,
    /// 上一次写进托盘的提示，避免每轮都去戳一次 Shell。
    last_tooltip: Mutex<String>,
}

pub fn setup(app: &AppHandle) -> tauri::Result<()> {
    let show_main = MenuItem::with_id(app, "show-main", "隐藏主界面", true, None::<&str>)?;
    let apply_active = MenuItem::with_id(app, "apply-active", "应用到桌面", true, None::<&str>)?;
    let toggle_pause = MenuItem::with_id(app, "toggle-pause", "暂停壁纸", false, None::<&str>)?;
    let muted = CheckMenuItem::with_id(app, "muted", "静音", true, false, None::<&str>)?;
    let auto_audio =
        CheckMenuItem::with_id(app, "auto-audio", "自动跟随音频源", true, true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "退出", true, None::<&str>)?;

    let menu = Menu::with_items(
        app,
        &[
            &show_main,
            &PredefinedMenuItem::separator(app)?,
            &apply_active,
            &toggle_pause,
            &muted,
            &PredefinedMenuItem::separator(app)?,
            &auto_audio,
            &PredefinedMenuItem::separator(app)?,
            &quit,
        ],
    )?;

    let mut builder = TrayIconBuilder::with_id(TRAY_ID)
        .tooltip(APP_NAME)
        .menu(&menu)
        // 左键留给「显示 / 隐藏」，菜单走右键
        .show_menu_on_left_click(false)
        .on_menu_event(on_menu_event)
        .on_tray_icon_event(on_tray_event);

    if let Some(icon) = app.default_window_icon() {
        builder = builder.icon(icon.clone());
    }

    let tray = builder.build(app)?;

    app.manage(TrayHandles {
        tray,
        show_main,
        apply_active,
        toggle_pause,
        muted,
        auto_audio,
        last_tooltip: Mutex::new(APP_NAME.to_string()),
    });

    sync(app);
    Ok(())
}

/// 把最新状态刷到托盘上。
pub fn sync(app: &AppHandle) {
    let Some(handles) = app.try_state::<TrayHandles>() else {
        return;
    };
    let settings = crate::commands::settings_of(app);
    let runtime = crate::commands::runtime_of(app);
    let running = runtime.mode != "stopped";

    let main_visible = app
        .get_webview_window("main")
        .and_then(|window| window.is_visible().ok())
        .unwrap_or(true);

    let _ = handles
        .show_main
        .set_text(if main_visible { "隐藏主界面" } else { "显示主界面" });
    let _ = handles.apply_active.set_text(if running {
        "停止壁纸"
    } else {
        "应用到桌面"
    });
    let _ = handles.toggle_pause.set_enabled(running);
    let _ = handles
        .toggle_pause
        .set_text(if runtime.paused { "继续壁纸" } else { "暂停壁纸" });
    let _ = handles.muted.set_checked(settings.muted);
    let _ = handles
        .auto_audio
        .set_checked(settings.audio_source != "process");

    let tooltip = match runtime.mode.as_str() {
        "desktop" => {
            let pause = if runtime.paused { " · 已暂停" } else { "" };
            if runtime.attached {
                format!(
                    "{APP_NAME} · 已应用到桌面（PID {}）{pause}",
                    runtime.pid
                )
            } else {
                format!("{APP_NAME} · 正在挂载壁纸（PID {}）{pause}", runtime.pid)
            }
        }
        "preview" => format!("{APP_NAME} · 预览中（PID {}）", runtime.pid),
        _ => {
            if settings.active_wallpaper.is_empty() {
                format!("{APP_NAME} · 还没有壁纸，左键打开主界面")
            } else {
                format!("{APP_NAME} · 左键显示 / 隐藏主界面")
            }
        }
    };

    let mut last = handles.last_tooltip.lock().unwrap_or_else(|e| e.into_inner());
    if *last != tooltip {
        *last = tooltip.clone();
        let _ = handles.tray.set_tooltip(Some(tooltip));
    }
}

/* ------------------------------------------------------------------ 交互 */

fn on_tray_event(tray: &TrayIcon<Wry>, event: TrayIconEvent) {
    // Windows 上按下 / 抬起各来一次，只认「抬起」，否则会来回切两下
    if let TrayIconEvent::Click {
        button: MouseButton::Left,
        button_state: MouseButtonState::Up,
        ..
    } = event
    {
        toggle_main(tray.app_handle());
    }
}

fn on_menu_event(app: &AppHandle, event: MenuEvent) {
    match event.id().as_ref() {
        "show-main" => toggle_main(app),
        "apply-active" => toggle_wallpaper(app),
        "toggle-pause" => {
            let app = app.clone();
            tauri::async_runtime::spawn_blocking(move || {
                match crate::commands::toggle_pause_blocking(&app) {
                    Ok(snapshot) => log(
                        &app,
                        "info",
                        if snapshot.paused {
                            "托盘：壁纸已暂停"
                        } else {
                            "托盘：壁纸已继续"
                        },
                    ),
                    Err(err) => log(&app, "warn", err),
                }
                sync(&app);
            });
        }
        "muted" => {
            let next = !crate::commands::settings_of(app).muted;
            let mut settings = crate::commands::settings_of(app);
            settings.muted = next;
            let _ = crate::prefs::store(app, &settings);
            crate::commands::set_settings_of(app, settings.clone());
            crate::prefs::broadcast(app, &settings);
            let runtime = crate::commands::runtime_of(app);
            if runtime.pid != 0 {
                if let Err(err) = control::set_process_volume(runtime.pid, None, Some(next)) {
                    log(app, "warn", err);
                }
            }
            log(
                app,
                "info",
                if next { "托盘：已静音" } else { "托盘：已取消静音" },
            );
            sync(app);
        }
        "auto-audio" => {
            let mut settings = crate::commands::settings_of(app);
            if settings.audio_source == "process" {
                settings.audio_source = "auto".to_string();
                settings.audio_process = String::new();
            } else {
                settings.audio_source = "process".to_string();
            }
            let settings = settings.normalized();
            let _ = crate::prefs::store(app, &settings);
            crate::commands::set_settings_of(app, settings.clone());
            crate::audio_link::configure(&settings);
            crate::prefs::broadcast(app, &settings);
            log(
                app,
                "info",
                if settings.audio_source == "process" {
                    "托盘：音频源改为指定进程（请在主界面选择）"
                } else {
                    "托盘：音频源改为自动跟随最响的进程"
                },
            );
            sync(app);
        }
        "quit" => app.exit(0),
        _ => {}
    }
}

/// 显示 / 隐藏主界面。最小化也算「藏起来了」。
fn toggle_main(app: &AppHandle) {
    let Some(window) = app.get_webview_window("main") else {
        return;
    };
    let visible = window.is_visible().unwrap_or(true);
    let minimized = window.is_minimized().unwrap_or(false);
    let shown = !visible || minimized;
    if shown {
        let _ = window.unminimize();
        let _ = window.show();
        let _ = window.set_focus();
    } else {
        let _ = window.hide();
    }
    sync(app);
}

/// 应用到桌面 / 停止壁纸。
fn toggle_wallpaper(app: &AppHandle) {
    let app = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let runtime = crate::commands::runtime_of(&app);
        if runtime.mode != "stopped" {
            crate::commands::stop_blocking(&app);
            sync(&app);
            return;
        }
        let settings = crate::commands::settings_of(&app);
        if settings.active_wallpaper.is_empty() {
            log(&app, "warn", "托盘：还没有选壁纸，先打开主界面导入并选择");
            toggle_main(&app);
            return;
        }
        match crate::commands::apply_blocking(&app, &settings.active_wallpaper) {
            Ok(_) => {}
            Err(err) => log(&app, "error", format!("托盘：应用壁纸失败 —— {err}")),
        }
        sync(&app);
    });
}
