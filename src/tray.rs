use std::sync::OnceLock;

use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Shell::{
    Shell_NotifyIconW, NOTIFYICONDATAW, NIF_ICON, NIF_MESSAGE, NIM_ADD, NIM_DELETE,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu, DispatchMessageW,
    GetCursorPos, GetMessageW, IDI_APPLICATION, LoadIconW, PostMessageW, PostQuitMessage,
    RegisterClassW, SetForegroundWindow, TrackPopupMenu, TranslateMessage, WM_APP, WM_DESTROY,
    WM_LBUTTONDBLCLK, WM_QUIT, WM_RBUTTONUP, WM_USER, WNDCLASSW, MF_STRING, TPM_RETURNCMD,
    TPM_RIGHTBUTTON,
};

/// 托盘图标回调消息
const WM_TRAY: u32 = WM_USER + 100;
/// 自定义命令：显示托盘图标
const WM_TRAY_SHOW: u32 = WM_APP + 1;
/// 自定义命令：移除托盘图标
const WM_TRAY_HIDE: u32 = WM_APP + 2;
/// 托盘图标唯一 ID
const TRAY_ICON_ID: u32 = 1;
/// 菜单项 ID
const MENU_SHOW: usize = 1;
const MENU_QUIT: usize = 2;

type TrayCallback = Box<dyn Fn() + Send + Sync + 'static>;

/// 托盘回调（在托盘线程触发，内部应转发到 UI 线程）
struct TrayHandlers {
    on_restore: TrayCallback,
    on_quit: TrayCallback,
}

static HANDLERS: OnceLock<TrayHandlers> = OnceLock::new();
/// 托盘消息窗口句柄
static TRAY_HWND: OnceLock<isize> = OnceLock::new();

/// 启动托盘：创建专用消息窗口并常驻消息循环
pub fn spawn(on_restore: impl Fn() + Send + Sync + 'static, on_quit: impl Fn() + Send + Sync + 'static) {
    let _ = HANDLERS.set(TrayHandlers {
        on_restore: Box::new(on_restore),
        on_quit: Box::new(on_quit),
    });
    std::thread::spawn(|| {
        unsafe {
            let class_name = "clipboard-yy-tray\0".encode_utf16().collect::<Vec<u16>>();
            let hinstance = GetModuleHandleW(std::ptr::null());

            let wnd_class = WNDCLASSW {
                lpfnWndProc: Some(tray_wnd_proc),
                hInstance: hinstance,
                lpszClassName: class_name.as_ptr(),
                ..Default::default()
            };
            if RegisterClassW(&wnd_class) == 0 {
                return;
            }
            let hwnd = CreateWindowExW(
                0,
                class_name.as_ptr(),
                class_name.as_ptr(),
                0,
                0,
                0,
                0,
                0,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                hinstance,
                std::ptr::null(),
            );
            if hwnd.is_null() {
                return;
            }
            let _ = TRAY_HWND.set(hwnd as isize);

            let mut msg = std::mem::zeroed();
            while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
    });
}

/// 主线程调用：显示托盘图标
pub fn show_icon() {
    post_command(WM_TRAY_SHOW);
}

/// 主线程调用：移除托盘图标
pub fn hide_icon() {
    post_command(WM_TRAY_HIDE);
}

/// 主线程调用：移除图标并让消息循环退出（进程即将结束）
pub fn shutdown() {
    post_command(WM_QUIT);
}

fn post_command(cmd: u32) {
    if let Some(hwnd) = TRAY_HWND.get() {
        unsafe {
            PostMessageW(*hwnd as HWND, cmd, 0, 0);
        }
    }
}

unsafe fn add_icon() {
    let Some(hwnd) = TRAY_HWND.get() else { return };
    let mut nid: NOTIFYICONDATAW = std::mem::zeroed();
    nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
    nid.hWnd = *hwnd as HWND;
    nid.uID = TRAY_ICON_ID;
    nid.uFlags = NIF_MESSAGE | NIF_ICON;
    nid.uCallbackMessage = WM_TRAY;
    nid.hIcon = LoadIconW(std::ptr::null_mut(), IDI_APPLICATION);
    // 悬浮提示文本
    let tip = "剪贴板工具 — 双击恢复\0".encode_utf16().collect::<Vec<u16>>();
    for (i, c) in tip.iter().take(127).enumerate() {
        nid.szTip[i] = *c;
    }
    Shell_NotifyIconW(NIM_ADD, &nid);
}

unsafe fn remove_icon() {
    let Some(hwnd) = TRAY_HWND.get() else { return };
    let mut nid: NOTIFYICONDATAW = std::mem::zeroed();
    nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
    nid.hWnd = *hwnd as HWND;
    nid.uID = TRAY_ICON_ID;
    Shell_NotifyIconW(NIM_DELETE, &nid);
}

/// 右键弹出菜单：显示 / 退出
unsafe fn show_context_menu(hwnd: HWND) {
    let menu = CreatePopupMenu();
    if menu.is_null() {
        return;
    }
    let item_show = "显示窗口\0".encode_utf16().collect::<Vec<u16>>();
    let item_quit = "退出\0".encode_utf16().collect::<Vec<u16>>();
    AppendMenuW(menu, MF_STRING, MENU_SHOW, item_show.as_ptr());
    AppendMenuW(menu, MF_STRING, MENU_QUIT, item_quit.as_ptr());

    let mut pt = POINT { x: 0, y: 0 };
    GetCursorPos(&mut pt);
    // 菜单需要窗口在前台才能接收命令
    SetForegroundWindow(hwnd);
    let cmd = TrackPopupMenu(
        menu,
        TPM_RIGHTBUTTON | TPM_RETURNCMD,
        pt.x,
        pt.y,
        0,
        hwnd,
        std::ptr::null(),
    );
    DestroyMenu(menu);

    if cmd == MENU_SHOW as i32 {
        if let Some(h) = HANDLERS.get() {
            (h.on_restore)();
        }
    } else if cmd == MENU_QUIT as i32 {
        if let Some(h) = HANDLERS.get() {
            (h.on_quit)();
        }
    }
}

unsafe extern "system" fn tray_wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_TRAY => match (lparam & 0xFFFF) as u32 {
            WM_LBUTTONDBLCLK => {
                if let Some(h) = HANDLERS.get() {
                    (h.on_restore)();
                }
                0
            }
            WM_RBUTTONUP => {
                show_context_menu(hwnd);
                0
            }
            _ => 0,
        },
        WM_TRAY_SHOW => {
            add_icon();
            0
        }
        WM_TRAY_HIDE => {
            remove_icon();
            0
        }
        WM_QUIT => {
            remove_icon();
            PostQuitMessage(0);
            0
        }
        WM_DESTROY => {
            PostQuitMessage(0);
            0
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}