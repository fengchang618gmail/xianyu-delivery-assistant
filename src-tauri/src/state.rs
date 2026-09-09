//! 应用共享状态：模式、暂停、扩展连接、商品缓存与待批准令牌。

use std::collections::HashMap;
use std::sync::Mutex;

use serde::Serialize;

use crate::db::{self, Db};
use crate::feishu::ProductEntry;
use crate::nm::NmHandle;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    Confirm,
    Auto,
}

impl Mode {
    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Confirm => "confirm",
            Mode::Auto => "auto",
        }
    }

    pub fn from_str(s: &str) -> Option<Mode> {
        match s {
            "auto" => Some(Mode::Auto),
            "confirm" | "" => Some(Mode::Confirm),
            _ => None,
        }
    }
}

pub struct AppState {
    pub db: Db,
    pub http: reqwest::Client,
    pub mode: Mutex<Mode>,
    pub paused: Mutex<bool>,
    pub nm: NmHandle,
    /// 应用数据目录（令牌等本机文件存放处）。
    pub data_dir: std::path::PathBuf,
    /// 发货内容摘要盐（每台安装随机生成）。
    pub salt: String,
    /// 商品缓存 (抓取时间, 行)。TTL 内不重复请求飞书。
    pub products: tokio::sync::Mutex<Option<(std::time::Instant, Vec<ProductEntry>)>>,
    /// 一次性批准令牌：task_id -> token。
    pub pending_approvals: Mutex<HashMap<String, String>>,
}

impl AppState {
    pub fn mode_now(&self) -> Mode {
        *self.mode.lock().expect("mode 锁")
    }

    pub fn paused_now(&self) -> bool {
        *self.paused.lock().expect("paused 锁")
    }

    pub fn set_mode(&self, mode: Mode) {
        *self.mode.lock().expect("mode 锁") = mode;
    }

    pub fn set_paused(&self, paused: bool) {
        *self.paused.lock().expect("paused 锁") = paused;
    }
}

/// 在数据库锁内执行同步操作（不要跨 await 持有连接）。
pub fn with_db<T>(state: &AppState, f: impl FnOnce(&rusqlite::Connection) -> Result<T, String>) -> Result<T, String> {
    let conn = state.db.0.lock().expect("db 锁");
    f(&conn)
}

/// 读取飞书应用凭据（App ID 来自配置，Secret 来自凭据管理器）。
pub fn feishu_app_credentials(state: &AppState) -> Result<(String, String), String> {
    let app_id = with_db(state, |conn| Ok(db::config_get(conn, "feishu_app_id")))?
        .filter(|s| !s.is_empty())
        .ok_or("尚未配置飞书 App ID")?;
    let app_secret = crate::secrets::Secret::FeishuAppSecret
        .load()?
        .filter(|s| !s.is_empty())
        .ok_or("尚未配置飞书 App Secret")?;
    Ok((app_id, app_secret))
}

/// 读取已选定的多维表格 (token, table_id, is_wiki)。
/// is_wiki=true 时 token 是知识库节点，需先用 feishu::resolve_app_token 换取 app_token。
pub fn bitable_selection(state: &AppState) -> Result<(String, String, bool), String> {
    let url = with_db(state, |conn| Ok(db::config_get(conn, "feishu_bitable_url")))?
        .filter(|s| !s.is_empty())
        .ok_or("尚未选择飞书多维表格")?;
    let (token, table_from_url, is_wiki) = crate::feishu::parse_bitable_url(&url)?;
    let table_id = table_from_url
        .or_else(|| with_db(state, |conn| Ok(db::config_get(conn, "feishu_table_id"))).ok().flatten())
        .filter(|t| !t.is_empty())
        .ok_or("尚未指定数据表")?;
    Ok((token, table_id, is_wiki))
}
