//! 任务状态机与业务流程：识别 → 匹配 → 确认/自动发送 → 页面证据 → 终结。
//! 不变量：
//! - 确认模式下绝不在用户确认前发送。
//! - 自动模式遇任何不确定条件即降级为待确认。
//! - 发送完成的唯一判据是扩展回传的页面证据；超时不盲目重发。
//! - 任务终结时清空发货内容明文，仅保留摘要与脱敏审计。

use std::sync::Arc;
use std::time::Duration;

use serde_json::json;
use tauri::{AppHandle, Emitter, Manager};
use tauri_plugin_notification::NotificationExt;

use crate::db::{self, DeliveryTask};
use crate::feishu::{self, ProductEntry};
use crate::nm::{PageEventPayload, RecheckPayload, SendResultPayload};
use crate::redact;
use crate::state::{bitable_selection, feishu_app_credentials, with_db, AppState, Mode};

const PRODUCTS_TTL: Duration = Duration::from_secs(60);
const SEND_TIMEOUT: Duration = Duration::from_secs(180);
const RECHECK_TIMEOUT: Duration = Duration::from_secs(60);
/// 周期协调间隔：定时向页面回查挂起订单的最新状态（捕获手动发货等不触发 page_event 的变更）。
const RECONCILE_INTERVAL: Duration = Duration::from_secs(45);

pub const STATUS_DISCOVERED: &str = "已发现";
pub const STATUS_MATCHING: &str = "匹配中";
pub const STATUS_PENDING_CONFIRM: &str = "待确认";
pub const STATUS_PENDING_AUTO: &str = "待自动发送";
pub const STATUS_TO_SEND: &str = "待发送";
pub const STATUS_DONE: &str = "已完成";
pub const STATUS_ERROR: &str = "异常";
pub const STATUS_CANCELLED: &str = "已取消";
pub const STATUS_IGNORED: &str = "已忽略";

// ---------- 事件与通知 ----------

pub fn emit_tasks(app: &AppHandle) {
    let _ = app.emit("tasks-changed", ());
}

fn notify(app: &AppHandle, title: &str, body: &str) {
    let _ = app.notification().builder().title(title).body(body).show();
}

/// 让主窗口浮到前台（toast 可能被系统/通知中心吞掉，真正"弹出"还得靠窗口）。
fn raise_main(app: &AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.show();
        let _ = w.unminimize();
        let _ = w.set_focus();
    }
}

fn new_task_id() -> String {
    format!("t-{}", &redact::new_token()[..12])
}

// ---------- 商品匹配 ----------

pub enum MatchOutcome {
    Unique(ProductEntry),
    EmptyContent,
    NoMatch,
    Ambiguous(usize),
}

/// 匹配算法（system-design §4）：商品 ID 精确优先；仅当页面无 ID 时标题标准化精确兜底。
pub fn match_products(rows: &[ProductEntry], product_id: &str, title: &str) -> MatchOutcome {
    if !product_id.is_empty() {
        let hits: Vec<&ProductEntry> = rows
            .iter()
            .filter(|r| r.product_id.as_deref() == Some(product_id))
            .collect();
        return match hits.len() {
            1 if hits[0].delivery_content.is_empty() => MatchOutcome::EmptyContent,
            1 => MatchOutcome::Unique(hits[0].clone()),
            0 => MatchOutcome::NoMatch,
            n => MatchOutcome::Ambiguous(n),
        };
    }
    let norm = feishu::normalize_title(title);
    if norm.is_empty() {
        return MatchOutcome::NoMatch;
    }
    let hits: Vec<&ProductEntry> = rows
        .iter()
        .filter(|r| r.product_title_normalized.as_deref() == Some(norm.as_str()))
        .collect();
    match hits.len() {
        1 if hits[0].delivery_content.is_empty() => MatchOutcome::EmptyContent,
        1 => MatchOutcome::Unique(hits[0].clone()),
        0 => {
            // 严格精确 0 命中：再试"容忍末尾 ASCII 复数 s"的宽容键。
            // 例：卡片标题 …《Agent Skills》 对应库中 …《Agent Skill》。
            let lax = feishu::normalize_title_lax(title);
            if lax == norm {
                return MatchOutcome::NoMatch; // 标题没有可容的差异
            }
            let hits: Vec<&ProductEntry> = rows
                .iter()
                .filter(|r| {
                    r.product_title_normalized
                        .as_deref()
                        .map(|t| feishu::normalize_title_lax(t) == lax)
                        .unwrap_or(false)
                })
                .collect();
            match hits.len() {
                1 if hits[0].delivery_content.is_empty() => MatchOutcome::EmptyContent,
                1 => MatchOutcome::Unique(hits[0].clone()),
                0 => MatchOutcome::NoMatch,
                n => MatchOutcome::Ambiguous(n),
            }
        }
        n => MatchOutcome::Ambiguous(n),
    }
}

/// 获取商品库（带 TTL 缓存）；失败返回 Err。
/// 非 force 路径在网络失败且内存尚有旧缓存时回退旧缓存（避免飞书瞬时抖动挡住建单），
/// 而不是一律报错。
pub async fn refresh_products(state: &Arc<AppState>, force: bool) -> Result<Vec<ProductEntry>, String> {
    {
        let cache = state.products.lock().await;
        if !force {
            if let Some((at, rows)) = cache.as_ref() {
                if at.elapsed() < PRODUCTS_TTL {
                    return Ok(rows.clone());
                }
            }
        }
    }
    let fetched = async {
        let (app_id, app_secret) = feishu_app_credentials(state)?;
        let token = feishu::ensure_access_token(&state.http, &app_id, &app_secret, &state.data_dir).await?;
        let (raw_token, table_id, is_wiki) = bitable_selection(state)?;
        let app_token = feishu::resolve_app_token(&state.http, &token, &raw_token, is_wiki).await?;
        let rows = feishu::fetch_products(&state.http, &token, &app_token, &table_id).await?;
        Ok::<_, String>(rows)
    }
    .await;
    let rows = match fetched {
        Ok(rows) => rows,
        Err(e) => {
            if !force {
                // 网络失败时回退内存旧缓存（内容可能略旧，但比挡住建单好）
                let cache = state.products.lock().await;
                if let Some((_, rows)) = cache.as_ref() {
                    eprintln!("[products] 重读商品库失败，回退旧缓存: {e}");
                    return Ok(rows.clone());
                }
            }
            return Err(e);
        }
    };
    // 写入商品缓存表（仅摘要，不留明文）
    let cache_rows: Vec<db::ProductRow> = rows
        .iter()
        .map(|r| db::ProductRow {
            product_id: r.product_id.clone(),
            title_normalized: r.product_title_normalized.clone(),
            content_hash: if r.delivery_content.is_empty() {
                String::new()
            } else {
                redact::content_hash(&state.salt, &r.delivery_content)
            },
        })
        .collect();
    with_db(state, |conn| db::product_cache_replace_all(conn, &cache_rows))?;
    *state.products.lock().await = Some((std::time::Instant::now(), rows.clone()));
    Ok(rows)
}

// ---------- 页面事件 → 任务 ----------

pub async fn handle_page_event(state: &Arc<AppState>, app: &AppHandle, p: PageEventPayload) {
    if state.paused_now() {
        with_db(state, |conn| {
            db::audit(conn, None, "paused_ignored", &format!("订单 {} 暂停期间忽略", p.order_id))
        })
        .ok();
        return;
    }
    if !p.status.contains("等待卖家发货") {
        return; // 非目标状态的高频页面事件，直接忽略
    }
    let has_id = !p.product_id.is_empty();
    let has_title = !p.title.is_empty();
    if !has_id && !has_title {
        with_db(state, |conn| {
            db::audit(conn, None, "unidentifiable", &format!("订单 {} 缺少商品 ID 与标题", p.order_id))
        })
        .ok();
        notify(app, "无法识别商品", "该订单缺少商品 ID 与标题，未创建任务。");
        return;
    }
    let product_key = if has_id {
        format!("pid:{}", p.product_id)
    } else {
        format!("title:{}", feishu::normalize_title(&p.title))
    };

    // 幂等去重
    let idem_key = match with_db(state, |conn| db::task_dedup_check(conn, &p.order_id, &product_key)) {
        Ok(db::DedupVerdict::Existing(t)) => {
            // 同一 订单+商品 的其它活跃残留（历史去重缺陷堆积）一并合并掉
            with_db(state, |conn| {
                db::task_collapse_duplicates(conn, &p.order_id, &product_key, &t.id)
            })
            .ok();
            emit_tasks(app);
            return;
        }
        Ok(db::DedupVerdict::Completed) => {
            with_db(state, |conn| {
                db::audit(conn, None, "dedup_skip", &format!("订单 {} 已完成过发货，忽略重复事件", p.order_id))
            })
            .ok();
            return;
        }
        Ok(db::DedupVerdict::Create(key)) => key,
        Err(e) => {
            with_db(state, |conn| db::audit(conn, None, "dedup_error", &format!("订单 {} 去重失败: {e}", p.order_id))).ok();
            return;
        }
    };

    // 飞书匹配；基础设施错误不创建任务，只记录并通知
    let rows = match refresh_products(state, false).await {
        Ok(rows) => rows,
        Err(e) => {
            with_db(state, |conn| {
                db::audit(conn, None, "feishu_error", &format!("订单 {} 匹配失败: {e}", p.order_id))
            })
            .ok();
            notify(app, "飞书读取失败", "暂时无法读取商品库，未创建发货任务；恢复后将自动重试。");
            return;
        }
    };
    let outcome = match_products(&rows, p.product_id.as_str(), p.title.as_str());
    let (status, content, fail_reason) = match outcome {
        MatchOutcome::Unique(hit) => {
            let content = hit.delivery_content;
            match state.mode_now() {
                Mode::Confirm => (STATUS_PENDING_CONFIRM, content, None),
                Mode::Auto => (STATUS_PENDING_AUTO, content, None),
            }
        }
        MatchOutcome::EmptyContent => (STATUS_ERROR, String::new(), Some("匹配到唯一商品但发货内容为空".to_string())),
        MatchOutcome::NoMatch => (
            STATUS_ERROR,
            String::new(),
            Some(if has_id {
                format!("商品 ID {} 在飞书中无匹配", p.product_id)
            } else {
                format!("标题「{}」在飞书中无唯一匹配", p.title)
            }),
        ),
        MatchOutcome::Ambiguous(n) => (STATUS_ERROR, String::new(), Some(format!("飞书中命中 {n} 行，需先消除重复"))),
    };

    let task = DeliveryTask {
        id: new_task_id(),
        idempotency_key: idem_key,
        order_or_conversation_id: p.order_id.clone(),
        product_id: has_id.then(|| p.product_id.clone()),
        product_title: has_title.then(|| p.title.clone()),
        delivery_content: (!content.is_empty()).then_some(content.clone()),
        delivery_content_hash: (!content.is_empty()).then(|| redact::content_hash(&state.salt, &content)),
        mode: state.mode_now().as_str().into(),
        status: status.to_string(),
        fail_reason: fail_reason.clone(),
        created_at: db::now(),
        completed_at: None,
    };
    let task_id = task.id.clone();
    let matched = status == STATUS_PENDING_CONFIRM || status == STATUS_PENDING_AUTO;
    let is_auto = status == STATUS_PENDING_AUTO;
    with_db(state, |conn| {
        db::task_insert(conn, &task)?;
        db::audit(
            conn,
            Some(&task_id),
            if matched { "task_created" } else { "task_anomaly" },
            &format!(
                "订单 {} 商品({}) 状态 {}{}",
                p.order_id,
                product_key,
                status,
                fail_reason.as_deref().map(|r| format!(" 原因: {r}")).unwrap_or_default()
            ),
        )?;
        Ok(())
    })
    .ok();
    emit_tasks(app);
    if matched {
        raise_main(app); // 真正把任务弹到眼前（前台窗口展示待确认任务）
        notify(
            app,
            if is_auto { "自动发货待执行" } else { "新待确认发货" },
            "检测到等待发货的订单并完成唯一匹配，请处理。",
        );
        if is_auto {
            spawn_recheck(state.clone(), app.clone(), task_id);
        }
    } else {
        notify(app, "发货匹配异常", fail_reason.as_deref().unwrap_or("未知原因"));
    }
}

// ---------- 二次校验（自动模式） ----------

pub fn spawn_recheck(state: Arc<AppState>, app: AppHandle, task_id: String) {
    tauri::async_runtime::spawn(async move {
        let task = with_db(&state, |conn| db::task_get(conn, &task_id));
        let Ok(Some(task)) = task else { return };
        if task.status != STATUS_PENDING_AUTO {
            return;
        }
        let send_res = state
            .nm
            .send(
                "recheck",
                &json!({
                    "task_id": task.id,
                    "order_id": task.order_or_conversation_id,
                    "product_id": task.product_id.clone().unwrap_or_default(),
                    "title": task.product_title.clone().unwrap_or_default(),
                }),
            )
            .await;
        if send_res.is_err() {
            degrade_to_confirm(&state, &app, &task_id, "扩展未连接，自动发送降级为人工确认");
            return;
        }
        tokio::time::sleep(RECHECK_TIMEOUT).await;
        // 超时看门狗：仍在待自动发送则降级
        let still = with_db(&state, |conn| db::task_get(conn, &task_id)).ok().flatten();
        if let Some(t) = still {
            if t.status == STATUS_PENDING_AUTO {
                degrade_to_confirm(&state, &app, &task_id, "二次校验超时，自动发送降级为人工确认");
            }
        }
    });
}

fn degrade_to_confirm(state: &Arc<AppState>, app: &AppHandle, task_id: &str, reason: &str) {
    with_db(state, |conn| {
        db::task_update(
            conn,
            task_id,
            &db::TaskUpdate {
                status: Some(STATUS_PENDING_CONFIRM.into()),
                mode: Some("confirm".into()),
                ..Default::default()
            },
        )?;
        db::audit(conn, Some(task_id), "auto_degraded", reason)?;
        Ok(())
    })
    .ok();
    notify(app, "自动发送已降级", reason);
    emit_tasks(app);
}

pub async fn handle_recheck_result(state: &Arc<AppState>, app: &AppHandle, p: RecheckPayload) {
    let task = with_db(state, |conn| db::task_get(conn, &p.task_id)).ok().flatten();
    let Some(task) = task else { return };
    if task.status != STATUS_PENDING_AUTO && task.status != STATUS_PENDING_CONFIRM {
        return; // 发送中/已终结的任务忽略周期回查
    }
    // 订单级门槛：页面确认“当前会话就是该订单”才允许下结论。
    // “未知”（看不到目标订单）一律不动作，避免页面水合/滚动间隙误判。
    if !p.order_seen {
        return;
    }
    if p.status.contains("等待卖家发货") {
        // 订单仍等待发货：待确认保持不动；待自动发送核对商品一致后自动发送
        if task.status == STATUS_PENDING_AUTO {
            // 商品一致性：ID 或标准化标题必须与任务一致
            let consistent = if let Some(pid) = &task.product_id {
                p.product_id == *pid
            } else if let Some(t) = &task.product_title {
                feishu::normalize_title(&p.title) == feishu::normalize_title(t)
            } else {
                false
            };
            if !consistent {
                degrade_to_confirm(state, app, &p.task_id, "二次校验发现商品信息变化，降级为人工确认");
                return;
            }
            approve_send(state, app, &p.task_id, None).await;
        }
        return;
    }
    // 订单级“已变化”：卖家很可能已手动发货 → 自动终结，不再当作待发货处理。
    // 周期回查正是为了捕获“手动发货不产生页面事件”这类场景。
    let (final_status, event_type, reason, note_title, note_body) = if task.status == STATUS_PENDING_AUTO {
        (STATUS_IGNORED, "auto_ignored", "订单状态已变化，疑似卖家已手动发货", "自动发送已终止", "检测到订单已发货（可能手动发货），已停止自动发送。")
    } else {
        (STATUS_CANCELLED, "auto_cancelled_ship", "检测到订单已发货（可能手动发货），自动取消待确认", "待确认已自动取消", "检测到订单已发货（可能手动发货），已自动取消该待确认。")
    };
    with_db(state, |conn| {
        db::task_finalize(conn, &p.task_id, final_status, None, Some(reason))?;
        db::audit(conn, Some(&p.task_id), event_type, reason)?;
        Ok(())
    })
    .ok();
    emit_tasks(app);
    notify(app, note_title, note_body);
}

/// 周期协调：对仍挂起的 待确认/待自动发送 任务定期向页面回查订单状态。
/// 用途：卖家可能在应用之外手动发货，聊天卡从“等待卖家发货”变成已发货/消失，
/// 这类变更不产生 page_event；本循环让桌面端在下一轮发现并把过期任务自动终结
/// （由 handle_recheck_result 依据订单级结论处理，此处只负责发请求）。
pub fn spawn_reconcile_ticker(state: Arc<AppState>) {
    tauri::async_runtime::spawn(async move {
        loop {
            tokio::time::sleep(RECONCILE_INTERVAL).await;
            if state.paused_now() {
                continue;
            }
            let tasks = with_db(&state, |conn| db::task_list_active(conn)).unwrap_or_default();
            for t in tasks {
                if t.status != STATUS_PENDING_CONFIRM && t.status != STATUS_PENDING_AUTO {
                    continue;
                }
                let _ = state
                    .nm
                    .send(
                        "recheck",
                        &json!({
                            "task_id": t.id,
                            "order_id": t.order_or_conversation_id,
                            "product_id": t.product_id.clone().unwrap_or_default(),
                            "title": t.product_title.clone().unwrap_or_default(),
                        }),
                    )
                    .await;
            }
        }
    });
}

// ---------- 发送批准与结果 ----------

pub async fn approve_send(state: &Arc<AppState>, app: &AppHandle, task_id: &str, content_override: Option<String>) {
    let task = match with_db(state, |conn| db::task_get(conn, task_id)) {
        Ok(Some(t)) => t,
        Ok(None) => {
            with_db(state, |conn| db::audit(conn, Some(task_id), "approve_skipped", "批准发送但任务不存在")).ok();
            eprintln!("[approve_send] 任务不存在: {task_id}");
            return;
        }
        Err(e) => {
            eprintln!("[approve_send] task_get 失败: {e}");
            return;
        }
    };
    if task.status != STATUS_PENDING_CONFIRM && task.status != STATUS_PENDING_AUTO {
        with_db(state, |conn| {
            db::audit(conn, Some(task_id), "approve_skipped", &format!("任务状态 {} 不可发送", task.status))
        })
        .ok();
        eprintln!("[approve_send] 跳过：任务状态 {}", task.status);
        return;
    }
    let content = content_override.or(task.delivery_content.clone()).unwrap_or_default();
    if content.trim().is_empty() {
        with_db(state, |conn| {
            db::task_finalize(conn, task_id, STATUS_ERROR, None, Some("发货内容为空，拒绝发送"))?;
            db::audit(conn, Some(task_id), "send_rejected", "发货内容为空，拒绝发送")?;
            Ok(())
        })
        .ok();
        emit_tasks(app);
        return;
    }
    let token = redact::new_token();
    state
        .pending_approvals
        .lock()
        .expect("approvals 锁")
        .insert(task_id.to_string(), token.clone());
    let hash = redact::content_hash(&state.salt, &content);
    let db_res = with_db(state, |conn| {
        db::task_update(
            conn,
            task_id,
            &db::TaskUpdate {
                status: Some(STATUS_TO_SEND.into()),
                delivery_content: Some(Some(content.clone())),
                delivery_content_hash: Some(Some(hash)),
                ..Default::default()
            },
        )?;
        db::audit(conn, Some(task_id), "send_approved", "用户批准发送，等待页面执行结果")?;
        Ok(())
    });
    if let Err(e) = db_res {
        // 批准状态入库失败就不能外发：否则扩展收到“批准”却在桌面端无记录
        state.pending_approvals.lock().expect("approvals 锁").remove(task_id);
        eprintln!("[approve_send] 状态入库失败: {e}");
        with_db(state, |conn| {
            db::audit(conn, Some(task_id), "approve_db_error", &format!("批准发送但任务状态入库失败: {e}"))
        })
        .ok();
        emit_tasks(app);
        notify(app, "发送批准失败", "无法更新任务状态，未能发送，请重试。");
        return;
    }
    emit_tasks(app);
    // order_id 供扩展自动定位到正确会话（用户无需预先点开该聊天窗）。
    let sent = state.nm.send(
        "approve_send",
        &json!({
            "task_id": task_id,
            "approval_token": token,
            "content": content,
            "order_id": task.order_or_conversation_id,
        }),
    ).await;
    if let Err(e) = sent {
        state.pending_approvals.lock().expect("approvals 锁").remove(task_id);
        with_db(state, |conn| {
            db::task_finalize(conn, task_id, STATUS_ERROR, None, Some(&format!("扩展未连接: {e}")))?;
            db::audit(conn, Some(task_id), "send_failed", "批准发送但扩展未连接")?;
            Ok(())
        })
        .ok();
        emit_tasks(app);
        notify(app, "发送失败", "浏览器扩展未连接，请检查扩展状态后重试。");
        return;
    }
    // 发送超时看门狗：超时视为失败，不盲目重发
    let st = state.clone();
    let ap = app.clone();
    let tid = task_id.to_string();
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(SEND_TIMEOUT).await;
        let still = with_db(&st, |conn| db::task_get(conn, &tid)).ok().flatten();
        if let Some(t) = still {
            if t.status == STATUS_TO_SEND {
                st.pending_approvals.lock().expect("approvals 锁").remove(&tid);
                with_db(&st, |conn| {
                    db::task_finalize(conn, &tid, STATUS_ERROR, None, Some("发送结果确认超时"))?;
                    db::audit(conn, Some(&tid), "send_timeout", "发送结果确认超时，未自动重发")?;
                    Ok(())
                })
                .ok();
                emit_tasks(&ap);
                notify(&ap, "发送未确认", "页面迟迟未回传发送结果，任务标记为异常，请人工处理。");
            }
        }
    });
}

pub async fn handle_send_result(state: &Arc<AppState>, app: &AppHandle, p: SendResultPayload) {
    // 校验一次性批准令牌：未批准任务的"发送结果"一律拒绝
    let expected = state.pending_approvals.lock().expect("approvals 锁").remove(&p.task_id);
    if expected.is_none() {
        let detail = if p.reason.trim().is_empty() {
            "收到未批准任务的发送结果，已拒绝".to_string()
        } else {
            format!("收到未批准任务的发送结果，已拒绝（扩展原因: {}）", p.reason)
        };
        with_db(state, |conn| db::audit(conn, Some(&p.task_id), "send_result_unauthorized", &detail)).ok();
        return;
    }
    let task = with_db(state, |conn| db::task_get(conn, &p.task_id)).ok().flatten();
    let Some(task) = task else { return };
    if task.status != STATUS_TO_SEND {
        return;
    }
    if p.ok {
        with_db(state, |conn| {
            db::task_finalize(conn, &p.task_id, STATUS_DONE, task.delivery_content_hash.as_deref(), None)?;
            db::audit(conn, Some(&p.task_id), "send_completed", &format!("页面确认发送成功，证据: {}", p.evidence))?;
            Ok(())
        })
        .ok();
        notify(app, "发货成功", "发货内容已发送并被页面确认。");
    } else {
        with_db(state, |conn| {
            db::task_finalize(conn, &p.task_id, STATUS_ERROR, None, Some(&format!("页面发送失败: {}", p.reason)))?;
            db::audit(conn, Some(&p.task_id), "send_failed", &format!("页面发送失败: {}", p.reason))?;
            Ok(())
        })
        .ok();
        notify(app, "发货失败", "页面未确认发送成功，任务标记为异常，请人工处理。");
    }
    emit_tasks(app);
}

// ---------- 用户操作 ----------

pub async fn user_confirm(state: &Arc<AppState>, app: &AppHandle, task_id: &str, edited_content: String) -> Result<(), String> {
    let content = edited_content.trim().to_string();
    if content.is_empty() {
        return Err("发货内容不能为空".into());
    }
    with_db(state, |conn| db::audit(conn, Some(task_id), "confirm_requested", "收到确认发送请求")).ok();
    approve_send(state, app, task_id, Some(content)).await;
    Ok(())
}

pub async fn user_cancel(state: &Arc<AppState>, app: &AppHandle, task_id: &str) -> Result<(), String> {
    let task = with_db(state, |conn| db::task_get(conn, task_id))?.ok_or("任务不存在")?;
    if task.status != STATUS_PENDING_CONFIRM {
        return Err(format!("状态 {} 不可取消", task.status));
    }
    with_db(state, |conn| {
        db::task_finalize(conn, task_id, STATUS_CANCELLED, None, Some("用户取消"))?;
        db::audit(conn, Some(task_id), "task_cancelled", "用户取消任务")?;
        Ok(())
    })?;
    emit_tasks(app);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(id: &str, title: &str, content: &str) -> ProductEntry {
        ProductEntry {
            record_id: id.into(),
            product_id: Some(format!("pid-{id}")),
            product_title: Some(title.into()),
            product_title_normalized: Some(feishu::normalize_title(title)),
            delivery_content: content.into(),
        }
    }

    #[test]
    fn product_id_match_takes_priority() {
        let rows = vec![row("1", "甲商品", "内容A"), row("2", "乙商品", "内容B")];
        match match_products(&rows, "pid-2", "甲商品") {
            MatchOutcome::Unique(h) => assert_eq!(h.delivery_content, "内容B"),
            _ => panic!("应按 ID 命中乙商品"),
        }
    }

    #[test]
    fn title_fallback_only_normalizes_whitespace() {
        let rows = vec![row("1", "Python  入门　课程", "内容A")];
        match match_products(&rows, "", " Python 入门 课程 ") {
            MatchOutcome::Unique(h) => assert_eq!(h.delivery_content, "内容A"),
            _ => panic!("标题兜底应标准化后命中"),
        }
    }

    #[test]
    fn zero_and_multi_match_are_not_sendable() {
        let rows = vec![row("1", "重复商品", "内容A"), row("2", "重复商品", "内容B")];
        assert!(matches!(match_products(&rows, "", "不存在"), MatchOutcome::NoMatch));
        assert!(matches!(match_products(&rows, "", "重复商品"), MatchOutcome::Ambiguous(2)));
    }

    #[test]
    fn id_no_match_does_not_fall_back_to_title() {
        // 页面取到了商品 ID 但飞书无此 ID：按规则不允许再用标题兜底
        let rows = vec![row("1", "甲商品", "内容A")];
        assert!(matches!(match_products(&rows, "pid-other", "甲商品"), MatchOutcome::NoMatch));
    }

    #[test]
    fn empty_content_flagged() {
        let rows = vec![row("1", "空内容商品", "")];
        assert!(matches!(match_products(&rows, "pid-1", ""), MatchOutcome::EmptyContent));
    }
}
