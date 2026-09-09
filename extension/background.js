/**
 * Service Worker：桌面端 Native Messaging 与内容脚本之间的桥。
 * 不做业务决策；仅转发、维持连接、在唤醒时重建端口。
 *
 * 诊断日志前缀 [xda-sw]：内容脚本 → SW → nmhost 的消息链路定位用。
 */

const HOST_NAME = 'com.local.xianyu.deliveryassistant';
const TARGET_URL = 'https://seller.goofish.com/';

let port = null;
let connected = false;
let lastPaused = false;

function ensurePort(reason) {
  if (port) return port;
  console.log(`[xda-sw] connectNative reason=${reason}`);
  port = chrome.runtime.connectNative(HOST_NAME);
  connected = true;
  port.onMessage.addListener(onHostMessage);
  port.onDisconnect.addListener(() => {
    const err = chrome.runtime && chrome.runtime.lastError;
    console.warn(`[xda-sw] port DISCONNECTED lastError=${err ? err.message : '(none)'}`);
    port = null;
    connected = false;
    broadcast({ v: 1, type: 'connection', payload: { connected: false } });
    // 桌面端可能稍后可用：低频重连（SW 存活期内快速重试；休眠后由 alarm 兜底）
    setTimeout(() => ensurePort('post-disconnect'), 5000);
  });
  broadcast({ v: 1, type: 'connection', payload: { connected: true } });
  console.log('[xda-sw] port created, broadcasting connection=true');
  return port;
}

function onHostMessage(msg) {
  if (!msg || msg.v !== 1) return;
  console.log(`[xda-sw] host->sw msg type=${msg.type}`);
  switch (msg.type) {
    case 'pause_state':
      lastPaused = !!msg.payload?.paused;
      broadcast(msg);
      break;
    case 'approve_send':
      forwardApproveToBestTab(msg);
      break;
    case 'recheck':
      forwardToTabs(msg);
      break;
    default:
      break; // hello_ok 等桌面端确认消息不透传
  }
}

// 桌面端“批准发送”不能像状态广播那样发给所有标签页：
// 自动定位会点击会话行，若多个标签页并行会互相干扰甚至对同一订单重复发送。
// 因此先向每个卖家标签页做一次轻量探测（谁已打开目标会话 / 谁有会话列表），
// 把 approve_send 只投递给最合适的那一个；一个都没有则立即回执失败，不等看门狗。
async function forwardApproveToBestTab(msg) {
  const payload = msg.payload || {};
  const tabs = await chrome.tabs.query({ url: `${TARGET_URL}*` });
  if (!tabs.length) {
    postNativeResult(payload.task_id, false, '浏览器没有打开的卖家页面（请先打开 seller.goofish.com）');
    return;
  }
  const probe = { v: 1, type: 'approve_probe', payload: { task_id: payload.task_id, order_id: payload.order_id } };
  const results = await Promise.all(
    tabs.map(async (tab) => {
      try {
        const r = await chrome.tabs.sendMessage(tab.id, probe);
        const good = r && typeof r.score === 'number' && r.can;
        return { tab, r: good ? r : null };
      } catch (e) {
        return { tab, r: null }; // 无内容脚本（旧页面）或注入失败
      }
    })
  );
  let best = null;
  for (const x of results) {
    if (!x.r) continue;
    if (!best || x.r.score > best.r.score) best = x;
  }
  if (!best) {
    console.warn('[xda-sw] approve_send: 没有标签页可处理（需刷新一次聊天页让新版脚本注入）');
    postNativeResult(payload.task_id, false, '没有可自动定位的会话标签页；请刷新卖家聊天页后重试');
    return;
  }
  console.log(`[xda-sw] approve_send -> tab ${best.tab.id} score=${best.r.score}`);
  try {
    await chrome.tabs.sendMessage(best.tab.id, msg);
  } catch (e) {
    postNativeResult(payload.task_id, false, '目标标签页无响应，请刷新卖家聊天页后重试');
  }
}

/** 桌面端侧的回执（仅用于扩展侧兜底失败；真正的发送结果由内容脚本上报）。 */
function postNativeResult(task_id, ok, reason) {
  try {
    const p = ensurePort('approve-fallback');
    p.postMessage({ v: 1, type: 'send_result', payload: { task_id, ok, reason } });
  } catch (e) {
    console.error(`[xda-sw] postNativeResult failed: ${e && e.message}`);
  }
}

async function forwardToTabs(msg) {
  const tabs = await chrome.tabs.query({ url: `${TARGET_URL}*` });
  console.log(`[xda-sw] forwardToTabs type=${msg.type} tabs=${tabs.length}`);
  for (const tab of tabs) {
    chrome.tabs.sendMessage(tab.id, msg).catch(() => {});
  }
}

async function broadcast(msg) {
  const tabs = await chrome.tabs.query({ url: `${TARGET_URL}*` });
  for (const tab of tabs) {
    chrome.tabs.sendMessage(tab.id, msg).catch(() => {});
  }
}

// 内容脚本 → 桌面端
chrome.runtime.onMessage.addListener((msg, _sender, sendResponse) => {
  if (!msg || msg.v !== 1) return;
  const p = ensurePort('content-msg');
  try {
    if (msg.type === 'page_event' && lastPaused) {
      // 暂停期间只上报最小事件（不含商品信息），桌面端亦会丢弃
      msg.payload = { ...msg.payload, product_id: '', title: '' };
    }
    p.postMessage(msg);
    console.log(`[xda-sw] content->native ${msg.type} posted connected=${connected}`);
    sendResponse({ ok: true, connected });
  } catch (e) {
    console.error(`[xda-sw] content->native ${msg.type} EXCEPTION: ${e && e.message}`);
    connected = false;
    port = null;
    sendResponse({ ok: false, error: String(e) });
  }
  return false; // 同步应答
});

// Service Worker 冷启动时重建端口并请求内容脚本重扫
console.log(`[xda-sw] SW boot at ${new Date().toISOString()}`);
ensurePort('boot');
chrome.tabs.query({ url: `${TARGET_URL}*` }).then((tabs) => {
  for (const tab of tabs) chrome.tabs.sendMessage(tab.id, { v: 1, type: 'rescan' }).catch(() => {});
});

// MV3 的 setTimeout 随 Service Worker 休眠一起消失，桌面端重启后将永远连不上。
// 用 alarms 周期性唤醒 SW：醒来即尝试重建端口（已连接时是 no-op）。
chrome.alarms.create('reconnect', { periodInMinutes: 0.5 });
chrome.alarms.onAlarm.addListener((alarm) => {
  if (alarm.name === 'reconnect') {
    ensurePort('alarm');
  }
});
