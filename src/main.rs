use std::cell::RefCell;
use std::rc::Rc;
use std::sync::OnceLock;
use std::time::Duration;

use global_hotkey::hotkey::{Code, HotKey, Modifiers};
use global_hotkey::{GlobalHotKeyEvent, GlobalHotKeyManager, HotKeyState};
use slint::{LogicalPosition, Model, ModelRc, Timer, TimerMode, VecModel};
use windows_sys::Win32::Foundation::HWND;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    FindWindowW, SetForegroundWindow, ShowWindow, SW_RESTORE,
};

mod storage;

slint::include_modules!();

/// 历史记录上限，超出后丢弃最旧的
const MAX_ENTRIES: usize = 50;

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

/// 置顶 / 取消置顶交由 Slint 内置的 Window.always-on-top 属性处理，
/// 这里不再需要 Win32 手动调用。

/// 恢复窗口并置为前台
fn bring_window_to_front(hwnd: isize) {
    unsafe {
        ShowWindow(hwnd as HWND, SW_RESTORE);
        SetForegroundWindow(hwnd as HWND);
    }
}

/// 读取系统剪贴板中的纯文本；非文本内容（图片/文件）返回 None
fn read_clipboard_text() -> Option<String> {
    let mut clipboard = arboard::Clipboard::new().ok()?;
    clipboard.get_text().ok()
}

/// 把文本写回系统剪贴板；成功返回 true
fn write_clipboard_text(text: &str) -> bool {
    let mut clipboard = match arboard::Clipboard::new() {
        Ok(cb) => cb,
        Err(_) => return false,
    };
    clipboard.set_text(text.to_string()).is_ok()
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
            model.push(Entry {
                text: entry.text.clone().into(),
                pinned: entry.pinned,
            });
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
                entries.push(storage::HistoryEntry {
                    text: e.text.to_string(),
                    pinned: e.pinned,
                });
            }
        }
        entries
    }

    // 新增一条记录：去重 + 置顶块下方插入 + 上限裁剪 + 持久化
    fn push_entry(model: &Rc<VecModel<Entry>>, text: &str) {
        if text.trim().is_empty() {
            return;
        }
        // 去重：与置顶块后最新的第一条相同则忽略（轮询最常见的重复源）
        let newest = first_unpinned_index(model);
        if newest < model.row_count()
            && model.row_data(newest).is_some_and(|e| e.text.as_str() == text)
        {
            return;
        }
        // 去重：历史中已存在则先移除旧位置（保留其置顶状态），再重插
        let mut existed_pinned = false;
        for i in 0..model.row_count() {
            if model.row_data(i).is_some_and(|e| e.text.as_str() == text) {
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
                pinned: existed_pinned,
            },
        );
        while model.row_count() > MAX_ENTRIES {
            let _ = model.remove(MAX_ENTRIES);
        }
        storage::save_history(&collect_entries(model));
    }

    // —— 定时轮询：每 500ms 检查剪贴板变化，自动追加历史 ——
    let poll_model = model.clone();
    let poll_ui = ui.as_weak();
    let last_seen = Rc::new(RefCell::new(String::new()));
    let last_seen_timer = last_seen.clone();

    let timer = Timer::default();
    timer.start(TimerMode::Repeated, Duration::from_millis(500), move || {
        let Some(text) = read_clipboard_text() else { return };
        if *last_seen_timer.borrow() == text {
            return; // 内容没变，跳过
        }
        *last_seen_timer.borrow_mut() = text.clone();
        push_entry(&poll_model, &text);
        // 窗口可能已关闭，Weak 升级失败则跳过
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

    // —— 全局快捷键：Ctrl+Alt+V 唤起窗口（避开系统 Win+V 剪贴板历史） ——
    let hotkey_ui = ui.as_weak();
    GlobalHotKeyEvent::set_event_handler(Some(move |event: GlobalHotKeyEvent| {
        if event.state() == HotKeyState::Pressed {
            let weak = hotkey_ui.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = weak.upgrade() {
                    let _ = ui.window().show();
                    if let Some(hwnd) = find_window_hwnd() {
                        bring_window_to_front(hwnd);
                    }
                    ui.set_status_text("已唤起窗口（Ctrl+Alt+V）".into());
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

    // —— 「刷新」按钮：手动读取当前剪贴板 ——
    let refresh_model = model.clone();
    let refresh_ui = ui.as_weak();
    ui.on_refresh_clicked(move || {
        let Some(ui) = refresh_ui.upgrade() else { return };
        match read_clipboard_text() {
            Some(text) => {
                push_entry(&refresh_model, &text);
                update_status(&ui, &*refresh_model);
                ui.set_status_text(format!("已刷新，共 {} 条记录", refresh_model.row_count()).into());
            }
            None => {
                ui.set_status_text("剪贴板中没有文本内容".into());
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

    // —— 点击条目：把该条内容重新写回系统剪贴板 ——
    let copy_model = model.clone();
    let copy_ui = ui.as_weak();
    ui.on_entry_clicked(move |index| {
        let Some(ui) = copy_ui.upgrade() else { return };
        // ListView 的索引不会为负，这里安全转换
        let Some(entry) = copy_model.row_data(index.max(0) as usize) else { return };
        if write_clipboard_text(entry.text.as_str()) {
            ui.set_status_text(format!("已复制：{}", preview(entry.text.as_str(), 30)).into());
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
                    pinned: false,
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
                    pinned: true,
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
    let drag_ui = ui.as_weak();
    ui.on_drag_move(move |dx, dy| {
        if let Some(ui) = drag_ui.upgrade() {
            let win = ui.window();
            let pos = win.position();
            win.set_position(LogicalPosition::new(pos.x as f32 + dx, pos.y as f32 + dy));
        }
    });

    // —— 标题栏：隐藏窗口（历史轮询继续，快捷键可唤回） ——
    let hide_ui = ui.as_weak();
    ui.on_hide_window(move || {
        if let Some(ui) = hide_ui.upgrade() {
            let _ = ui.window().hide();
        }
    });

    // —— 标题栏：关闭程序 ——
    ui.on_close_window(|| {
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
    if let Some(text) = read_clipboard_text() {
        *last_seen.borrow_mut() = text.clone();
        push_entry(&model, &text);
        update_status(&ui, &*model);
    }

    ui.run()
}