use std::borrow::Cow;
use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use global_hotkey::hotkey::{Code, HotKey, Modifiers};
use global_hotkey::{GlobalHotKeyEvent, GlobalHotKeyManager, HotKeyState};
use slint::{Image, Model, ModelRc, Timer, TimerMode, VecModel};
use windows_sys::Win32::Foundation::{GlobalFree, HWND, POINT, RECT};
use windows_sys::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, GetClipboardData, OpenClipboard, SetClipboardData,
};
use windows_sys::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
use windows_sys::Win32::UI::Shell::{DragQueryFileW, DROPFILES};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    FindWindowW, GetCursorPos, GetWindowRect, IsWindowVisible, SetForegroundWindow, SetWindowPos,
    ShowWindow, SWP_NOACTIVATE, SWP_NOSIZE, SWP_NOZORDER, SW_HIDE, SW_RESTORE, SW_SHOW,
};

mod storage;
mod tray;

slint::include_modules!();

/// 历史记录上限，超出后丢弃最旧的
const MAX_ENTRIES: usize = 50;

/// CF_HDROP 剪贴板格式（拖放文件列表）
const CF_HDROP: u32 = 15;

/// 剪贴板内容的三种形态
enum ClipContent {
    Text(String),
    Image {
        rgba: Vec<u8>,
        width: u32,
        height: u32,
    },
    Files(Vec<PathBuf>),
}

/// 窗口句柄缓存（Windows 句柄本质是指针，用 isize 存储保证 Sync）
static HWND_CACHE: OnceLock<isize> = OnceLock::new();

/// 窗口标题（与 app.slint 保持一致）
fn window_title_wide() -> Vec<u16> {
    "剪贴板工具".encode_utf16().chain(std::iter::once(0)).collect()
}

/// 通过窗口标题查找 Win32 句柄；找到后缓存
fn find_window_hwnd() -> Option<isize> {
    if let Some(h) = HWND_CACHE.get() {
        return Some(*h);
    }
    unsafe {
        let title = window_title_wide();
        let hwnd = FindWindowW(std::ptr::null(), title.as_ptr());
        if !hwnd.is_null() {
            let _ = HWND_CACHE.set(hwnd as isize);
            return Some(hwnd as isize);
        }
    }
    None
}

/// 显示窗口（SW_RESTORE + SW_SHOW）并置为前台
fn show_window(hwnd: isize) {
    unsafe {
        ShowWindow(hwnd as HWND, SW_RESTORE);
        ShowWindow(hwnd as HWND, SW_SHOW);
        SetForegroundWindow(hwnd as HWND);
    }
}

/// 隐藏窗口到托盘（Win32 原生隐藏；事件循环不受影响，轮询继续）
fn hide_window(hwnd: isize) {
    unsafe {
        ShowWindow(hwnd as HWND, SW_HIDE);
    }
}

/// 查询窗口当前是否可见（用于热键切换判断）
fn is_window_visible(hwnd: isize) -> bool {
    unsafe { IsWindowVisible(hwnd as HWND) != 0 }
}

/// 获取窗口左上角物理坐标
fn window_pos(hwnd: isize) -> Option<(i32, i32)> {
    unsafe {
        let mut rect = RECT {
            left: 0,
            top: 0,
            right: 0,
            bottom: 0,
        };
        if GetWindowRect(hwnd as HWND, &mut rect) != 0 {
            Some((rect.left, rect.top))
        } else {
            None
        }
    }
}

/// 设置窗口左上角物理坐标（Win32 原生，绕开 winit 无边框坐标问题）
fn set_window_pos(hwnd: isize, left: i32, top: i32) {
    unsafe {
        SetWindowPos(
            hwnd as HWND,
            std::ptr::null_mut(),
            left,
            top,
            0,
            0,
            SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
        );
    }
}

/// 读取鼠标在屏幕上的绝对物理坐标（拖动参考系，与窗口位置无关）
fn cursor_pos() -> Option<(i32, i32)> {
    unsafe {
        let mut pt = POINT { x: 0, y: 0 };
        if GetCursorPos(&mut pt) != 0 {
            Some((pt.x, pt.y))
        } else {
            None
        }
    }
}

/// 读取剪贴板中的文件路径列表（CF_HDROP / 拖放格式）
fn read_clipboard_files() -> Option<Vec<PathBuf>> {
    unsafe {
        if OpenClipboard(std::ptr::null_mut()) == 0 {
            return None;
        }
        let result = (|| {
            let hdrop = GetClipboardData(CF_HDROP);
            if hdrop.is_null() {
                return None;
            }
            let count = DragQueryFileW(hdrop, u32::MAX, std::ptr::null_mut(), 0);
            if count == 0 {
                return Some(Vec::new());
            }
            let mut paths = Vec::with_capacity(count as usize);
            for i in 0..count {
                let len = DragQueryFileW(hdrop, i, std::ptr::null_mut(), 0);
                let mut buf = vec![0u16; (len + 1) as usize];
                DragQueryFileW(hdrop, i, buf.as_mut_ptr(), len + 1);
                let s = String::from_utf16_lossy(&buf[..len as usize]);
                paths.push(PathBuf::from(s));
            }
            Some(paths)
        })();
        CloseClipboard();
        result
    }
}

/// 读取剪贴板内容：文本 > 图片 > 文件路径列表
fn read_clipboard_content() -> Option<ClipContent> {
    let mut clipboard = arboard::Clipboard::new().ok()?;
    if let Ok(text) = clipboard.get_text() {
        return Some(ClipContent::Text(text));
    }
    if let Ok(img) = clipboard.get_image() {
        return Some(ClipContent::Image {
            rgba: img.bytes.into_owned(),
            width: img.width as u32,
            height: img.height as u32,
        });
    }
    if let Some(paths) = read_clipboard_files() {
        if !paths.is_empty() {
            return Some(ClipContent::Files(paths));
        }
    }
    None
}

/// 内容去重指纹：文本/文件用原文，图片用 FNV 哈希
fn content_key(content: &ClipContent) -> String {
    match content {
        ClipContent::Text(t) => t.clone(),
        ClipContent::Image { rgba, width, height } => {
            let mut hash = 0xcbf29ce484222325u64;
            for &b in rgba.iter() {
                hash = (hash ^ b as u64).wrapping_mul(0x100000001b3);
            }
            format!("img:{width}x{height}:{hash:x}")
        }
        ClipContent::Files(paths) => paths
            .iter()
            .map(|p| p.to_string_lossy())
            .collect::<Vec<_>>()
            .join("|"),
    }
}

/// 把图片 RGBA 保存为 PNG，返回 (完整路径, 文件名)
fn save_clipboard_image(rgba: &[u8], width: u32, height: u32) -> Option<(PathBuf, String)> {
    let dir = storage::images_dir();
    let _ = std::fs::create_dir_all(&dir);
    let name = format!(
        "img_{}.png",
        SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_nanos()
    );
    let path = dir.join(&name);
    image::save_buffer(&path, rgba, width, height, image::ExtendedColorType::Rgba8).ok()?;
    Some((path, name))
}

/// 图片文件名 → 完整路径
fn image_full_path(name: &str) -> PathBuf {
    storage::images_dir().join(name)
}

/// 把文本写回系统剪贴板；成功返回 true
fn write_clipboard_text(text: &str) -> bool {
    let mut clipboard = match arboard::Clipboard::new() {
        Ok(cb) => cb,
        Err(_) => return false,
    };
    clipboard.set_text(text.to_string()).is_ok()
}

/// 把图片文件写回系统剪贴板；成功返回 true
fn write_clipboard_image(path: &Path) -> bool {
    let Ok(img) = image::open(path) else {
        return false;
    };
    let rgba = img.to_rgba8();
    let (w, h) = rgba.dimensions();
    let mut clipboard = match arboard::Clipboard::new() {
        Ok(cb) => cb,
        Err(_) => return false,
    };
    clipboard
        .set_image(arboard::ImageData {
            width: w as usize,
            height: h as usize,
            bytes: Cow::Owned(rgba.into_raw()),
        })
        .is_ok()
}

/// 把文件路径列表写回系统剪贴板（CF_HDROP / 拖放格式）；成功返回 true
fn write_clipboard_files(paths: &[PathBuf]) -> bool {
    unsafe {
        // 构造 DROPFILES + 多路径（WCHAR 双空结尾）
        let mut wide = Vec::<u16>::new();
        for p in paths {
            for u in p.to_string_lossy().encode_utf16() {
                wide.push(u);
            }
            wide.push(0);
        }
        wide.push(0); // 列表结束的双空

        let dropfiles_size = std::mem::size_of::<DROPFILES>();
        let total_bytes = dropfiles_size + wide.len() * 2;
        let hglobal = GlobalAlloc(GMEM_MOVEABLE, total_bytes);
        if hglobal.is_null() {
            return false;
        }
        let ptr = GlobalLock(hglobal) as *mut u8;
        if ptr.is_null() {
            GlobalFree(hglobal);
            return false;
        }
        let dropfiles = ptr as *mut DROPFILES;
        (*dropfiles).pFiles = dropfiles_size as u32;
        (*dropfiles).pt = POINT { x: 0, y: 0 };
        (*dropfiles).fNC = 0;
        (*dropfiles).fWide = 1;
        std::ptr::copy_nonoverlapping(
            wide.as_ptr(),
            ptr.add(dropfiles_size) as *mut u16,
            wide.len(),
        );
        GlobalUnlock(hglobal);

        if OpenClipboard(std::ptr::null_mut()) == 0 {
            GlobalFree(hglobal);
            return false;
        }
        let ok = EmptyClipboard() != 0 && !SetClipboardData(CF_HDROP, hglobal).is_null();
        CloseClipboard();
        if !ok {
            GlobalFree(hglobal);
        }
        ok
    }
}

/// 状态栏预览：长文本截断为 limit 字符（含省略号）
fn preview(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_string();
    }
    let head: String = text.chars().take(limit - 1).collect();
    format!("{head}…")
}

/// 置顶块末尾的下标：即第一个非置顶条目的位置（新条目插在这里）
fn first_unpinned_index(model: &VecModel<Entry>) -> usize {
    let mut i = 0;
    while i < model.row_count() && model.row_data(i).is_some_and(|e| e.pinned) {
        i += 1;
    }
    i
}

fn main() -> Result<(), slint::PlatformError> {
    let ui = AppWindow::new()?;

    // 历史列表模型：Rust 侧持有，Slint 侧实时反映
    let model = Rc::new(VecModel::<Entry>::default());

    // 启动时加载持久化的历史记录（最多 MAX_ENTRIES 条，最新在前）
    {
        let history = storage::load_history();
        for entry in history.iter().take(MAX_ENTRIES) {
            match entry.kind.as_str() {
                "image" => {
                    if let Some(name) = &entry.image {
                        let path = image_full_path(name);
                        if let Ok(img) = Image::load_from_path(&path) {
                            model.push(Entry {
                                text: name.clone().into(),
                                kind: 1,
                                pinned: entry.pinned,
                                image: img,
                                key: format!("img:{name}").into(),
                            });
                        }
                    }
                }
                "file" => {
                    model.push(Entry {
                        text: entry.text.clone().into(),
                        kind: 2,
                        pinned: entry.pinned,
                        image: Default::default(),
                        key: entry.text.clone().into(),
                    });
                }
                _ => {
                    model.push(Entry {
                        text: entry.text.clone().into(),
                        kind: 0,
                        pinned: entry.pinned,
                        image: Default::default(),
                        key: entry.text.clone().into(),
                    });
                }
            }
        }
    }
    ui.set_clipboard_items(ModelRc::from(model.clone()));

    fn update_status(ui: &AppWindow, model: &dyn Model<Data = Entry>) {
        let count = model.row_count();
        ui.set_status_text(format!("{count} 条记录").into());
    }
    update_status(&ui, &*model);

    // 把模型内容按显示顺序收集成 HistoryEntry 列表，用于持久化
    fn collect_entries(model: &Rc<VecModel<Entry>>) -> Vec<storage::HistoryEntry> {
        let mut entries = Vec::with_capacity(model.row_count());
        for i in 0..model.row_count() {
            if let Some(e) = model.row_data(i) {
                let kind = match e.kind {
                    1 => "image",
                    2 => "file",
                    _ => "text",
                };
                let image = if e.kind == 1 {
                    Path::new(e.text.as_str())
                        .file_name()
                        .map(|s| s.to_string_lossy().into_owned())
                } else {
                    None
                };
                entries.push(storage::HistoryEntry {
                    kind: kind.to_string(),
                    text: e.text.to_string(),
                    pinned: e.pinned,
                    image,
                });
            }
        }
        entries
    }

    // 新增一条记录：按指纹去重 + 置顶块下方插入 + 上限裁剪 + 持久化
    fn push_entry(model: &Rc<VecModel<Entry>>, kind: i32, text: String, image: Image, key: String) {
        if key.trim().is_empty() {
            return;
        }
        // 去重：与最新一条指纹相同则忽略（轮询最常见的重复源）
        let newest = first_unpinned_index(model);
        if newest < model.row_count()
            && model.row_data(newest).is_some_and(|e| e.key.as_str() == key)
        {
            return;
        }
        // 去重：历史中已存在则先移除旧位置（保留其置顶状态），再重插
        let mut existed_pinned = false;
        for i in 0..model.row_count() {
            if model.row_data(i).is_some_and(|e| e.key.as_str() == key) {
                existed_pinned = model.row_data(i).is_some_and(|e| e.pinned);
                let _ = model.remove(i);
                break;
            }
        }
        let insert_at = first_unpinned_index(model);
        model.insert(
            insert_at,
            Entry {
                text: text.into(),
                kind,
                pinned: existed_pinned,
                image,
                key: key.into(),
            },
        );
        while model.row_count() > MAX_ENTRIES {
            let _ = model.remove(MAX_ENTRIES);
        }
        storage::save_history(&collect_entries(model));
    }

    // 把读到的剪贴板内容入列（三类统一入口）
    fn ingest_content(model: &Rc<VecModel<Entry>>, content: ClipContent) {
        let key = content_key(&content);
        match content {
            ClipContent::Text(t) => {
                push_entry(model, 0, t.clone(), Default::default(), t);
            }
            ClipContent::Image { rgba, width, height } => {
                if let Some((path, name)) = save_clipboard_image(&rgba, width, height) {
                    if let Ok(img) = Image::load_from_path(&path) {
                        push_entry(model, 1, name, img, key);
                    }
                }
            }
            ClipContent::Files(paths) => {
                let joined = paths
                    .iter()
                    .map(|p| p.to_string_lossy().into_owned())
                    .collect::<Vec<_>>()
                    .join("|");
                push_entry(model, 2, joined.clone(), Default::default(), joined);
            }
        }
    }

    // —— 定时轮询：每 500ms 检查剪贴板变化，自动追加历史 ——
    let poll_model = model.clone();
    let poll_ui = ui.as_weak();
    let last_seen = Rc::new(RefCell::new(String::new()));
    let last_seen_timer = last_seen.clone();

    let timer = Timer::default();
    timer.start(TimerMode::Repeated, Duration::from_millis(500), move || {
        let Some(content) = read_clipboard_content() else { return };
        let key = content_key(&content);
        if *last_seen_timer.borrow() == key {
            return; // 内容没变，跳过
        }
        *last_seen_timer.borrow_mut() = key;
        ingest_content(&poll_model, content);
        // 窗口可能已关闭/隐藏，Weak 升级失败则跳过
        if let Some(ui) = poll_ui.upgrade() {
            update_status(&ui, &*poll_model);
        }
    });
    let _timer = timer; // 保持定时器存活到窗口关闭

    // —— Win32 句柄探测：窗口在 run() 后才创建，这里周期尝试直到找到并缓存 ——
    let handle_timer = Timer::default();
    handle_timer.start(TimerMode::Repeated, Duration::from_millis(300), move || {
        let _ = find_window_hwnd();
    });
    let _handle_timer = handle_timer;

    // —— 全局快捷键：Ctrl+Alt+V 切换显示/隐藏（避开系统 Win+V 剪贴板历史） ——
    let hotkey_ui = ui.as_weak();
    GlobalHotKeyEvent::set_event_handler(Some(move |event: GlobalHotKeyEvent| {
        if event.state() == HotKeyState::Pressed {
            let weak = hotkey_ui.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = weak.upgrade() {
                    let Some(hwnd) = find_window_hwnd() else { return };
                    if is_window_visible(hwnd) {
                        // 可见 → 隐藏到托盘
                        hide_window(hwnd);
                        tray::show_icon();
                        ui.set_status_text("已隐藏到托盘（Ctrl+Alt+V 唤回）".into());
                    } else {
                        // 隐藏 → 唤回前台
                        show_window(hwnd);
                        tray::hide_icon();
                        ui.set_status_text("已唤起窗口（Ctrl+Alt+V 隐藏）".into());
                    }
                }
            });
        }
    }));
    let hotkey_manager = GlobalHotKeyManager::new().expect("全局快捷键初始化失败");
    let hotkey = HotKey::new(Some(Modifiers::CONTROL | Modifiers::ALT), Code::KeyV);
    hotkey_manager
        .register(hotkey)
        .expect("注册快捷键失败（可能已被其他程序占用）");
    let _hotkey_manager = hotkey_manager; // 保持存活直到程序退出

    // —— 系统托盘：隐藏时驻留，双击/右键菜单唤回或退出 ——
    let tray_ui = Arc::new(Mutex::new(ui.as_weak()));
    let tray_restore_ui = tray_ui.clone();
    tray::spawn(
        move || {
            let weak = tray_restore_ui.lock().unwrap().clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = weak.upgrade() {
                    if let Some(hwnd) = find_window_hwnd() {
                        show_window(hwnd);
                    }
                    tray::hide_icon();
                    ui.set_status_text("已从托盘恢复".into());
                }
            });
        },
        move || {
            let _ = slint::invoke_from_event_loop(move || {
                let _ = slint::quit_event_loop();
            });
        },
    );

    // —— 「刷新」按钮：手动读取当前剪贴板 ——
    let refresh_model = model.clone();
    let refresh_ui = ui.as_weak();
    ui.on_refresh_clicked(move || {
        let Some(ui) = refresh_ui.upgrade() else { return };
        match read_clipboard_content() {
            Some(content) => {
                ingest_content(&refresh_model, content);
                update_status(&ui, &*refresh_model);
                ui.set_status_text(format!("已刷新，共 {} 条记录", refresh_model.row_count()).into());
            }
            None => {
                ui.set_status_text("剪贴板中没有可识别的内容".into());
            }
        }
    });

    // —— 「清空」按钮：清空历史列表（不影响系统剪贴板） ——
    let clear_model = model.clone();
    let clear_ui = ui.as_weak();
    ui.on_clear_clicked(move || {
        let Some(ui) = clear_ui.upgrade() else { return };
        clear_model.clear();
        storage::save_history(&[]);
        update_status(&ui, &*clear_model);
        ui.set_status_text("历史已清空".into());
    });

    // —— 点击条目：按类型把内容重新写回系统剪贴板 ——
    let copy_model = model.clone();
    let copy_ui = ui.as_weak();
    ui.on_entry_clicked(move |index| {
        let Some(ui) = copy_ui.upgrade() else { return };
        // ListView 的索引不会为负，这里安全转换
        let Some(entry) = copy_model.row_data(index.max(0) as usize) else { return };
        let ok = match entry.kind {
            1 => write_clipboard_image(&image_full_path(entry.text.as_str())),
            2 => {
                let paths: Vec<PathBuf> =
                    entry.text.split('|').map(PathBuf::from).collect();
                write_clipboard_files(&paths)
            }
            _ => write_clipboard_text(entry.text.as_str()),
        };
        if ok {
            let name = match entry.kind {
                1 => "图片".to_string(),
                2 => format!("文件：{}", preview(entry.text.as_str(), 24)),
                _ => preview(entry.text.as_str(), 30),
            };
            ui.set_status_text(format!("已复制：{name}").into());
        } else {
            ui.set_status_text("复制失败：无法访问剪贴板".into());
        }
    });

    // —— 置顶 / 取消置顶（条目级） ——
    let pin_model = model.clone();
    let pin_ui = ui.as_weak();
    ui.on_entry_pin_clicked(move |index| {
        let Some(ui) = pin_ui.upgrade() else { return };
        let i = index.max(0) as usize;
        let Some(entry) = pin_model.row_data(i) else { return };
        if entry.pinned {
            // 取消置顶：原地变换标记，位置不动
            let _ = pin_model.set_row_data(
                i,
                Entry {
                    text: entry.text.clone(),
                    kind: entry.kind,
                    pinned: false,
                    image: entry.image.clone(),
                    key: entry.key.clone(),
                },
            );
            ui.set_status_text(format!("已取消置顶：{}", preview(entry.text.as_str(), 20)).into());
        } else {
            // 置顶：先移除，再插到置顶块末尾
            let _ = pin_model.remove(i);
            let pin_end = first_unpinned_index(&pin_model);
            pin_model.insert(
                pin_end,
                Entry {
                    text: entry.text.clone(),
                    kind: entry.kind,
                    pinned: true,
                    image: entry.image.clone(),
                    key: entry.key.clone(),
                },
            );
            ui.set_status_text(format!("已置顶：{}", preview(entry.text.as_str(), 20)).into());
        }
        storage::save_history(&collect_entries(&pin_model));
        update_status(&ui, &*pin_model);
    });

    // —— 删除单条 ——
    let delete_model = model.clone();
    let delete_ui = ui.as_weak();
    ui.on_entry_delete_clicked(move |index| {
        let Some(ui) = delete_ui.upgrade() else { return };
        let i = index.max(0) as usize;
        if i < delete_model.row_count() {
            let _ = delete_model.remove(i);
        }
        storage::save_history(&collect_entries(&delete_model));
        update_status(&ui, &*delete_model);
        ui.set_status_text("已删除该条".into());
    });

    // —— 标题栏：拖动移动窗口 ——
    // 锚定法 + 屏幕绝对坐标（GetCursorPos）：鼠标位移与窗口位置相互独立，
    // 彻底消除“相对坐标随窗口移动回缩”导致的抖动和延迟
    // 拖动锚点：Some((鼠标屏幕x, 鼠标屏幕y, 窗口起点left, 窗口起点top))
    let drag_state: Rc<RefCell<Option<(i32, i32, i32, i32)>>> = Rc::new(RefCell::new(None));

    let drag_state_begin = drag_state.clone();
    ui.on_drag_begin(move |_mx, _my| {
        let Some(hwnd) = find_window_hwnd() else { return };
        let Some((cx, cy)) = cursor_pos() else { return };
        if let Some((left, top)) = window_pos(hwnd) {
            *drag_state_begin.borrow_mut() = Some((cx, cy, left, top));
        }
    });

    let drag_state_move = drag_state.clone();
    ui.on_drag_move(move |_mx, _my| {
        let Some(hwnd) = find_window_hwnd() else { return };
        let Some((sx, sy, left0, top0)) = *drag_state_move.borrow() else { return };
        let Some((cx, cy)) = cursor_pos() else { return };
        set_window_pos(hwnd, left0 + (cx - sx), top0 + (cy - sy));
    });

    let drag_state_end = drag_state.clone();
    ui.on_drag_end(move || {
        *drag_state_end.borrow_mut() = None;
    });

    // —— 标题栏：隐藏到系统托盘（后台轮询继续，托盘/快捷键唤回） ——
    ui.on_hide_window(move || {
        if let Some(hwnd) = find_window_hwnd() {
            hide_window(hwnd);
            tray::show_icon();
        }
    });

    // —— 标题栏：关闭程序（清理托盘图标后退出） ——
    ui.on_close_window(|| {
        tray::shutdown();
        let _ = slint::quit_event_loop();
    });

    // —— 标题栏：窗口置顶开关（Slint 内置 always-on-top 属性） ——
    let win_pin_ui = ui.as_weak();
    ui.on_toggle_pin(move || {
        let Some(ui) = win_pin_ui.upgrade() else { return };
        let new_state = !ui.get_topmost();
        ui.set_topmost(new_state);
        ui.set_status_text(if new_state { "窗口已置顶".into() } else { "窗口已取消置顶".into() });
    });

    // 启动时读取一次当前剪贴板
    if let Some(content) = read_clipboard_content() {
        let key = content_key(&content);
        *last_seen.borrow_mut() = key;
        ingest_content(&model, content);
        update_status(&ui, &*model);
    }

    ui.run()
}