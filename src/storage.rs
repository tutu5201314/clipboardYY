use std::path::PathBuf;

/// 历史记录文件路径：系统应用数据目录下的 clipboard-yy/history.json
/// Windows 上即 %APPDATA%\clipboard-yy\history.json
fn history_file_path() -> PathBuf {
    let base = dirs::config_dir().unwrap_or_else(|| PathBuf::from("."));
    base.join("clipboard-yy").join("history.json")
}

/// 启动时加载历史记录；文件缺失或损坏时返回空列表
pub fn load_history() -> Vec<String> {
    let path = history_file_path();
    let Ok(content) = std::fs::read_to_string(&path) else {
        return Vec::new();
    };
    serde_json::from_str(&content).unwrap_or_default()
}

/// 持久化历史记录（按显示顺序：最新在前）
pub fn save_history(entries: &[String]) {
    let path = history_file_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    match serde_json::to_string(entries) {
        Ok(json) => {
            if let Err(e) = std::fs::write(&path, json) {
                eprintln!("保存历史失败: {e}");
            }
        }
        Err(e) => eprintln!("序列化历史失败: {e}"),
    }
}