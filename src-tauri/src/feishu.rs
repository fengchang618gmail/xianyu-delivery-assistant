//! 飞书开放平台客户端：OAuth 授权、令牌刷新与多维表格只读访问。
//! 仅读取“商品 ID / 商品标题 / 发货内容”三列，不写入、不删除任何数据。

use std::error::Error;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::secrets as store;

pub const AUTH_BASE: &str = "https://accounts.feishu.cn/open-apis/authen/v1/authorize";
// 令牌端点统一走 v2 OAuth（RFC 6749，自建/商店应用均支持）。
// 注意：refresh_token 只在授权时声明 scope=offline_access 才会下发，
// 与应用类型无关（v1 oidc/access_token 是旧接口，要求 Bearer app_access_token 头，勿再使用）。
pub const TOKEN_URL: &str = "https://open.feishu.cn/open-apis/authen/v2/oauth/token";
pub const REFRESH_URL: &str = TOKEN_URL;
// 授权页 scope 为增量授予，历史已授予权限会累积保留。
// bitable:app:readonly 读多维表格；wiki:node:read 解析 /wiki/ 分享链接；offline_access 换取 refresh_token。
pub const AUTH_SCOPE: &str = "bitable:app:readonly wiki:node:read offline_access";
pub const API_BASE: &str = "https://open.feishu.cn/open-apis";
pub const REDIRECT_PORT: u16 = 15731;
pub const REDIRECT_URI: &str = "http://127.0.0.1:15731/callback";

pub const FIELD_PRODUCT_ID: &str = "商品 ID";
pub const FIELD_PRODUCT_TITLE: &str = "商品标题";
pub const FIELD_DELIVERY: &str = "发货内容";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProductEntry {
    pub record_id: String,
    pub product_id: Option<String>,
    pub product_title: Option<String>,
    pub product_title_normalized: Option<String>,
    pub delivery_content: String,
}

#[derive(Debug, Serialize)]
pub struct TableValidation {
    pub ok: bool,
    pub missing_fields: Vec<String>,
    pub duplicate_ids: Vec<String>,
    pub duplicate_titles: Vec<String>,
    pub empty_content_rows: usize,
    pub total_rows: usize,
    /// 脱敏后的样例，供向导预览。
    pub sample_redacted: Vec<String>,
    pub error: Option<String>,
}

/// 标准化标题：去除首尾空白并压缩连续空白（PRD 规则）。
pub fn normalize_title(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// 二次匹配用宽容标题键：仅去掉"最末一段 ASCII 词尾的单数/复数 s"差异。
/// 例：订单卡片标题「自学吴恩达《Agent Skills》」与商品库「自学吴恩达《Agent Skill》」
/// 因末尾复数 s 差一个字母而无法严格命中；此键把两者都归一成同一个。
/// 只在严格精确匹配 0 命中时使用（见 tasks::match_products），避免误伤其它标题。
pub fn normalize_title_lax(s: &str) -> String {
    let n = normalize_title(s);
    let b = n.as_bytes();
    // 定位最末一段 ASCII 字母串（中英混排时《》、数字、中文都不是 ASCII 字母）
    let mut end = b.len();
    while end > 0 && !b[end - 1].is_ascii_alphabetic() {
        end -= 1;
    }
    let mut start = end;
    while start > 0 && b[start - 1].is_ascii_alphabetic() {
        start -= 1;
    }
    if start >= end {
        return n;
    }
    let word = &n[start..end];
    if word.len() >= 3 && (word.ends_with('s') || word.ends_with('S')) && !word[..word.len() - 1].ends_with('s') {
        // Skills→Skill；但 Bookcases→… 这类双 s 结尾（库本/复数名词）不去
        let mut out = n;
        out.replace_range(end - 1..end, "");
        out
    } else {
        n
    }
}

/// 从多维表格 URL 提取 (token, table_id, is_wiki)。
/// /base/ 链接 → token 即 app_token；/wiki/ 链接（知识库形态）→ token 为知识节点，
/// 调 API 前需用 `resolve_app_token` 换取真实 obj_token。
pub fn parse_bitable_url(url: &str) -> Result<(String, Option<String>, bool), String> {
    let trimmed = url.trim();
    for (marker, is_wiki) in [("/base/", false), ("/wiki/", true)] {
        let Some(pos) = trimmed.find(marker) else { continue };
        let rest = &trimmed[pos + marker.len()..];
        let token: String = rest.chars().take_while(|c| c.is_ascii_alphanumeric()).collect();
        if token.is_empty() {
            continue;
        }
        let table_id = rest
            .split_once('?')
            .and_then(|(_, q)| q.split('&').find_map(|kv| kv.strip_prefix("table=")))
            .filter(|t| !t.is_empty())
            .map(|t| t.to_string());
        return Ok((token, table_id, is_wiki));
    }
    Err("不是有效的多维表格链接（缺少 /base/ 或 /wiki/）".into())
}

/// 把 /wiki/ 链接解析出的知识节点 token 换成实际 bitable app_token。
/// 需要应用开通「获取知识空间节点信息」(wiki:node:read) 权限。
pub async fn resolve_app_token(
    client: &reqwest::Client,
    user_token: &str,
    token: &str,
    is_wiki: bool,
) -> Result<String, String> {
    if !is_wiki {
        return Ok(token.to_string());
    }
    let url = format!("{API_BASE}/wiki/v2/spaces/get_node?token={token}");
    let resp = client
        .get(&url)
        .bearer_auth(user_token)
        .timeout(std::time::Duration::from_secs(30))
        .send()
        .await
        .map_err(|e| format!("解析知识库链接失败: {e}"))?;
    let json: Value = resp.json().await.map_err(|e| format!("知识库响应解析失败: {e}"))?;
    if json.get("code").and_then(|c| c.as_i64()) != Some(0) {
        let msg = json
            .get("msg")
            .and_then(|m| m.as_str())
            .unwrap_or("未知错误");
        return Err(format!(
            "无法解析知识库节点（飞书错误: {msg}）。请在开发者后台为应用开通「获取知识空间节点信息」(wiki:node:read) 权限并发布新版本"
        ));
    }
    let obj_type = json
        .pointer("/data/node/obj_type")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    if obj_type != "bitable" {
        return Err("该知识库节点不是多维表格".into());
    }
    json.pointer("/data/node/obj_token")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| "知识库节点响应缺少 obj_token".into())
}

/// 构造授权页 URL。scope 必须显式声明 offline_access 才能拿到 refresh_token；
/// scope 名含 ':'，空格按官方示例编码为 %20。
pub fn authorize_url(app_id: &str, state: &str) -> String {
    let scope = AUTH_SCOPE.replace(' ', "%20");
    format!(
        "{AUTH_BASE}?client_id={app_id}&response_type=code&redirect_uri={REDIRECT_URI}&state={state}&scope={scope}"
    )
}

/// 启动一次性本地回调服务器，等待飞书重定向，返回 (code, state)。
pub fn wait_for_oauth_code(timeout_secs: u64) -> Result<(String, String), String> {
    let listener = TcpListener::bind(("127.0.0.1", REDIRECT_PORT))
        .map_err(|e| format!("无法绑定本机回调端口 {REDIRECT_PORT}: {e}"))?;
    listener
        .set_nonblocking(true)
        .map_err(|e| e.to_string())?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);
    loop {
        if std::time::Instant::now() > deadline {
            return Err("等待飞书授权回调超时".into());
        }
        match listener.accept() {
            Ok((mut stream, _)) => {
                // Windows 上 accept 出的套接字继承监听端的非阻塞模式：
                // 请求字节未到达时 read_line 会立刻抛 WSAEWOULDBLOCK(10035)。
                // 必须恢复阻塞模式并设读超时；读不到有效请求就继续等下一次重定向。
                let _ = stream.set_nonblocking(false);
                let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(10)));
                let mut line = String::new();
                let read_ok = match stream.try_clone() {
                    Ok(clone) => {
                        let mut reader = BufReader::new(clone);
                        matches!(reader.read_line(&mut line), Ok(n) if n > 0)
                    }
                    Err(_) => false,
                };
                if !read_ok {
                    continue;
                }
                let code = extract_query_param(&line, "code");
                let state = extract_query_param(&line, "state");
                // 字节字符串字面量只允许 ASCII，中文响应体需走 String
                let body = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nConnection: close\r\n\r\n\
                     <meta charset=\"utf-8\"><body style=\"font-family:sans-serif;text-align:center;padding-top:4em\">\
                     <h2>授权已完成</h2><p>请回到闲鱼发货助手继续。</p></body>"
                );
                let _ = stream.write_all(body.as_bytes());
                let _ = stream.flush();
                if let Some(code) = code {
                    return Ok((code, state.unwrap_or_default()));
                }
                return Err("回调中缺少授权码".into());
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(std::time::Duration::from_millis(150));
            }
            Err(e) => return Err(format!("回调服务器错误: {e}")),
        }
    }
}

fn extract_query_param(request_line: &str, key: &str) -> Option<String> {
    let url_part = request_line.split_whitespace().nth(1)?;
    let query = url_part.split_once('?')?.1;
    query.split('&').find_map(|kv| {
        let (k, v) = kv.split_once('=')?;
        (k == key).then(|| urldecode(v))
    })
}

fn urldecode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                if let Ok(b) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                    out.push(b);
                    i += 3;
                    continue;
                }
                out.push(b'%');
                i += 1;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

// ---------- 令牌 ----------

/// DPAPI 加密文件中的令牌包。飞书 JWT 超出凭据管理器 2560 字节上限，故整体落文件。
#[derive(Debug, Serialize, Deserialize)]
pub struct StoredAccessToken {
    pub access_token: String,
    pub expires_at_epoch: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
}

fn load_bundle(token_dir: &Path) -> Result<Option<StoredAccessToken>, String> {
    match store::load_token_bundle(token_dir)? {
        Some(json) => serde_json::from_str(&json).map(Some).map_err(|e| format!("令牌包解析失败: {e}")),
        None => Ok(None),
    }
}

fn save_bundle(token_dir: &Path, bundle: &StoredAccessToken) -> Result<(), String> {
    let json = serde_json::to_string(bundle).map_err(|e| e.to_string())?;
    store::save_token_bundle(token_dir, &json)
}

/// 是否持有"可用"令牌：access 未过期，或有 refresh_token 可续期。
/// 向导第④步的"已授权"状态以此为准。
pub fn has_usable_token(token_dir: &Path) -> Result<bool, String> {
    Ok(load_bundle(token_dir)?
        .map(|b| {
            chrono::Utc::now().timestamp() < b.expires_at_epoch
                || b.refresh_token.as_deref().map(|r| !r.is_empty()).unwrap_or(false)
        })
        .unwrap_or(false))
}

fn store_token(token_dir: &Path, access_token: &str, expires_in_secs: i64, new_refresh: Option<&str>) -> Result<(), String> {
    // 响应未带新 refresh_token 时沿用旧值（飞书只在首次授权/特定场景下发）。
    let mut bundle = StoredAccessToken {
        access_token: access_token.to_string(),
        expires_at_epoch: chrono::Utc::now().timestamp() + expires_in_secs - 60,
        refresh_token: None,
    };
    if let Some(r) = new_refresh {
        bundle.refresh_token = Some(r.to_string());
    } else if let Ok(Some(old)) = load_bundle(token_dir) {
        bundle.refresh_token = old.refresh_token;
    }
    save_bundle(token_dir, &bundle)
}

/// 用授权码换取用户令牌并保存。
pub async fn exchange_code(
    client: &reqwest::Client,
    app_id: &str,
    app_secret: &str,
    code: &str,
    token_dir: &Path,
) -> Result<(), String> {
    let body = serde_json::json!({
        "grant_type": "authorization_code",
        "client_id": app_id,
        "client_secret": app_secret,
        "code": code,
        "redirect_uri": REDIRECT_URI,
    });
    token_request(client, TOKEN_URL, body, token_dir).await
}

/// 确保持有有效 user_access_token：必要时用 refresh_token 刷新。
pub async fn ensure_access_token(
    client: &reqwest::Client,
    app_id: &str,
    app_secret: &str,
    token_dir: &Path,
) -> Result<String, String> {
    if let Some(stored) = load_bundle(token_dir)? {
        if chrono::Utc::now().timestamp() < stored.expires_at_epoch {
            return Ok(stored.access_token);
        }
    }
    let refresh = load_bundle(token_dir)?
        .and_then(|s| s.refresh_token)
        .filter(|r| !r.is_empty())
        .ok_or("尚未完成飞书授权，请重新点击「开始授权」")?;
    let body = serde_json::json!({
        "grant_type": "refresh_token",
        "client_id": app_id,
        "client_secret": app_secret,
        "refresh_token": refresh,
    });
    token_request(client, REFRESH_URL, body, token_dir).await?;
    load_bundle(token_dir)?
        .map(|s| s.access_token)
        .ok_or_else(|| "令牌刷新后仍无法读取".into())
}

async fn token_request(client: &reqwest::Client, url: &str, body: Value, token_dir: &Path) -> Result<(), String> {
    // 与 send_retry 同理：飞书 CDN 到本机偶发连接不稳（h2 修复后仍有残余瞬时抖动）。
    // 令牌请求同样对传输层错误自动重试；业务错误（HTTP 4xx/5xx + JSON）不重试，
    // 直接进入 parse_token_response 给出针对性提示。
    let mut last_err: Option<String> = None;
    for attempt in 1..=3u32 {
        let send = client
            .post(url)
            .json(&body)
            .timeout(std::time::Duration::from_secs(20))
            .send()
            .await;
        match send {
            Ok(resp) => return parse_token_response(resp, token_dir).await,
            Err(e) => {
                last_err = Some(describe_token_error(&e));
                eprintln!("[feishu] 令牌请求失败(第{attempt}次): {}", last_err.as_deref().unwrap_or_default());
                if attempt < 3 {
                    tokio::time::sleep(std::time::Duration::from_millis(500 * attempt as u64)).await;
                }
            }
        }
    }
    Err(format!(
        "{}（已自动重试 3 次仍失败）",
        last_err.unwrap_or_else(|| "令牌请求失败".into())
    ))
}

/// 把 reqwest 传输层错误转成含类别与完整原因链的描述，便于一眼定位连接/TLS/超时问题。
fn describe_token_error(e: &reqwest::Error) -> String {
    let kind = if e.is_timeout() {
        "请求超时"
    } else if e.is_connect() {
        "无法建立连接"
    } else if e.is_request() {
        "请求发送中断"
    } else if e.is_body() {
        "读取响应中断"
    } else {
        "传输层异常"
    };
    let mut msg = format!("令牌请求失败[{kind}]: {e}");
    let mut src = e.source();
    for _ in 0..8 {
        let Some(s) = src else { break };
        msg.push_str(&format!(" -> {s}"));
        src = s.source();
    }
    msg
}

/// 解析令牌接口 HTTP 响应：非成功/业务错误码给出针对性提示；成功则落盘存储。
async fn parse_token_response(resp: reqwest::Response, token_dir: &Path) -> Result<(), String> {
    let status = resp.status();
    let json: Value = resp.json().await.map_err(|e| format!("令牌响应解析失败: {e}"))?;
    // v2 OAuth 响应为扁平结构：成功 code=0 且 access_token 等字段在顶层；失败带 error/error_description。
    let biz_code = json.get("code").and_then(|c| c.as_i64());
    let has_access = json.get("access_token").and_then(|v| v.as_str()).is_some();
    let success = biz_code == Some(0) || (biz_code.is_none() && has_access);
    if !status.is_success() || !success {
        let code = biz_code.unwrap_or(-1);
        let desc = json
            .get("error_description")
            .or_else(|| json.get("msg"))
            .and_then(|v| v.as_str())
            .unwrap_or("未知错误");
        let hint = match code {
            // 20003/20004/20065 = 授权码无效/过期/已使用；20037 = refresh_token 过期
            20003 | 20004 | 20065 | 20037 => "；请重新点击「开始授权」获取新的授权码",
            20002 => "；App ID 或 App Secret 不正确，请核对第③步的应用凭证",
            20010 => "；当前用户不在应用可用范围内，请在开发者后台发布应用并把当前用户加入可用范围",
            20009 | 20048 | 20069 => "；应用未安装/未启用，请检查开发者后台的应用发布状态",
            20071 => "；回调地址与授权时不一致，请直接重试授权",
            20068 => "；请求的权限超出用户已授权范围，请重新点击「开始授权」",
            _ => "",
        };
        return Err(format!("飞书令牌接口返回错误（HTTP {status}, code {code}）: {desc}{hint}"));
    }
    let access = json
        .get("access_token")
        .and_then(|v| v.as_str())
        .ok_or("响应缺少 access_token")?;
    let expires_in = json.get("expires_in").and_then(|v| v.as_i64()).unwrap_or(7200);
    let refresh = json.get("refresh_token").and_then(|v| v.as_str());
    store_token(token_dir, access, expires_in, refresh)
}

// ---------- 多维表格只读 ----------

/// 列出指定多维表格（app_token）下的全部数据表。
pub async fn list_tables(client: &reqwest::Client, token: &str, app_token: &str) -> Result<Vec<(String, String)>, String> {
    let url = format!("{API_BASE}/bitable/v1/apps/{app_token}/tables?page_size=100");
    let resp = api_get(client, &url, token).await?;
    let items = resp
        .pointer("/data/items")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    Ok(items
        .iter()
        .filter_map(|it| {
            Some((
                it.get("table_id")?.as_str()?.to_string(),
                it.get("name").and_then(|n| n.as_str()).unwrap_or("").to_string(),
            ))
        })
        .collect())
}

/// 列出数据表的全部字段名（不依赖具体字段名，永不 1254045）。
async fn list_field_names(client: &reqwest::Client, token: &str, app_token: &str, table_id: &str) -> Result<Vec<String>, String> {
    let url = format!("{API_BASE}/bitable/v1/apps/{app_token}/tables/{table_id}/fields?page_size=200");
    let resp = api_get(client, &url, token).await?;
    Ok(resp
        .pointer("/data/items")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default()
        .iter()
        .filter_map(|it| it.get("field_name").and_then(|n| n.as_str()).map(|s| s.to_string()))
        .collect())
}

/// 字段名模糊匹配键：去全部空白 + 小写（「商品 ID」「商品ID」「商品　ID」视为同列）。
fn squash_name(s: &str) -> String {
    s.split_whitespace().collect::<String>().to_lowercase()
}

/// 拉取全部记录，只取三列，做标准化与完整校验。
/// 字段名先经 fields/list 容错匹配，避免飞书 1254045 FieldNameNotFound。
pub async fn fetch_products(
    client: &reqwest::Client,
    token: &str,
    app_token: &str,
    table_id: &str,
) -> Result<Vec<ProductEntry>, String> {
    let field_names = list_field_names(client, token, app_token, table_id).await?;
    let resolve = |expected: &str| -> Option<String> {
        field_names
            .iter()
            .find(|f| f.as_str() == expected)
            .or_else(|| field_names.iter().find(|f| squash_name(f) == squash_name(expected)))
            .cloned()
    };
    let actual_id = resolve(FIELD_PRODUCT_ID);
    let actual_title = resolve(FIELD_PRODUCT_TITLE);
    let actual_delivery = resolve(FIELD_DELIVERY);
    let resolved: Vec<&str> = [&actual_id, &actual_title, &actual_delivery]
        .into_iter()
        .filter_map(|o| o.as_deref())
        .collect();
    if resolved.is_empty() {
        return Err(format!(
            "所选数据表中找不到「{}」「{}」「{}」任一字段（该表现有字段：{}）。请确认选对了商品库表，或把列名改成标准名",
            FIELD_PRODUCT_ID,
            FIELD_PRODUCT_TITLE,
            FIELD_DELIVERY,
            if field_names.is_empty() { "（无字段）".to_string() } else { field_names.join("、") }
        ));
    }

    let url = format!("{API_BASE}/bitable/v1/apps/{app_token}/tables/{table_id}/records/search");
    let body = serde_json::json!({ "field_names": resolved });
    let mut out = Vec::new();
    let mut page_token: Option<String> = None;
    loop {
        let resp = send_retry("读取多维表格", || {
            let mut rb = client
                .post(&url)
                .bearer_auth(token)
                .timeout(std::time::Duration::from_secs(30))
                .json(&body);
            if let Some(pt) = &page_token {
                rb = rb.query(&[("page_token", pt)]);
            }
            rb
        })
        .await?;
        let json: Value = resp.json().await.map_err(|e| format!("多维表格响应解析失败: {e}"))?;
        if json.get("code").and_then(|c| c.as_i64()) != Some(0) {
            let code = json.get("code").and_then(|c| c.as_i64()).unwrap_or(-1);
            let msg = json.get("msg").and_then(|m| m.as_str()).unwrap_or("未知");
            let hint = if code == 1254045 {
                "；字段名与表中实际列不一致，请把列名改为「商品 ID / 商品标题 / 发货内容」，或确认选对了表"
            } else {
                ""
            };
            return Err(format!("飞书 API 错误 {code}: {msg}{hint}"));
        }
        if let Some(items) = json.pointer("/data/items").and_then(|v| v.as_array()) {
            for item in items {
                let record_id = item.get("record_id").and_then(|v| v.as_str()).unwrap_or("").to_string();
                let fields = item.get("fields").cloned().unwrap_or_default();
                out.push(ProductEntry {
                    record_id: record_id.clone(),
                    product_id: actual_id.as_deref().and_then(|n| field_string(&fields, n)),
                    product_title: actual_title.as_deref().and_then(|n| field_string(&fields, n)),
                    product_title_normalized: actual_title
                        .as_deref()
                        .and_then(|n| field_string(&fields, n))
                        .map(|t| normalize_title(&t)),
                    delivery_content: actual_delivery
                        .as_deref()
                        .and_then(|n| field_string(&fields, n))
                        .unwrap_or_default(),
                });
            }
        }
        let has_more = json.pointer("/data/has_more").and_then(|v| v.as_bool()).unwrap_or(false);
        page_token = json
            .pointer("/data/page_token")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        if !has_more || page_token.is_none() {
            break;
        }
    }
    Ok(out)
}

/// 提取记录字段值为纯文本（兼容文本段数组 / 数字 / 字符串）。
fn field_string(fields: &Value, name: &str) -> Option<String> {
    let v = fields.get(name)?;
    match v {
        Value::String(s) => Some(s.trim().to_string()),
        Value::Number(n) => Some(n.to_string()),
        Value::Array(items) => {
            let mut out = String::new();
            for item in items {
                match item {
                    Value::String(s) => out.push_str(s),
                    Value::Object(o) => {
                        // 超链接段同时带 text（可见文字，常是飞书文档标题）与 link（真实地址）。
                        // 发货内容要的是原始链接，因此优先取 link，而不是文档标题。
                        let link = o.get("link").and_then(|l| {
                            l.as_str()
                                .or_else(|| l.get("url").and_then(|u| u.as_str()))
                                .or_else(|| l.get("text").and_then(|u| u.as_str()))
                        });
                        if let Some(l) = link {
                            out.push_str(l);
                        } else if let Some(t) = o.get("text").and_then(|t| t.as_str()) {
                            out.push_str(t);
                        } else if let Some(en) = o.get("en_name").and_then(|e| e.as_str()) {
                            out.push_str(en);
                        }
                    }
                    _ => {}
                }
            }
            let trimmed = out.trim().to_string();
            (!trimmed.is_empty()).then_some(trimmed)
        }
        _ => None,
    }
}

/// 校验三列字段、重复 ID/标题与空发货内容，并返回脱敏样例。
pub fn validate_products(rows: &[ProductEntry]) -> TableValidation {
    let mut missing = Vec::new();
    if rows.iter().all(|r| r.product_id.is_none()) {
        missing.push(FIELD_PRODUCT_ID.to_string());
    }
    if rows.iter().all(|r| r.product_title.is_none()) {
        missing.push(FIELD_PRODUCT_TITLE.to_string());
    }
    if rows.iter().all(|r| r.delivery_content.is_empty()) {
        missing.push(FIELD_DELIVERY.to_string());
    }
    let mut id_counts: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
    let mut title_counts: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
    for r in rows {
        if let Some(id) = &r.product_id {
            *id_counts.entry(id.as_str()).or_default() += 1;
        }
        if let Some(t) = &r.product_title_normalized {
            *title_counts.entry(t.as_str()).or_default() += 1;
        }
    }
    let duplicate_ids = id_counts
        .into_iter()
        .filter(|(_, n)| *n > 1)
        .map(|(k, _)| k.to_string())
        .collect::<Vec<_>>();
    let duplicate_titles = title_counts
        .into_iter()
        .filter(|(_, n)| *n > 1)
        .map(|(k, _)| k.to_string())
        .collect::<Vec<_>>();
    let empty_content_rows = rows.iter().filter(|r| r.delivery_content.is_empty()).count();
    let sample_redacted = rows
        .iter()
        .take(3)
        .map(|r| {
            crate::redact::redact_text(&format!(
                "{} | {} | {}",
                r.product_id.as_deref().unwrap_or("-"),
                r.product_title.as_deref().unwrap_or("-"),
                if r.delivery_content.is_empty() { "(空)" } else { "内容已就绪" }
            ))
        })
        .collect();
    let ok = missing.is_empty() && duplicate_ids.is_empty() && duplicate_titles.is_empty() && empty_content_rows == 0;
    TableValidation {
        ok,
        missing_fields: missing,
        duplicate_ids,
        duplicate_titles,
        empty_content_rows,
        total_rows: rows.len(),
        sample_redacted,
        error: None,
    }
}

/// 带重试的请求发送：连接/超时类错误重试并打日志（飞书 CDN 到本机偶发不稳）；
/// 其余错误立即返回原始信息，便于在 app.log 里定位真实原因。
async fn send_retry(desc: &str, build: impl Fn() -> reqwest::RequestBuilder) -> Result<reqwest::Response, String> {
    let mut last = String::new();
    for attempt in 1..=3 {
        let rb = build();
        match rb.send().await {
            Ok(resp) => return Ok(resp),
            Err(e) => {
                last = format!("{desc} 请求失败(第{attempt}次): {e}");
                eprintln!("[feishu] {last}");
                if !e.is_connect() && !e.is_timeout() && !e.is_request() {
                    return Err(last); // 非连接/超时类（如 TLS 协商）不重试，直接暴露
                }
                tokio::time::sleep(std::time::Duration::from_millis(500 * attempt as u64)).await;
            }
        }
    }
    Err(last)
}

async fn api_get(client: &reqwest::Client, url: &str, token: &str) -> Result<Value, String> {
    let resp = send_retry("读取飞书", || {
        client.get(url).bearer_auth(token).timeout(std::time::Duration::from_secs(30))
    })
    .await?;
    let json: Value = resp.json().await.map_err(|e| e.to_string())?;
    if json.get("code").and_then(|c| c.as_i64()) != Some(0) {
        return Err(format!(
            "飞书 API 错误 {}: {}",
            json.get("code").and_then(|c| c.as_i64()).unwrap_or(-1),
            json.get("msg").and_then(|m| m.as_str()).unwrap_or("未知")
        ));
    }
    Ok(json)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_bitable_url() {
        let (a, t, w) = parse_bitable_url("https://abc.feishu.cn/base/BaseTok123?table=tblXYZ&view=vew1").unwrap();
        assert_eq!(a, "BaseTok123");
        assert_eq!(t.as_deref(), Some("tblXYZ"));
        assert!(!w);
        let (a2, t2, w2) = parse_bitable_url("  https://x.feishu.cn/base/Tok9  ").unwrap();
        assert_eq!(a2, "Tok9");
        assert_eq!(t2, None);
        assert!(!w2);
        // 知识库形态：/wiki/ 节点 token + table 参数同样可提取
        let (a3, t3, w3) = parse_bitable_url(
            "https://h0vi6o0by5.feishu.cn/wiki/E1PuwNpMGiWH90kxZkrc2pFRnEb?from=from_copylink&table=tblABC",
        )
        .unwrap();
        assert_eq!(a3, "E1PuwNpMGiWH90kxZkrc2pFRnEb");
        assert_eq!(t3.as_deref(), Some("tblABC"));
        assert!(w3);
        assert!(parse_bitable_url("https://x.feishu.cn/docs/xxx").is_err());
    }

    #[test]
    fn normalizes_title_per_prd() {
        assert_eq!(normalize_title("  Python  入门　课程 "), "Python 入门 课程");
    }

    #[test]
    fn lax_key_strips_trailing_plural_s_only() {
        // Agent Skills 与 Agent Skill 归一为同一键
        assert_eq!(
            normalize_title_lax("自学吴恩达《Agent Skills》"),
            normalize_title_lax("自学吴恩达《Agent Skill》")
        );
        // 已单数的标题保持不变
        assert_eq!(normalize_title_lax("自学吴恩达《Agent Skill》"), "自学吴恩达《Agent Skill》");
        // 不含英文复数 s 的标题不动；双 s 结尾（Class 等）不动
        assert_eq!(normalize_title_lax("吴恩达AI提示词入门课"), "吴恩达AI提示词入门课");
        assert_eq!(normalize_title_lax("The Great Class"), "The Great Class");
    }

    #[test]
    fn extracts_field_values() {
        let fields = serde_json::json!({
            FIELD_PRODUCT_ID: 12345,
            FIELD_PRODUCT_TITLE: [{ "text": "Python 入门课程" }],
            FIELD_DELIVERY: "链接: https://pan.cn/x 提取码: ab12"
        });
        assert_eq!(field_string(&fields, FIELD_PRODUCT_ID).as_deref(), Some("12345"));
        assert_eq!(field_string(&fields, FIELD_PRODUCT_TITLE).as_deref(), Some("Python 入门课程"));
        assert!(field_string(&fields, FIELD_DELIVERY).unwrap().starts_with("链接"));
    }

    #[test]
    fn validation_flags_duplicates_and_empty() {
        let rows = vec![
            ProductEntry {
                record_id: "r1".into(),
                product_id: Some("p1".into()),
                product_title: Some("同标题".into()),
                product_title_normalized: Some("同标题".into()),
                delivery_content: "内容".into(),
            },
            ProductEntry {
                record_id: "r2".into(),
                product_id: Some("p1".into()),
                product_title: Some(" 同标题 ".into()),
                product_title_normalized: Some("同标题".into()),
                delivery_content: String::new(),
            },
        ];
        let v = validate_products(&rows);
        assert!(!v.ok);
        assert_eq!(v.duplicate_ids, vec!["p1"]);
        assert_eq!(v.duplicate_titles, vec!["同标题"]);
        assert_eq!(v.empty_content_rows, 1);
    }
}
