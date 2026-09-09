//! 本地 SQLite：配置、商品缓存、发货任务（幂等去重）与脱敏审计。

use std::path::Path;
use std::sync::Mutex;

use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;

use crate::redact;

pub struct Db(pub Mutex<Connection>);

#[derive(Debug, Clone, Serialize)]
pub struct DeliveryTask {
    pub id: String,
    pub idempotency_key: String,
    pub order_or_conversation_id: String,
    pub product_id: Option<String>,
    pub product_title: Option<String>,
    /// 仅未终结任务临时持有明文；终结时置空，长期只留摘要。
    pub delivery_content: Option<String>,
    pub delivery_content_hash: Option<String>,
    pub mode: String,
    pub status: String,
    pub fail_reason: Option<String>,
    pub created_at: String,
    pub completed_at: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct AuditEvent {
    pub id: i64,
    pub task_id: Option<String>,
    pub event_type: String,
    pub details_redacted: String,
    pub created_at: String,
}

pub fn open(path: &Path) -> Result<Connection, String> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let conn = Connection::open(path).map_err(|e| e.to_string())?;
    conn.execute_batch("PRAGMA journal_mode = WAL; PRAGMA foreign_keys = ON;")
        .map_err(|e| e.to_string())?;
    migrate(&conn)?;
    Ok(conn)
}

fn migrate(conn: &Connection) -> Result<(), String> {
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS config (
            key TEXT PRIMARY KEY,
            value TEXT NOT NULL,
            updated_at TEXT NOT NULL
        );
        CREATE TABLE IF NOT EXISTS product_cache (
            cache_key TEXT PRIMARY KEY,
            product_id TEXT,
            product_title_normalized TEXT,
            delivery_content_hash TEXT,
            refreshed_at TEXT NOT NULL
        );
        CREATE TABLE IF NOT EXISTS delivery_task (
            id TEXT PRIMARY KEY,
            idempotency_key TEXT NOT NULL UNIQUE,
            order_or_conversation_id TEXT NOT NULL,
            product_id TEXT,
            product_title TEXT,
            delivery_content TEXT,
            delivery_content_hash TEXT,
            mode TEXT NOT NULL,
            status TEXT NOT NULL,
            fail_reason TEXT,
            created_at TEXT NOT NULL,
            completed_at TEXT
        );
        CREATE INDEX IF NOT EXISTS idx_task_order ON delivery_task(order_or_conversation_id);
        CREATE INDEX IF NOT EXISTS idx_task_status ON delivery_task(status);
        CREATE TABLE IF NOT EXISTS audit_event (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            task_id TEXT REFERENCES delivery_task(id),
            event_type TEXT NOT NULL,
            details_redacted TEXT NOT NULL,
            created_at TEXT NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_audit_task ON audit_event(task_id);
        "#,
    )
    .map_err(|e| e.to_string())
}

pub fn now() -> String {
    chrono::Utc::now().to_rfc3339()
}

// ---------- config ----------

pub fn config_get(conn: &Connection, key: &str) -> Option<String> {
    conn.query_row("SELECT value FROM config WHERE key = ?1", params![key], |r| r.get::<_, String>(0))
        .optional()
        .ok()
        .flatten()
}

pub fn config_set(conn: &Connection, key: &str, value: &str) -> Result<(), String> {
    conn.execute(
        "INSERT INTO config(key, value, updated_at) VALUES(?1, ?2, ?3)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
        params![key, value, now()],
    )
    .map_err(|e| e.to_string())?;
    Ok(())
}

pub fn config_del(conn: &Connection, key: &str) -> Result<(), String> {
    conn.execute("DELETE FROM config WHERE key = ?1", params![key])
        .map_err(|e| e.to_string())?;
    Ok(())
}

// ---------- audit ----------

pub fn audit(conn: &Connection, task_id: Option<&str>, event_type: &str, details: &str) -> Result<(), String> {
    conn.execute(
        "INSERT INTO audit_event(task_id, event_type, details_redacted, created_at) VALUES(?1, ?2, ?3, ?4)",
        params![task_id, event_type, redact::redact_text(details), now()],
    )
    .map_err(|e| e.to_string())?;
    Ok(())
}

pub fn audit_list(conn: &Connection, limit: i64) -> Result<Vec<AuditEvent>, String> {
    let mut stmt = conn
        .prepare("SELECT id, task_id, event_type, details_redacted, created_at FROM audit_event ORDER BY id DESC LIMIT ?1")
        .map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map(params![limit], |r| {
            Ok(AuditEvent {
                id: r.get(0)?,
                task_id: r.get(1)?,
                event_type: r.get(2)?,
                details_redacted: r.get(3)?,
                created_at: r.get(4)?,
            })
        })
        .map_err(|e| e.to_string())?;
    rows.collect::<Result<Vec<_>, _>>().map_err(|e| e.to_string())
}

// ---------- product cache ----------

pub struct ProductRow {
    pub product_id: Option<String>,
    pub title_normalized: Option<String>,
    pub content_hash: String,
}

pub fn product_cache_replace_all(conn: &Connection, rows: &[ProductRow]) -> Result<(), String> {
    conn.execute("DELETE FROM product_cache", []).map_err(|e| e.to_string())?;
    for r in rows {
        conn.execute(
            "INSERT INTO product_cache(cache_key, product_id, product_title_normalized, delivery_content_hash, refreshed_at)
             VALUES(?1, ?2, ?3, ?4, ?5)",
            params![
                format!("{}|{}", r.product_id.as_deref().unwrap_or("-"), r.title_normalized.as_deref().unwrap_or("-")),
                r.product_id,
                r.title_normalized,
                r.content_hash,
                now()
            ],
        )
        .map_err(|e| e.to_string())?;
    }
    Ok(())
}

// ---------- delivery task ----------

const TERMINAL: [&str; 4] = ["已完成", "异常", "已取消", "已忽略"];

pub fn task_get(conn: &Connection, id: &str) -> Result<Option<DeliveryTask>, String> {
    conn.query_row(&(TASK_SQL.to_owned() + " WHERE id = ?1"), params![id], task_from_row)
        .optional()
        .map_err(|e| e.to_string())
}

pub fn task_list_active(conn: &Connection) -> Result<Vec<DeliveryTask>, String> {
    // 匿名占位符必须与绑定参数一一对应；漏绑会导致参数计数不匹配而查询失败。
    let cond = format!("status NOT IN ({})", placeholders(TERMINAL.len()));
    let params = rusqlite::params_from_iter(TERMINAL.iter().map(|s| s.to_string()));
    task_list_where(conn, &cond, params)
}

pub fn task_list_recent(conn: &Connection, limit: i64) -> Result<Vec<DeliveryTask>, String> {
    let mut stmt = conn
        .prepare(&format!("{TASK_SQL} ORDER BY created_at DESC LIMIT ?1"))
        .map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map(params![limit], task_from_row)
        .map_err(|e| e.to_string())?;
    rows.collect::<Result<Vec<_>, _>>().map_err(|e| e.to_string())
}

fn task_list_where(conn: &Connection, cond: &str, params: impl rusqlite::Params) -> Result<Vec<DeliveryTask>, String> {
    let sql = format!("{TASK_SQL} WHERE {cond} ORDER BY created_at DESC");
    let mut stmt = conn.prepare(&sql).map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map(params, task_from_row)
        .map_err(|e| e.to_string())?;
    rows.collect::<Result<Vec<_>, _>>().map_err(|e| e.to_string())
}

const TASK_SQL: &str = "SELECT id, idempotency_key, order_or_conversation_id, product_id, product_title, delivery_content, delivery_content_hash, mode, status, fail_reason, created_at, completed_at FROM delivery_task";

/// 生成 n 个匿名占位符；不要用 `?N` 编号——编号会与查询前部的显式 `?1` 撞号。
fn placeholders(n: usize) -> String {
    std::iter::repeat("?").take(n).collect::<Vec<_>>().join(", ")
}

fn task_from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<DeliveryTask> {
    Ok(DeliveryTask {
        id: r.get(0)?,
        idempotency_key: r.get(1)?,
        order_or_conversation_id: r.get(2)?,
        product_id: r.get(3)?,
        product_title: r.get(4)?,
        delivery_content: r.get(5)?,
        delivery_content_hash: r.get(6)?,
        mode: r.get(7)?,
        status: r.get(8)?,
        fail_reason: r.get(9)?,
        created_at: r.get(10)?,
        completed_at: r.get(11)?,
    })
}

pub enum DedupVerdict {
    /// 已存在未终结任务，直接复用。
    Existing(DeliveryTask),
    /// 同键任务已完成，忽略本次。
    Completed,
    /// 可创建新任务（含失败重试的尝试序号）。
    Create(String),
}

/// 幂等键匹配：既匹配 attempt0（无后缀）也匹配重试产生的 attemptN 后缀。
/// 若只按无后缀 base 匹配，已带 |attemptN 的活跃任务永远不被复用 → 同一订单反复 page_event
/// 会不断生成重复任务（已实测复现）。
/// `n` 为幂等键参数在整条 SQL 中的起始序号（?n 放 base，?n+1 放 pattern）。
fn idem_cond(alias: &str, n: usize) -> String {
    format!("{alias}idempotency_key = ?{n} OR {alias}idempotency_key LIKE ?{} ESCAPE '\\'", n + 1)
}

/// 按“订单/会话标识 + 状态事件 + 内容无关的识别键”做幂等判定。
/// 识别键：订单 ID + 待发货状态 + 商品识别标识（ID 或标准化标题）。
pub fn task_dedup_check(conn: &Connection, order_id: &str, product_key: &str) -> Result<DedupVerdict, String> {
    let base_key = format!("v1|{order_id}|待发货|{product_key}");
    let pattern = format!("{base_key}|attempt%");
    // 去重只防“同一时刻的重复活跃任务”，不做“已完成即永久忽略”：
    // 卖家点过发送后订单状态可能仍是“等待卖家发货”（未点去发货），
    // 页面再次检测到该订单时应当能再建一条待确认，而不是被旧记忆挡住。
    let existing = conn
        .query_row(
            &format!(
                "{TASK_SQL} WHERE ({}) AND status NOT IN ({}) LIMIT 1",
                idem_cond("", 1),
                placeholders(TERMINAL.len())
            ),
            rusqlite::params_from_iter(
                std::iter::once(base_key.clone())
                    .chain(std::iter::once(pattern.clone()))
                    .chain(TERMINAL.iter().map(|s| s.to_string()))
            ),
            task_from_row,
        )
        .optional()
        .map_err(|e| e.to_string())?;
    if let Some(t) = existing {
        return Ok(DedupVerdict::Existing(t));
    }
    // 历史尝试次数
    let attempts: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM delivery_task WHERE idempotency_key LIKE ?1",
            params![format!("{base_key}%")],
            |r| r.get(0),
        )
        .map_err(|e| e.to_string())?;
    let key = if attempts == 0 { base_key } else { format!("{base_key}|attempt{attempts}") };
    Ok(DedupVerdict::Create(key))
}

/// 同一 订单+商品 只保留一个活跃任务：把其余仍处于活跃态（待确认/待自动发送/待发送…）
/// 的同键任务置为已取消。用于修复历史上因去重缺陷堆积出的重复待确认，并保持不变量。
/// keep_id 为要保留的任务；返回被合并的任务数。
pub fn task_collapse_duplicates(conn: &Connection, order_id: &str, product_key: &str, keep_id: &str) -> Result<usize, String> {
    let base_key = format!("v1|{order_id}|待发货|{product_key}");
    let pattern = format!("{base_key}|attempt%");
    // 占位序号：?1=keep_id；status NOT IN 用 ?2..?{1+L}；幂等键 base/pattern 随后（见 idem_cond n）。
    let keep_n = 1;
    let status_start = keep_n + 1; // 2
    let idem_n = status_start + TERMINAL.len(); // base 在此，pattern 在 +1
    let sql = format!(
        "UPDATE delivery_task SET status = '已取消', fail_reason = '重复任务，自动合并' \
         WHERE id <> ?{keep_n} AND status NOT IN ({}) AND ({})",
        placeholders(TERMINAL.len()),
        idem_cond("", idem_n)
    );
    let n = conn
        .execute(
            &sql,
            rusqlite::params_from_iter(
                std::iter::once(keep_id.to_string())
                    .chain(TERMINAL.iter().map(|s| s.to_string()))
                    .chain(std::iter::once(base_key).chain(std::iter::once(pattern)))
            ),
        )
        .map_err(|e| e.to_string())?;
    Ok(n)
}

pub fn task_insert(conn: &Connection, task: &DeliveryTask) -> Result<(), String> {
    conn.execute(
        "INSERT INTO delivery_task(id, idempotency_key, order_or_conversation_id, product_id, product_title, delivery_content, delivery_content_hash, mode, status, fail_reason, created_at, completed_at)
         VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)",
        params![
            task.id,
            task.idempotency_key,
            task.order_or_conversation_id,
            task.product_id,
            task.product_title,
            task.delivery_content,
            task.delivery_content_hash,
            task.mode,
            task.status,
            task.fail_reason,
            task.created_at,
            task.completed_at
        ],
    )
    .map_err(|e| e.to_string())?;
    Ok(())
}

#[derive(Default)]
pub struct TaskUpdate {
    pub status: Option<String>,
    pub fail_reason: Option<Option<String>>,
    pub delivery_content: Option<Option<String>>,
    pub delivery_content_hash: Option<Option<String>>,
    pub product_id: Option<Option<String>>,
    pub product_title: Option<Option<String>>,
    pub mode: Option<String>,
    pub completed_at: Option<Option<String>>,
}

impl TaskUpdate {
    pub fn status(status: &str) -> Self {
        TaskUpdate {
            status: Some(status.into()),
            fail_reason: None,
            delivery_content: None,
            delivery_content_hash: None,
            product_id: None,
            product_title: None,
            mode: None,
            completed_at: None,
        }
    }

    pub fn fail(status: &str, reason: &str) -> Self {
        TaskUpdate {
            status: Some(status.into()),
            fail_reason: Some(Some(reason.into())),
            delivery_content: Some(None), // 终结/异常时清空明文
            delivery_content_hash: None,
            product_id: None,
            product_title: None,
            mode: None,
            completed_at: Some(Some(now())),
        }
    }
}

pub fn task_update(conn: &Connection, id: &str, u: &TaskUpdate) -> Result<(), String> {
    let mut sets = Vec::new();
    let mut values: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
    if let Some(v) = &u.status {
        sets.push("status = ?");
        values.push(Box::new(v.clone()));
    }
    if let Some(v) = &u.fail_reason {
        sets.push("fail_reason = ?");
        values.push(Box::new(v.clone()));
    }
    if let Some(v) = &u.delivery_content {
        sets.push("delivery_content = ?");
        values.push(Box::new(v.clone()));
    }
    if let Some(v) = &u.delivery_content_hash {
        sets.push("delivery_content_hash = ?");
        values.push(Box::new(v.clone()));
    }
    if let Some(v) = &u.product_id {
        sets.push("product_id = ?");
        values.push(Box::new(v.clone()));
    }
    if let Some(v) = &u.product_title {
        sets.push("product_title = ?");
        values.push(Box::new(v.clone()));
    }
    if let Some(v) = &u.mode {
        sets.push("mode = ?");
        values.push(Box::new(v.clone()));
    }
    if let Some(v) = &u.completed_at {
        sets.push("completed_at = ?");
        values.push(Box::new(v.clone()));
    }
    if sets.is_empty() {
        return Ok(());
    }
    // 必须用 ? 占位符绑定 id；此前误写成 WHERE id = <数字>，既少一个占位符又拿字面量比 id，
    // 导致任何 task_update 都抛 “Wrong number of parameters”，approve_send/degrade 静默失败。
    let sql = format!("UPDATE delivery_task SET {} WHERE id = ?", sets.join(", "));
    values.push(Box::new(id.to_string()));
    let refs: Vec<&dyn rusqlite::types::ToSql> = values.iter().map(|b| b.as_ref()).collect();
    conn.execute(&sql, refs.as_slice()).map_err(|e| e.to_string())?;
    Ok(())
}

/// 终结任务：清空明文内容，仅保留摘要与脱敏审计。
pub fn task_finalize(conn: &Connection, id: &str, status: &str, content_hash: Option<&str>, reason: Option<&str>) -> Result<(), String> {
    let terminal = TERMINAL.contains(&status);
    if !terminal {
        return Err(format!("状态 {status} 不是终结态"));
    }
    conn.execute(
        "UPDATE delivery_task SET status = ?1, delivery_content = NULL, delivery_content_hash = COALESCE(?2, delivery_content_hash), fail_reason = ?3, completed_at = ?4 WHERE id = ?5",
        params![status, content_hash, reason, now(), id],
    )
    .map_err(|e| e.to_string())?;
    Ok(())
}

/// 清除全部本机任务与审计记录（不动飞书商品库）。
pub fn wipe_records(conn: &Connection) -> Result<(), String> {
    conn.execute_batch("DELETE FROM audit_event; DELETE FROM delivery_task;")
        .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mem_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        conn
    }

    #[test]
    fn dedup_flow() {
        let conn = mem_db();
        let key = match task_dedup_check(&conn, "ord-1", "pid-9").unwrap() {
            DedupVerdict::Create(k) => k,
            _ => panic!("expected create"),
        };
        let task = DeliveryTask {
            id: "t1".into(),
            idempotency_key: key,
            order_or_conversation_id: "ord-1".into(),
            product_id: Some("pid-9".into()),
            product_title: None,
            delivery_content: Some("secret".into()),
            delivery_content_hash: None,
            mode: "confirm".into(),
            status: "待确认".into(),
            fail_reason: None,
            created_at: now(),
            completed_at: None,
        };
        task_insert(&conn, &task).unwrap();
        assert!(matches!(task_dedup_check(&conn, "ord-1", "pid-9").unwrap(), DedupVerdict::Existing(_)));
        task_finalize(&conn, "t1", "已完成", Some("h"), None).unwrap();
        let done = task_get(&conn, "t1").unwrap().unwrap();
        assert!(done.delivery_content.is_none(), "明文必须被清除");
        assert_eq!(done.delivery_content_hash.as_deref(), Some("h"));
        // 已完成不再作为永久记忆：同一订单若再次被检测到（页面仍显示待发货），应允许再建任务
        assert!(matches!(task_dedup_check(&conn, "ord-1", "pid-9").unwrap(), DedupVerdict::Create(_)));
    }

    #[test]
    fn failed_task_allows_retry_attempt() {
        let conn = mem_db();
        let k1 = match task_dedup_check(&conn, "o", "p").unwrap() {
            DedupVerdict::Create(k) => k,
            _ => panic!(),
        };
        task_insert(&conn, &DeliveryTask {
            id: "t1".into(), idempotency_key: k1, order_or_conversation_id: "o".into(),
            product_id: None, product_title: Some("p".into()), delivery_content: None,
            delivery_content_hash: None, mode: "auto".into(), status: "异常".into(),
            fail_reason: Some("x".into()), created_at: now(), completed_at: None,
        }).unwrap();
        match task_dedup_check(&conn, "o", "p").unwrap() {
            DedupVerdict::Create(k2) => assert!(k2.contains("attempt1"), "{k2}"),
            _ => panic!("失败后应允许重试"),
        }
    }

    #[test]
    fn audit_stores_redacted_details() {
        let conn = mem_db();
        audit(&conn, None, "test", "访问 https://pan.baidu.com/s/x 提取码: zz99aa").unwrap();
        let list = audit_list(&conn, 10).unwrap();
        assert_eq!(list.len(), 1);
        assert!(!list[0].details_redacted.contains("baidu"));
        assert!(!list[0].details_redacted.contains("zz99aa"));
    }
}
