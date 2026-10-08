//! 捕获归还的实机验证：直接调用宿主里那份逻辑，检查「合成按下 → 捕获被壁纸抢走 →
//! 归还给桌面图标列表」这条链路真的成立。
//!
//! 为什么需要它：`GetGUIThreadInfo` 的捕获归属、`AttachThreadInput` 能不能跨进程
//! `SetCapture`、以及「捕获到底落在哪个线程的队列上」，这些都是只能在真机上量的行为。
//! 单元测试量不到，所以这里连到当前桌面上的真实窗口来跑。
//!
//! 用法（需要一个正在跑的壁纸窗口；没有就跳过）：
//!   cargo test --manifest-path src-tauri/Cargo.toml --test capture_repair -- --nocapture

#![cfg(windows)]

use miside_wallpaper_engine_lib::win::desktop;
use windows::core::BOOL;

/// 当前桌面上的壁纸窗口（挂在 WorkerW 下的 `UnityWndClass`）。
fn find_wallpaper() -> Option<windows::Win32::Foundation::HWND> {
    use windows::core::w;
    use windows::Win32::Foundation::{HWND, LPARAM};
    use windows::Win32::UI::WindowsAndMessaging::{
        EnumChildWindows, EnumWindows, FindWindowExW, GetClassNameW,
    };

    unsafe extern "system" fn find_unity(child: HWND, data: LPARAM) -> BOOL {
        let mut buffer = [0u16; 128];
        let length = GetClassNameW(child, &mut buffer);
        if length > 0 {
            let class = String::from_utf16_lossy(&buffer[..length as usize]);
            if class == "UnityWndClass" {
                *(data.0 as *mut HWND) = child;
                return BOOL(0);
            }
        }
        BOOL(1)
    }

    unsafe extern "system" fn find_worker(window: HWND, data: LPARAM) -> BOOL {
        let mut buffer = [0u16; 128];
        let length = GetClassNameW(window, &mut buffer);
        if length > 0 {
            let class = String::from_utf16_lossy(&buffer[..length as usize]);
            if class == "WorkerW" {
                let mut found: HWND = HWND::default();
                let _ = EnumChildWindows(
                    Some(window),
                    Some(find_unity),
                    LPARAM(&mut found as *mut HWND as isize),
                );
                if !found.is_invalid() {
                    *(data.0 as *mut HWND) = found;
                    return BOOL(0);
                }
            }
        }
        // 有些挂法里 Unity 窗口直接在 Progman 下
        let _ = FindWindowExW(Some(window), None, w!("UnityWndClass"), None);
        BOOL(1)
    }

    let mut found: HWND = HWND::default();
    unsafe {
        let _ = EnumWindows(
            Some(find_worker),
            LPARAM(&mut found as *mut HWND as isize),
        );
    }
    (!found.is_invalid()).then_some(found)
}

/// 合成一次按下，观察捕获是否被壁纸抢走，再用宿主那套逻辑还回去。
#[test]
fn synthetic_press_capture_is_handed_back_to_the_desktop() {
    use windows::Win32::Foundation::{LPARAM, WPARAM};
    use windows::Win32::UI::WindowsAndMessaging::{PostMessageW, WM_LBUTTONDOWN, WM_LBUTTONUP};

    let Some(wallpaper) = find_wallpaper() else {
        eprintln!("跳过：当前桌面上没有壁纸窗口（先应用一个壁纸再跑这个测试）");
        return;
    };

    // 干净起点：先确保没人拿着捕获
    let _ = desktop::capture_of_desktop();

    unsafe {
        // 1. 合成按下（宿主转发鼠标时就是这么干的）
        let _ = PostMessageW(Some(wallpaper), WM_LBUTTONDOWN, WPARAM(1), LPARAM(0x000A000A));
    }
    std::thread::sleep(std::time::Duration::from_millis(120));

    let stolen = desktop::capture_of_thread(desktop::window_thread(
        desktop::desktop_window().expect("桌面窗口"),
    ));
    let stolen_is_wallpaper = !stolen.is_invalid()
        && (stolen == wallpaper
            || unsafe {
                windows::Win32::UI::WindowsAndMessaging::IsChild(wallpaper, stolen).as_bool()
            });
    eprintln!("合成按下后捕获 = 0x{:X}（是壁纸窗口：{stolen_is_wallpaper}）", stolen.0 as isize);

    // 2. 用宿主那套逻辑归还
    let _ = desktop::icon_listview();
    miside_wallpaper_engine_lib::win::input::hand_capture_back_for_test(0, wallpaper.0 as isize);

    let after = desktop::capture_of_thread(desktop::window_thread(
        desktop::desktop_window().expect("桌面窗口"),
    ));
    eprintln!("归还之后捕获 = 0x{:X}", after.0 as isize);

    // 收尾：补一个抬起，别把按键状态留在半路
    unsafe {
        let _ = PostMessageW(Some(wallpaper), WM_LBUTTONUP, WPARAM(0), LPARAM(0x000A000A));
    }
    std::thread::sleep(std::time::Duration::from_millis(150));

    if stolen_is_wallpaper {
        assert!(
            !(!after.is_invalid()
                && (after == wallpaper
                    || unsafe {
                        windows::Win32::UI::WindowsAndMessaging::IsChild(wallpaper, after).as_bool()
                    })),
            "捕获必须从壁纸窗口手里拿回来，否则桌面图标会点不动"
        );
    } else {
        eprintln!("注意：这次合成按下没有抢到捕获（壁纸没走 DefWindowProc 的 SetCapture 路径）");
    }
}