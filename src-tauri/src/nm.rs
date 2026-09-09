//! 桌面端与扩展伴随进程（nmhost）之间的本机命名管道服务。
//! nmhost 负责浏览器 Native Messaging 的长度前缀协议，本模块只处理业务 JSON。
//! 安全边界：管道限本机当前用户；连接须携带应用启动时写入磁盘的会话令牌。

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::windows::named_pipe::ServerOptions;
use tokio::sync::{mpsc, Mutex};

pub const PIPE_NAME: &str = r"\\.\pipe\xianyu-delivery-assistant-nm";

// ---------- 协议 ----------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PageEventPayload {
    pub order_id: String,
    #[serde(default)]
    pub conversation_id: String,
    #[serde(default)]
    pub product_id: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub page_url: String,
    #[serde(default)]
    pub dom_healthy: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SendResultPayload {
    pub task_id: String,
    pub ok: bool,
    #[serde(default)]
    pub evidence: String,
    #[serde(default)]
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecheckPayload {
    pub task_id: String,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub product_id: String,
    #[serde(default)]
    pub title: String,
    /// 内容脚本是否确认“当前打开的会话就是该订单”。
    /// 只有 order_seen=true 时的“已变化”才是可信结论（卖家可能已手动发货），
    /// “未知”（看不到该订单）不得触发任何发送或终结。
    #[serde(default)]
    pub order_seen: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusReportPayload {
    #[serde(default)]
    pub logged_in: bool,
    #[serde(default)]
    pub dom_healthy: bool,
    #[serde(default)]
    pub detail: String,
}

/// 页面诊断：仅结构与关键词标志，不含任何聊天内容。
#[derive(Debug, Clone, Deserialize)]
pub struct PageDiagPayload {
    #[serde(default)]
    pub url_path: String,
    #[serde(default)]
    pub iframes: Vec<String>,
    #[serde(default)]
    pub has_wait_ship: bool,
    #[serde(default)]
    pub has_dai_fahuo: bool,
    #[serde(default)]
    pub has_deng_fahuo: bool,
    #[serde(default)]
    pub has_fahuo: bool,
    #[serde(default)]
    pub has_daifu: bool,
    #[serde(default)]
    pub body_len: u64,
    #[serde(default)]
    pub input_found: bool,
    #[serde(default)]
    pub send_found: bool,
}

#[derive(Debug, Clone)]
pub enum Inbound {
    Hello { token: String, ext_version: String },
    PageEvent(PageEventPayload),
    SendResult(SendResultPayload),
    RecheckResult(RecheckPayload),
    StatusReport(StatusReportPayload),
    PageDiag(PageDiagPayload),
}

pub fn decode_inbound(line: &str) -> Option<Inbound> {
    let v: Value = serde_json::from_str(line).ok()?;
    if v.get("v").and_then(|x| x.as_u64()) != Some(1) {
        return None;
    }
    let t = v.get("type")?.as_str()?;
    let payload = v.get("payload").cloned().unwrap_or_default();
    Some(match t {
        "hello" => Inbound::Hello {
            token: payload.get("token").and_then(|x| x.as_str()).unwrap_or("").into(),
            ext_version: payload.get("ext_version").and_then(|x| x.as_str()).unwrap_or("").into(),
        },
        "page_event" => serde_json::from_value(payload).ok().map(Inbound::PageEvent)?,
        "send_result" => serde_json::from_value(payload).ok().map(Inbound::SendResult)?,
        "recheck_result" => serde_json::from_value(payload).ok().map(Inbound::RecheckResult)?,
        "status_report" => serde_json::from_value(payload).ok().map(Inbound::StatusReport)?,
        "page_diag" => serde_json::from_value(payload).ok().map(Inbound::PageDiag)?,
        _ => return None,
    })
}

pub fn encode_outbound(type_name: &str, payload: &Value) -> String {
    serde_json::json!({ "v": 1, "type": type_name, "payload": payload }).to_string()
}

// ---------- 连接句柄 ----------

/// 应用状态持有的扩展连接句柄：向当前连接发送出站消息。
#[derive(Clone, Default)]
pub struct NmHandle {
    tx: Arc<Mutex<Option<mpsc::Sender<String>>>>,
}

impl NmHandle {
    pub fn new() -> Self {
        Self::default()
    }

    pub async fn send(&self, type_name: &str, payload: &Value) -> Result<(), String> {
        let guard = self.tx.lock().await;
        match guard.as_ref() {
            Some(tx) => tx
                .send(encode_outbound(type_name, payload))
                .await
                .map_err(|_| "扩展连接已断开".to_string()),
            None => Err("扩展未连接".to_string()),
        }
    }

    pub fn connected(&self) -> bool {
        self.tx.try_lock().map(|g| g.is_some()).unwrap_or(true)
    }

    fn set_tx(&self, tx: Option<mpsc::Sender<String>>) {
        if let Ok(mut guard) = self.tx.try_lock() {
            *guard = tx;
        }
    }
}

/// 启动管道服务器循环：接受 nmhost 连接，逐行读取入站 JSON 并交给 `on_message`。
/// `on_message` 为 async 业务回调，返回的字符串（已编码 JSON）会立即回写给扩展。
pub async fn run_server<F, Fut>(handle: NmHandle, session_token: String, on_message: F) -> Result<(), String>
where
    F: Fn(Inbound) -> Fut + Send + Sync + 'static,
    Fut: std::future::Future<Output = Option<String>> + Send + 'static,
{
    let on_message = Arc::new(on_message);
    // 首个管道实例：独占创建，避免残留旧实例干扰
    let server = ServerOptions::new()
        .first_pipe_instance(true)
        .create(PIPE_NAME)
        .map_err(|e| format!("创建命名管道失败: {e}"))?;
    eprintln!("[nm] 管道实例已创建: {PIPE_NAME}");
    let mut pending_server = Some(server);
    loop {
        // 实例缺失（上次准备失败）或连接中断都不能退出服务器循环：
        // 退出意味着扩展在应用重启前永远无法再连接。重建实例并继续。
        let pipe = match pending_server.take() {
            Some(p) => p,
            None => match ServerOptions::new().create(PIPE_NAME) {
                Ok(p) => p,
                Err(e) => {
                    eprintln!("重建管道实例失败: {e}");
                    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                    continue;
                }
            },
        };
        if let Err(e) = pipe.connect().await {
            // 客户端在连接完成前消失（例如 nmhost 进程被杀）：该实例作废，重建后继续
            eprintln!("等待连接失败: {e}");
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            continue;
        }
        // 为下一个客户端准备新实例
        pending_server = ServerOptions::new().create(PIPE_NAME).ok();

        let (out_tx, mut out_rx) = mpsc::channel::<String>(64);
        handle.set_tx(Some(out_tx.clone()));
        let (reader, mut writer) = tokio::io::split(pipe);

        let write_task = tokio::spawn(async move {
            while let Some(line) = out_rx.recv().await {
                if writer.write_all(line.as_bytes()).await.is_err() || writer.write_all(b"\n").await.is_err() {
                    break;
                }
                let _ = writer.flush().await;
            }
        });

        let read_handle = handle.clone();
        let reply_tx = out_tx.clone();
        let on_message = on_message.clone();
        let expected_token = session_token.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(reader).lines();
            let mut authenticated = false;
            loop {
                match lines.next_line().await {
                    Ok(Some(line)) if line.trim().is_empty() => continue,
                    Ok(Some(line)) => {
                        let Some(msg) = decode_inbound(&line) else { continue };
                        if !authenticated {
                            // 首条消息必须是携带正确令牌的 Hello；其余一律断开
                            let authenticated_ok = match &msg {
                                Inbound::Hello { token, .. } => *token == expected_token,
                                _ => false,
                            };
                            if !authenticated_ok {
                                break;
                            }
                            authenticated = true;
                            let _ = reply_tx
                                .send(encode_outbound("hello_ok", &serde_json::json!({ "ok": true })))
                                .await;
                            // 认证成功的 Hello 继续进入业务回调：
                            // 触发 ext-conn 前端事件与 ext_connected 审计
                        }
                        if let Some(reply) = on_message(msg).await {
                            let _ = reply_tx.send(reply).await;
                        }
                    }
                    Ok(None) | Err(_) => break,
                }
            }
            read_handle.set_tx(None);
            write_task.abort();
        });
    }
}
