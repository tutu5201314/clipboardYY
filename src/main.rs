slint::include_modules!();

fn main() -> Result<(), slint::PlatformError> {
    let ui = AppWindow::new()?;

    // 示例数据：后续接入真实剪贴板读取
    let items: Vec<slint::SharedString> = vec![
        "示例条目 1：欢迎使用剪贴板工具".into(),
        "示例条目 2：这里将显示历史复制内容".into(),
        "示例条目 3（文本 / 图片 / 文件）".into(),
    ];
    let model = std::rc::Rc::new(slint::VecModel::from(items));
    ui.set_clipboard_items(model.into());

    ui.run()
}