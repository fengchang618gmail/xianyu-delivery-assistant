//! 闲鱼发货助手：Tauri 桌面端装配层。
//! 组合 db / secrets / feishu / nm / tasks 模块，暴露前端命令与扩展消息分发。

pub mod db;
pub mod feishu;
pub mod nm;
pub mod redact;
pub mod secrets;
pub mod state;
pub mod tasks;

use std::sync::Arc;

use serde::Serialize;
use serde_json::json;
use tauri::{AppHandle, Emitter, Manager, State};
use winreg::enums::{HKEY_CURRENT_USER, KEY_SET_VALUE};
use winreg::RegKey;

use nm::Inbound;
use state::{with_db, AppState, Mode};

/// 浏览器 Native Messaging Host 名称与扩展固定 ID（由 manifest.json 的 key 派生）。
pub const NM_HOST_NAME: &str = "com.local.xianyu.deliveryassistant";
pub const EXTENSION_ID: &str = "lkfdpfllnpejncdiggdbiffiebombajd";

// ---------- 启动装配 ----------

pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_opener::init())
        .setup(|app| {
            let handle = app.handle().clone();
            let data_dir = handle
                .path()
                .app_data_dir()
                .map_err(|e| format!("无法确定数据目录: {e}"))?;
            std::fs::create_dir_all(&data_dir).ok();

            let conn = db::open(&data_dir.join("data.db"))?;

            // 内容摘要盐：首次启动生成，长期复用
            let salt = match db::config_get(&conn, "content_salt") {
                Some(s) if !s.is_empty() => s,
                _ => {
                    let s = redact::new_salt();
                    db::config_set(&conn, "content_salt", &s)?;
                    s
                }
            };

            // 恢复模式与暂停状态
            let mode = db::config_get(&conn, "send_mode")
                .and_then(|m| Mode::from_str(&m))
                .unwrap_or(Mode::Confirm);
            let paused = db::config_get(&conn, "paused").map(|v| v == "1").unwrap_or(false);

            // 会话令牌：nmhost 连接管道时必须出示
            let session_token = redact::new_token();
            std::fs::write(data_dir.join("nm_token"), &session_token).ok();

            let app_state = Arc::new(AppState {
                db: db::Db(std::sync::Mutex::new(conn)),
                // 强制 HTTP/1.1：reqwest 默认协商 HTTP/2，本机到飞书 CDN 的链路 h2 握手不稳定，
                // 会出现 “error sending request” 而 curl（HTTP/1.1）正常。http1_only 规避之。
                http: reqwest::Client::builder()
                    .http1_only()
                    .connect_timeout(std::time::Duration::from_secs(10))
                    .build()
                    .expect("构建 HTTP 客户端失败"),
                mode: std::sync::Mutex::new(mode),
                paused: std::sync::Mutex::new(paused),
                nm: nm::NmHandle::new(),
                data_dir: data_dir.clone(),
                salt,
                products: tokio::sync::Mutex::new(None),
                pending_approvals: std::sync::Mutex::new(Default::default()),
            });
            handle.manage(app_state.clone());

            // 周期协调：定时回查挂起任务对应的订单状态（捕获手动发货等不触发 page_event 的变更）
            tasks::spawn_reconcile_ticker(app_state.clone());

            // 命名管道服务器：接收扩展（经 nmhost）消息
            let st = app_state.clone();
            let h = handle.clone();
            let token = session_token.clone();
            tauri::async_runtime::spawn(async move {
                let st2 = st.clone();
                let h2 = h.clone();
                let dispatch = move |msg: Inbound| {
                    let st = st2.clone();
                    let h = h2.clone();
                    async move { on_nm_message(&st, &h, msg).await }
                };
                // 兜底：run_server 因任何原因返回时重启循环，保证扩展随时能连上
                eprintln!("[nm] 管道服务器任务已启动");
                loop {
                    if let Err(e) = nm::run_server(app_state.nm.clone(), token.clone(), dispatch.clone()).await {
                        with_db(&st, |conn| db::audit(conn, None, "nm_server_error", &e)).ok();
                    }
                    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                }
            });

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            app_status,
            set_mode,
            set_paused,
            feishu_save_app,
            feishu_start_oauth,
            feishu_list_tables,
            feishu_validate_table,
            feishu_preview_template,
            feishu_sync_products,
            feishu_disconnect,
            extension_status,
            extension_register,
            extension_open_page,
            tasks_active,
            tasks_recent,
            task_confirm,
            task_cancel,
            records_recent,
            wipe_records,
        ])
        .run(tauri::generate_context!())
        .expect("闲鱼发货助手启动失败");
}

/// 扩展入站消息分发。
async fn on_nm_message(state: &Arc<AppState>, app: &AppHandle, msg: Inbound) -> Option<String> {
    match msg {
        Inbound::Hello { ext_version, .. } => {
            app.emit("ext-conn", json!({ "connected": true, "ext_version": ext_version })).ok();
            with_db(state, |conn| db::audit(conn, None, "ext_connected", &format!("扩展已连接 (v{ext_version})"))).ok();
            Some(nm::encode_outbound(
                "pause_state",
                &json!({ "paused": state.paused_now() }),
            ))
        }
        Inbound::PageEvent(p) => {
            tasks::handle_page_event(state, app, p).await;
            None
        }
        Inbound::SendResult(p) => {
            tasks::handle_send_result(state, app, p).await;
            None
        }
        Inbound::RecheckResult(p) => {
            tasks::handle_recheck_result(state, app, p).await;
            None
        }
        Inbound::StatusReport(p) => {
            app.emit("ext-status", json!({ "logged_in": p.logged_in, "dom_healthy": p.dom_healthy, "detail": p.detail })).ok();
            with_db(state, |conn| {
                db::audit(
                    conn,
                    None,
                    "ext_status",
                    &format!("登录: {} DOM: {} {}", p.logged_in, p.dom_healthy, p.detail),
                )
            })
            .ok();
            None
        }
        Inbound::PageDiag(p) => {
            // 仅记录结构标志与路径（无查询串、无聊天内容），用于定位订单识别问题
            let frames = if p.iframes.is_empty() {
                "无iframe".to_string()
            } else {
                format!("iframe[{}]", p.iframes.join(","))
            };
            with_db(state, |conn| {
                db::audit(
                    conn,
                    None,
                    "page_diag",
                    &format!(
                        "path={} {} 等待卖家发货={} 待发货={} 等待发货={} 发货={} 待付款={} body_len={} input={} send={}",
                        p.url_path, frames, p.has_wait_ship, p.has_dai_fahuo, p.has_deng_fahuo,
                        p.has_fahuo, p.has_daifu, p.body_len, p.input_found, p.send_found
                    ),
                )
            })
            .ok();
            None
        }
    }
}

// ---------- 状态与模式 ----------

#[derive(Serialize)]
struct AppStatus {
    mode: String,
    paused: bool,
    ext_connected: bool,
    feishu_app_configured: bool,
    feishu_authorized: bool,
    bitable_selected: bool,
    data_dir: String,
}

#[tauri::command]
async fn app_status(state: State<'_, Arc<AppState>>) -> Result<AppStatus, String> {
    let s = state.inner();
    let feishu_app_configured = with_db(s, |conn| Ok(db::config_get(conn, "feishu_app_id").is_some()))?
        && secrets::Secret::FeishuAppSecret.load()?.is_some();
    // “已授权”须含可用性：access 未过期或有 refresh_token 可续期，否则向导应重新引导授权
    let feishu_authorized = feishu::has_usable_token(&s.data_dir)?;
    let bitable_selected = with_db(s, |conn| Ok(db::config_get(conn, "feishu_bitable_url").is_some()))?;
    let data_dir = with_db(s, |conn| {
        Ok(conn
            .query_row("SELECT file FROM pragma_database_list WHERE seq = 0", [], |r| {
                r.get::<_, String>(0)
            })
            .unwrap_or_default())
    })?;
    Ok(AppStatus {
        mode: s.mode_now().as_str().into(),
        paused: s.paused_now(),
        ext_connected: s.nm.connected(),
        feishu_app_configured,
        feishu_authorized,
        bitable_selected,
        data_dir,
    })
}

#[tauri::command]
async fn set_mode(state: State<'_, Arc<AppState>>, app: AppHandle, mode: String) -> Result<(), String> {
    let m = Mode::from_str(&mode).ok_or("无效模式")?;
    let s = state.inner();
    s.set_mode(m);
    with_db(s, |conn| db::config_set(conn, "send_mode", m.as_str()))?;
    with_db(s, |conn| {
        db::audit(conn, None, "mode_changed", &format!("切换为 {}", if m == Mode::Auto { "自动发送" } else { "确认后发送" }))
    })
    .ok();
    tasks::emit_tasks(&app);
    Ok(())
}

#[tauri::command]
async fn set_paused(state: State<'_, Arc<AppState>>, paused: bool) -> Result<(), String> {
    let s = state.inner();
    s.set_paused(paused);
    with_db(s, |conn| db::config_set(conn, "paused", if paused { "1" } else { "0" }))?;
    with_db(s, |conn| {
        db::audit(conn, None, "pause_changed", if paused { "监控已暂停" } else { "监控已恢复" })
    })
    .ok();
    state
        .nm
        .send("pause_state", &json!({ "paused": paused }))
        .await
        .ok();
    Ok(())
}

// ---------- 飞书配置 ----------

#[tauri::command]
async fn feishu_save_app(state: State<'_, Arc<AppState>>, app_id: String, app_secret: String) -> Result<(), String> {
    let app_id = app_id.trim().to_string();
    let app_secret = app_secret.trim().to_string();
    if app_id.is_empty() || app_secret.is_empty() {
        return Err("App ID 与 App Secret 不能为空".into());
    }
    let s = state.inner();
    with_db(s, |conn| db::config_set(conn, "feishu_app_id", &app_id))?;
    secrets::Secret::FeishuAppSecret.save(&app_secret)?;
    with_db(s, |conn| db::audit(conn, None, "feishu_app_saved", &format!("已保存飞书应用 {app_id} 的凭据")))?;
    Ok(())
}

#[tauri::command]
async fn feishu_start_oauth(state: State<'_, Arc<AppState>>, app: AppHandle) -> Result<String, String> {
    let s = state.inner();
    let (app_id, app_secret) = state::feishu_app_credentials(s)?;
    let oauth_state = redact::new_state();
    with_db(s, |conn| db::config_set(conn, "oauth_state", &oauth_state))?;
    let url = feishu::authorize_url(&app_id, &oauth_state);

    let st = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        let result = (|| -> Result<(), String> {
            let (code, returned_state) = feishu::wait_for_oauth_code(300)?;
            let expected = with_db(&st, |conn| Ok(db::config_get(conn, "oauth_state")))?.unwrap_or_default();
            if returned_state != expected {
                return Err("OAuth state 校验失败，请重试".into());
            }
            tauri::async_runtime::block_on(async {
                feishu::exchange_code(&st.http, &app_id, &app_secret, &code, &st.data_dir).await
            })?;
            with_db(&st, |conn| db::config_del(conn, "oauth_state"))?;
            with_db(&st, |conn| db::audit(conn, None, "feishu_authorized", "飞书授权成功"))?;
            Ok(())
        })();
        match result {
            Ok(()) => {
                app.emit("feishu-oauth", json!({ "ok": true })).ok();
            }
            Err(e) => {
                with_db(&st, |conn| db::audit(conn, None, "feishu_oauth_failed", &e)).ok();
                app.emit("feishu-oauth", json!({ "ok": false, "error": e })).ok();
            }
        }
    });
    Ok(url)
}

#[tauri::command]
async fn feishu_list_tables(state: State<'_, Arc<AppState>>, link: String) -> Result<Vec<(String, String)>, String> {
    let s = state.inner();
    let (app_id, app_secret) = state::feishu_app_credentials(s)?;
    let token = feishu::ensure_access_token(&s.http, &app_id, &app_secret, &s.data_dir).await?;
    let (raw_token, _, is_wiki) = feishu::parse_bitable_url(&link)?;
    let app_token = feishu::resolve_app_token(&s.http, &token, &raw_token, is_wiki).await?;
    feishu::list_tables(&s.http, &token, &app_token).await
}

#[tauri::command]
async fn feishu_validate_table(
    state: State<'_, Arc<AppState>>,
    app: AppHandle,
    link: String,
    table_id: String,
) -> Result<feishu::TableValidation, String> {
    let s = state.inner();
    let (app_id, app_secret) = state::feishu_app_credentials(s)?;
    let token = feishu::ensure_access_token(&s.http, &app_id, &app_secret, &s.data_dir).await?;
    // 校验链接可解析；table_id 以用户选择为准；/wiki/ 链接先换真实 app_token
    let (raw_token, _, is_wiki) = feishu::parse_bitable_url(&link)?;
    let app_token = feishu::resolve_app_token(&s.http, &token, &raw_token, is_wiki).await?;
    let rows = feishu::fetch_products(&s.http, &token, &app_token, &table_id).await?;
    let validation = feishu::validate_products(&rows);
    if validation.ok {
        with_db(s, |conn| {
            db::config_set(conn, "feishu_bitable_url", link.trim())?;
            db::config_set(conn, "feishu_table_id", &table_id)?;
            db::audit(
                conn,
                None,
                "bitable_validated",
                &format!("多维表格校验通过：{} 行商品", validation.total_rows),
            )?;
            Ok(())
        })?;
        *s.products.lock().await = Some((std::time::Instant::now(), rows));
        tasks::emit_tasks(&app);
    } else {
        with_db(s, |conn| {
            db::audit(
                conn,
                None,
                "bitable_invalid",
                &format!(
                    "字段缺失 {:?} 重复ID {:?} 重复标题 {:?} 空内容 {} 行",
                    validation.missing_fields, validation.duplicate_ids, validation.duplicate_titles, validation.empty_content_rows
                ),
            )
        })
        .ok();
    }
    Ok(validation)
}

/// 向导"安全测试"：预览一条真实模板（仅显示，不发送、不落盘）。
#[tauri::command]
async fn feishu_preview_template(state: State<'_, Arc<AppState>>) -> Result<serde_json::Value, String> {
    let s = state.inner();
    let rows = tasks::refresh_products(s, false).await?;
    let row = rows
        .iter()
        .find(|r| !r.delivery_content.is_empty())
        .ok_or("商品库中没有含发货内容的行")?;
    Ok(json!({
        "product_id": row.product_id,
        "product_title": row.product_title,
        "delivery_content": row.delivery_content,
        "note": "仅预览，不会发送该内容",
    }))
}

/// 手动同步商品库：强制从飞书重读（越过 TTL），返回条数；供 UI “同步”按钮调用。
#[tauri::command]
async fn feishu_sync_products(state: State<'_, Arc<AppState>>) -> Result<usize, String> {
    let s = state.inner();
    match tasks::refresh_products(s, true).await {
        Ok(rows) => {
            with_db(s, |conn| {
                db::audit(conn, None, "manual_sync", &format!("手动同步商品库完成（{} 条）", rows.len()))
            })
            .ok();
            Ok(rows.len())
        }
        Err(e) => {
            // 失败也落审计：手动同步失败通常伴随令牌刷新/网络问题，留痕便于在“最近记录”里追溯。
            with_db(s, |conn| db::audit(conn, None, "manual_sync_failed", &e)).ok();
            Err(e)
        }
    }
}

#[tauri::command]
async fn feishu_disconnect(state: State<'_, Arc<AppState>>) -> Result<(), String> {
    let s = state.inner();
    secrets::clear_all_feishu(&s.data_dir)?;
    with_db(s, |conn| {
        for k in ["feishu_app_id", "feishu_bitable_url", "feishu_table_id", "oauth_state"] {
            db::config_del(conn, k)?;
        }
        db::audit(conn, None, "feishu_disconnected", "已断开飞书授权并清除本机凭据")?;
        Ok(())
    })?;
    *s.products.lock().await = None;
    Ok(())
}

// ---------- 浏览器扩展 ----------

#[derive(Serialize)]
struct ExtensionStatus {
    registered: bool,
    manifest_path: String,
    host_path: String,
    host_exists: bool,
    chrome_found: bool,
    edge_found: bool,
    extension_dir: Option<String>,
    extension_id: String,
}

fn browser_paths() -> Vec<(&'static str, Vec<std::path::PathBuf>)> {
    let exe = |p: &str| std::path::PathBuf::from(p);
    let local = std::env::var("LOCALAPPDATA").unwrap_or_default();
    vec![
        (
            "chrome",
            vec![
                exe("C:\\Program Files\\Google\\Chrome\\Application\\chrome.exe"),
                exe("C:\\Program Files (x86)\\Google\\Chrome\\Application\\chrome.exe"),
                exe(&format!("{local}\\Google\\Chrome\\Application\\chrome.exe")),
            ],
        ),
        (
            "edge",
            vec![
                exe("C:\\Program Files (x86)\\Microsoft\\Edge\\Application\\msedge.exe"),
                exe("C:\\Program Files\\Microsoft\\Edge\\Application\\msedge.exe"),
            ],
        ),
    ]
}

fn find_browser(name: &str) -> Option<std::path::PathBuf> {
    browser_paths()
        .into_iter()
        .find(|(n, _)| *n == name)
        .and_then(|(_, paths)| paths.into_iter().find(|p| p.is_file()))
}

fn nm_manifest_path(app: &AppHandle) -> Result<std::path::PathBuf, String> {
    let dir = app
        .path()
        .app_data_dir()
        .map_err(|e| e.to_string())?
        .join("native");
    Ok(dir.join(format!("{NM_HOST_NAME}.json")))
}

#[tauri::command]
async fn extension_status(state: State<'_, Arc<AppState>>, app: AppHandle) -> Result<ExtensionStatus, String> {
    let _ = state;
    let manifest_path = nm_manifest_path(&app)?;
    let host_path = current_dir_exe()?.join("nmhost.exe");
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let registered = ["Software\\Google\\Chrome\\NativeMessagingHosts", "Software\\Microsoft\\Edge\\NativeMessagingHosts"]
        .iter()
        .any(|base| {
            hkcu.open_subkey_with_flags(format!("{base}\\{NM_HOST_NAME}"), winreg::enums::KEY_READ)
                .map(|k| k.get_value::<String, _>("").is_ok())
                .unwrap_or(false)
        });
    Ok(ExtensionStatus {
        registered,
        manifest_path: manifest_path.display().to_string(),
        host_path: host_path.display().to_string(),
        host_exists: host_path.is_file(),
        chrome_found: find_browser("chrome").is_some(),
        edge_found: find_browser("edge").is_some(),
        extension_dir: extension_dir(&app).map(|p| p.display().to_string()),
        extension_id: EXTENSION_ID.into(),
    })
}

#[tauri::command]
async fn extension_register(state: State<'_, Arc<AppState>>, app: AppHandle) -> Result<ExtensionStatus, String> {
    let _ = state;
    let host_path = current_dir_exe()?.join("nmhost.exe");
    if !host_path.is_file() {
        return Err(format!("未找到本机通信组件: {}", host_path.display()));
    }
    let manifest_path = nm_manifest_path(&app)?;
    std::fs::create_dir_all(manifest_path.parent().unwrap()).map_err(|e| e.to_string())?;
    let manifest = json!({
        "name": NM_HOST_NAME,
        "description": "闲鱼发货助手本机通信组件",
        "path": host_path.display().to_string(),
        "type": "stdio",
        "allowed_origins": [format!("chrome-extension://{EXTENSION_ID}/")],
    });
    std::fs::write(
        &manifest_path,
        serde_json::to_string_pretty(&manifest).map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;

    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    for base in ["Software\\Google\\Chrome\\NativeMessagingHosts", "Software\\Microsoft\\Edge\\NativeMessagingHosts"] {
        let (key, _) = hkcu
            .create_subkey_with_flags(format!("{base}\\{NM_HOST_NAME}"), KEY_SET_VALUE)
            .map_err(|e| format!("写注册表失败 ({base}): {e}"))?;
        key.set_value("", &manifest_path.display().to_string())
            .map_err(|e| e.to_string())?;
    }
    with_db(state.inner(), |conn| {
        db::audit(conn, None, "nm_registered", &format!("本机通信组件已注册: {}", manifest_path.display()))
    })
    .ok();
    extension_status(state, app).await
}

/// 打开浏览器的扩展管理页（chrome://extensions），由用户手动"加载已解压的扩展程序"。
#[tauri::command]
async fn extension_open_page(browser: String) -> Result<(), String> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let exe = find_browser(&browser).ok_or_else(|| format!("未找到浏览器 {browser}"))?;
    let page = if browser == "edge" { "edge://extensions" } else { "chrome://extensions" };
    std::process::Command::new(exe)
        .arg(page)
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .map_err(|e| format!("启动浏览器失败: {e}"))?;
    Ok(())
}

fn current_dir_exe() -> Result<std::path::PathBuf, String> {
    std::env::current_exe()
        .map_err(|e| e.to_string())?
        .parent()
        .map(|p| p.to_path_buf())
        .ok_or_else(|| "无法确定程序目录".into())
}

/// 扩展目录：安装态在资源目录；开发态从可执行文件向上找。
fn extension_dir(app: &AppHandle) -> Option<std::path::PathBuf> {
    if let Ok(rd) = app.path().resource_dir() {
        let p = rd.join("extension");
        if p.is_dir() {
            return Some(p);
        }
    }
    let mut dir = std::env::current_exe().ok()?.parent()?.to_path_buf();
    for _ in 0..5 {
        dir = dir.parent()?.to_path_buf();
        let p = dir.join("extension");
        if p.is_dir() {
            return Some(p);
        }
    }
    None
}

// ---------- 任务与记录 ----------

#[tauri::command]
async fn tasks_active(state: State<'_, Arc<AppState>>) -> Result<Vec<db::DeliveryTask>, String> {
    with_db(state.inner(), |conn| db::task_list_active(conn))
}

#[tauri::command]
async fn tasks_recent(state: State<'_, Arc<AppState>>, limit: Option<i64>) -> Result<Vec<db::DeliveryTask>, String> {
    with_db(state.inner(), |conn| db::task_list_recent(conn, limit.unwrap_or(50)))
}

#[tauri::command]
async fn task_confirm(
    state: State<'_, Arc<AppState>>,
    app: AppHandle,
    task_id: String,
    content: String,
) -> Result<(), String> {
    tasks::user_confirm(state.inner(), &app, &task_id, content).await
}

#[tauri::command]
async fn task_cancel(state: State<'_, Arc<AppState>>, app: AppHandle, task_id: String) -> Result<(), String> {
    tasks::user_cancel(state.inner(), &app, &task_id).await
}

#[tauri::command]
async fn records_recent(state: State<'_, Arc<AppState>>, limit: Option<i64>) -> Result<Vec<db::AuditEvent>, String> {
    with_db(state.inner(), |conn| db::audit_list(conn, limit.unwrap_or(100)))
}

#[tauri::command]
async fn wipe_records(state: State<'_, Arc<AppState>>) -> Result<(), String> {
    let s = state.inner();
    with_db(s, |conn| {
        db::wipe_records(conn)?;
        db::audit(conn, None, "records_wiped", "用户清除了全部本机记录")?;
        Ok(())
    })
}
