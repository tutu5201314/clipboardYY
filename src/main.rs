use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use slint::{Model, ModelRc, SharedString, Timer, TimerMode, VecModel};

mod storage;

slint::include_modules!();

/// 历史记录上限，超出后丢弃最旧的
const MAX_ENTRIES: usize = 50;

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

/// 状态栏预览：长文本截断为 30 字符，保留首尾
fn preview(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_string();
    }
    let head: String = text.chars().take(limit - 1).collect();
    format!("{head}…")
}

fn main() -> Result<(), slint::PlatformError> {
    let ui = AppWindow::new()?;

    // 历史列表模型：Rust 侧持有，Slint 侧实时反映
    let model = Rc::new(VecModel::<SharedString>::default());

    // 启动时加载持久化的历史记录（最多 MAX_ENTRIES 条，最新在前）
    {
        let history = storage::load_history();
        for text in history.iter().take(MAX_ENTRIES) {
            model.push(text.clone().into());
        }
    }
    ui.set_clipboard_items(ModelRc::from(model.clone()));

    fn update_status(ui: &AppWindow, model: &dyn Model<Data = SharedString>) {
        let count = model.row_count();
        ui.set_status_text(format!("{count} 条记录").into());
    }
    update_status(&ui, &*model);

    // 新增一条记录：顶部插入 + 去重 + 上限裁剪
    fn push_entry(model: &Rc<VecModel<SharedString>>, text: &str) {
        if text.trim().is_empty() {
            return;
        }
        // 去重：与顶部最新一条相同则忽略（轮询场景最常见的重复源）
        if let Some(top) = model.row_data(0) {
            if top.as_str() == text {
                return;
            }
        }
        // 去重：历史中已存在则先移除旧位置，再置顶
        for i in 0..model.row_count() {
            if let Some(existing) = model.row_data(i) {
                if existing.as_str() == text {
                    let _ = model.remove(i);
                    break;
                }
            }
        }
        model.insert(0, text.into());
        while model.row_count() > MAX_ENTRIES {
            let _ = model.remove(MAX_ENTRIES);
        }
        storage::save_history(&collect_entries(model));
    }

    /// 把模型内容按显示顺序（最新在前）收集成 Vec<String>，用于持久化
    fn collect_entries(model: &Rc<VecModel<SharedString>>) -> Vec<String> {
        let mut entries = Vec::with_capacity(model.row_count());
        for i in 0..model.row_count() {
            if let Some(item) = model.row_data(i) {
                entries.push(item.to_string());
            }
        }
        entries
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
        let Some(item) = copy_model.row_data(index.max(0) as usize) else { return };
        if write_clipboard_text(item.as_str()) {
            ui.set_status_text(format!("已复制：{}", preview(item.as_str(), 30)).into());
        } else {
            ui.set_status_text("复制失败：无法访问剪贴板".into());
        }
    });

    // 启动时读取一次当前剪贴板
    if let Some(text) = read_clipboard_text() {
        *last_seen.borrow_mut() = text.clone();
        push_entry(&model, &text);
        update_status(&ui, &*model);
    }

    ui.run()
}