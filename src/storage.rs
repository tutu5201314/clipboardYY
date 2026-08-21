use std::path::PathBuf;

use serde::{Deserialize, Serialize};

fn default_kind() -> String {
    "text".to_string()
}

/// 一条历史记录
/// - kind: "text" | "image" | "file"
/// - text: 文本内容 / 文件路径（用 | 分隔）/ 图片条目的完整保存路径
/// - image: 图片文件名（images/ 目录下），仅 kind=image 时使用
#[derive(Serialize, Deserialize, Clone)]
pub struct HistoryEntry {
    #[serde(default = "default_kind")]
    pub kind: String,
    pub text: String,
    #[serde(default)]
    pub pinned: bool,
    #[serde(default)]
    pub image: Option<String>,
}

fn data_dir() -> PathBuf {
    let base = dirs::config_dir().unwrap_or_else(|| PathBuf::from("."));
    base.join("clipboard-yy")
}

/// 历史记录文件路径
/// Windows 上即 %APPDATA%\clipboard-yy\history.json
fn history_file_path() -> PathBuf {
    data_dir().join("history.json")
}

/// 图片保存目录（自动创建）
pub fn images_dir() -> PathBuf {
    data_dir().join("images")
}

/// 启动时加载历史记录；兼容三种格式：
/// - 最新 `[{kind, text, pinned, image}]`
/// - 旧对象格式 `[{text, pinned}]`（自动视为文本条目）
/// - 最旧字符串数组 `["str", ...]`（自动迁移为文本条目）
pub fn load_history() -> Vec<HistoryEntry> {
    let path = history_file_path();
    let Ok(content) = std::fs::read_to_string(&path) else {
        return Vec::new();
    };
    if let Ok(entries) = serde_json::from_str::<Vec<HistoryEntry>>(&content) {
        return entries;
    }
    if let Ok(texts) = serde_json::from_str::<Vec<String>>(&content) {
        return texts
            .into_iter()
            .map(|text| HistoryEntry {
                kind: "text".to_string(),
                text,
                pinned: false,
                image: None,
            })
            .collect();
    }
    Vec::new()
}

/// 持久化历史记录（按显示顺序：最新在前，置顶集中在前段）
pub fn save_history(entries: &[HistoryEntry]) {
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