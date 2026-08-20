use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use slint::{Model, ModelRc, SharedString, Timer, TimerMode, VecModel};

slint::include_modules!();

/// 历史记录上限，超出后丢弃最旧的
const MAX_ENTRIES: usize = 50;

/// 读取系统剪贴板中的纯文本；非文本内容（图片/文件）返回 None
fn read_clipboard_text() -> Option<String> {
    let mut clipboard = arboard::Clipboard::new().ok()?;
    clipboard.get_text().ok()
}

fn main() -> Result<(), slint::PlatformError> {
    let ui = AppWindow::new()?;

    // 历史列表模型：Rust 侧持有，Slint 侧实时反映
    let model = Rc::new(VecModel::<SharedString>::default());
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
        update_status(&ui, &*clear_model);
        ui.set_status_text("历史已清空".into());
    });

    // 启动时读取一次当前剪贴板
    if let Some(text) = read_clipboard_text() {
        *last_seen.borrow_mut() = text.clone();
        push_entry(&model, &text);
        update_status(&ui, &*model);
    }

    ui.run()
}