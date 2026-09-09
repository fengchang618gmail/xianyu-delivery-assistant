/**
 * 闲鱼发货助手内容脚本：仅在 seller.goofish.com 运行。
 * 职责：
 *  1) 观察页面可见状态，识别"等待卖家发货"订单并提取商品 ID/标题（尽力而为，DOM 变化需实测校准）；
 *  2) 收到桌面端一次性批准令牌后，执行"填入并发送"，并回传页面成功证据；
 *  3) 对桌面端二次校验请求，重新读取当前订单状态。
 * 边界：不抓包、不伪造请求、不绕过登录/验证；仅操作用户可见的输入框与按钮。
 */
(() => {
  'use strict';
  console.log(`[xda] boot url=${location.href} top=${window.top === window} frame=${location.pathname}`);

  const normalize = (v) => (v || '').replace(/\s+/g, ' ').trim();
  const PAUSE_KEY = 'xda_paused';

  let paused = false;
  let lastKey = '';
  let lastFpJson = '';
  let sending = false;
  let navLock = false; // 自动定位点开会话行期间为真；避免 scan 为错开的会话生成任务

  // ---------- 提取 ----------

  /** 从会话/订单区域提取商品 ID。优先数据属性，其次可见文本模式。 */
  function extractProductId(root) {
    const scope = root || document;
    // 常见模式：商品卡片或订单卡片上带 data-id / itemid
    const el = scope.querySelector('[data-itemid], [data-product-id], [itemid]');
    if (el) {
      const v = el.getAttribute('data-itemid') || el.getAttribute('data-product-id') || el.getAttribute('itemid');
      if (v) return v.trim();
    }
    const text = scope.body ? scope.body.innerText : '';
    const m = text.match(/商品\s*ID[：:]\s*([A-Za-z0-9_-]{4,})/i) || text.match(/宝贝\s*ID[：:]\s*([A-Za-z0-9_-]{4,})/i);
    return m ? m[1] : '';
  }

  /** 提取当前会话的商品标题（防御性多选择器，需真实页面校准）。 */
  function extractTitle() {
    const selectors = [
      '[class*="itemTitle"]',
      '[class*="item-title"]',
      '[class*="productTitle"]',
      '[class*="goodsTitle"]',
      '[class*="ItemHeader"] [class*="title"]',
      '[class*="orderCard"] [class*="title"]',
    ];
    for (const sel of selectors) {
      const el = document.querySelector(sel);
      const t = el && normalize(el.textContent);
      if (t) return t;
    }
    return '';
  }

  /** 订单/会话标识：优先 URL 参数，其次订单号文本。 */
  function extractOrderId() {
    try {
      const url = new URL(location.href);
      for (const k of ['orderId', 'order_id', 'itemId', 'item_id', 'bizOrderId', 'conversationId', 'chatId']) {
        const v = url.searchParams.get(k);
        if (v) return `${k}:${v}`;
      }
    } catch (_) { /* ignore */ }
    const m = (document.body.innerText || '').match(/订单号[：:]\s*(\d{6,})/);
    return m ? `order:${m[1]}` : `page:${location.pathname}${location.search}`;
  }

  /** 在已打开的聊天窗中找"待发货/未发货"订单卡。
   *  真实卡片文本形如：
   *    《学习观：从感觉懂了到真正学会》于建国著 ¥ 1.88 [共1件]
   *    成交价 ¥ 1.88 发货状态 未发货 收货信息 … 去发货 取消订单
   *  标题 = 卡片文本第一个 ¥ 之前的行首部分。 */
  function chatPendingOrder() {
    // 与 consoleProbe 中验证过的逻辑一致：从每个"去发货"叶子按钮向上爬出所属订单卡。
    // 卡有紧凑形态（我已付款，等待你发货…去发货）与完整形态（行首是商品标题，含两处 ¥），
    // 匹配完整卡拿标题。
    const seen = new Set();
    const cards = [];
    document.querySelectorAll('div,span,a,button').forEach((el) => {
      const t = ((el.innerText || el.textContent) || '').trim();
      if (el.children.length || !/^(去发货)$/.test(t)) return;
      let card = null;
      for (let cur = el.parentElement, i = 0; cur && cur !== document.body && i < 7; cur = cur.parentElement, i++) {
        const ct = (cur.innerText || '').replace(/\s+/g, ' ');
        if (/¥|订单|发货|收货/.test(ct) && ct.length >= 15 && ct.length <= 600) { card = cur; break; }
      }
      if (!card || seen.has(card)) return;
      seen.add(card);
      cards.push(normalize(card.innerText));
    });
    for (const txt of cards) {
      // 完整商品卡 = 商品价行 + "成交价 ¥" 行 → ≥2 个 ¥；只含下半部时标题会截错
      if ((txt.match(/[¥￥]/g) || []).length < 2) continue;
      if (!/发货状态/.test(txt) || !/未发货|待发货/.test(txt)) continue;
      const cut = txt.search(/[¥￥]/);
      let title = cut > 0 ? txt.slice(0, cut).trim() : '';
      if (title && /收货|订单|发货|备注|等待|已付款|成交价|发货状态/.test(title)) title = '';
      if (title && title.length < 2) title = '';
      if (!title) continue;
      const bodyText = document.body ? document.body.innerText : '';
      const oidM = bodyText.match(/订单编号\s*(\d+)/) || bodyText.match(/\b\d{15,}\b/);
      return { title, order_id: oidM ? oidM[1] : '', status: '等待卖家发货' };
    }
    return null;
  }

  /** DOM 健康检查：聊天输入框与发送按钮是否存在。 */
  function domHealth() {
    const input = findChatInput();
    const sendBtn = findSendButton();
    return { healthy: !!(input && sendBtn), detail: `input=${!!input} sendBtn=${!!sendBtn}` };
  }

  function findChatInput() {
    return (
      document.querySelector('[class*="chatInput"] textarea') ||
      document.querySelector('[class*="chat"] textarea') ||
      document.querySelector('textarea[placeholder], textarea') ||
      document.querySelector('[contenteditable="true"]')
    );
  }

  function findSendButton() {
    const candidates = document.querySelectorAll('button');
    for (const b of candidates) {
      const t = normalize(b.textContent);
      if (t === '发送' || /^发\s*送$/.test(t)) return b;
    }
    return document.querySelector('[class*="send"] button, button[class*="send"]');
  }

  // ---------- 页面结构探查（走 Console，不脱敏、无长度限制；仅本地输出供人工复制） ----------

  let lastProbeAt = 0;
  let lastProbeHref = '';
  /** 页面结构探查（Console 输出，人工复制用）：围绕每个"等待发货"标签看其所在区块
   *  的真实文字与类名，并清点输入框形态。不上报审计。 */
  function consoleProbe() {
    const now = Date.now();
    if (location.href === lastProbeHref && now - lastProbeAt < 6000) return;
    lastProbeAt = now;
    lastProbeHref = location.href;

    // 清点所有可见输入区（textarea/input/contenteditable/富文本）
    const inputs = [];
    document.querySelectorAll('textarea, input:not([type=hidden]), [contenteditable="true"]').forEach((el) => {
      if (!(el.offsetParent !== null || el.getClientRects().length)) return; // 只看可见的
      inputs.push({
        t: el.tagName,
        c: String(el.className || el.getAttribute('class') || '').slice(0, 70),
        ph: (el.getAttribute && el.getAttribute('placeholder')) ? String(el.getAttribute('placeholder')).slice(0, 30) : '',
        ce: el.isContentEditable,
      });
    });

    // 每个"等待发货"标签所在区块的真实文字（判定是列表行还是聊天内订单卡）
    const chips = [];
    document.querySelectorAll('[class*="order-wait"],[class*="wait"]').forEach((chipEl) => {
      if (chips.length >= 3) return;
      const ctxt = String(chipEl.className || '');
      if (!ctxt.includes('wait')) return;
      // 向上找文字量适中的容器（列表行≈几十字；聊天订单卡≈几百字）
      let ctx = null;
      for (let a = chipEl.parentElement, i = 0; a && a !== document.body && i < 7; a = a.parentElement, i++) {
        const t = (a.innerText || '').replace(/\s+/g, ' ').trim();
        if (t.length >= 6 && t.length <= 600) {
          ctx = { cls: String(a.className || '').slice(0, 90), txt: t.slice(0, 240) };
          break;
        }
      }
      chips.push({ cls: ctxt.slice(0, 60), ctx });
    });

    // 页面主要文本块（去重后取最长的 8 个，用于发现商品标题在哪个容器）
    const textBlocks = [];
    const seenT = new Set();
    document.querySelectorAll('div,section,p,span,h1,h2,h3,a').forEach((el) => {
      if (el.children.length > 4) return; // 只要叶子级文本块
      const t = (el.innerText || '').replace(/\s+/g, ' ').trim();
      if (t.length >= 10 && t.length <= 120 && !seenT.has(t)) {
        seenT.add(t);
        textBlocks.push({ cls: String(el.className || '').slice(0, 80), txt: t });
      }
    });
    textBlocks.sort((a, b) => b.txt.length - a.txt.length);

    // 会话行是否带 <a> 链接：决定点击后跳到哪（新标签/新路由）
    const rows = [];
    document.querySelectorAll('[class*="conversation-item"]').forEach((row) => {
      if (rows.length >= 6) return;
      const a = row.closest('a') || row.querySelector('a');
      rows.push({
        cls: String(row.className || '').slice(0, 60),
        txt: (row.innerText || '').replace(/\s+/g, ' ').trim().slice(0, 60),
        hasA: !!a,
        aHref: a ? (a.href || '').slice(0, 200) : '',
        aTarget: a ? (a.target || '') : '',
      });
    });

    // 聊天窗深探：有聊天 textarea 时，逐张订单卡（head→整卡）dump 结构 + 发送按钮
    const chatTa = [...document.querySelectorAll('textarea')].find((ta) => {
      const ph = (ta.getAttribute('placeholder') || '');
      return /请输入消息|Enter|发送/.test(ph) || (ta.offsetParent !== null && ta.offsetWidth > 200);
    });
    let chat = null;
    if (chatTa) {
      // 发送按钮：聊天 textarea 向上 3 层内的 button
      const sbtns = [];
      let btnRoot = chatTa;
      for (let i = 0; i < 3 && btnRoot.parentElement; i++) btnRoot = btnRoot.parentElement;
      btnRoot.querySelectorAll('button').forEach((b) => {
        sbtns.push({ txt: (b.innerText || '').trim().slice(0, 16), c: String(b.className || '').slice(0, 60), dis: b.disabled });
      });
      // 每张订单卡：最深含"订单编号"的容器做起点，向上取含 待发货/已发货 的=head，
      // 再向上爬首个同时含"发货状态"+"成交价"的祖先 = 整卡（含商品标题块/缩略图）。
      const allDivs = [...document.querySelectorAll('div,section')];
      const heads0 = allDivs.filter((el) => {
        const t = (el.innerText || '');
        if (!t.includes('订单编号')) return false;
        return ![...el.children].some((ch) => (ch.innerText || '').includes('订单编号'));
      });
      const orders = [];
      for (const hd0 of heads0.slice(0, 3)) {
        // 起点向上找到含状态的头部行
        let hd = hd0;
        for (let cur = hd0.parentElement, i = 0; cur && cur !== document.body && i < 4; cur = cur.parentElement, i++) {
          if (/待发货|已发货/.test(cur.innerText || '')) { hd = cur; break; }
        }
        const headTxt = (hd.innerText || '').replace(/\s+/g, ' ').trim().slice(0, 160);
        const oidM = (hd0.innerText || hd.innerText || '').match(/订单编号\s*(\d+)/);
        let card = null;
        for (let cur = hd.parentElement, i = 0; cur && cur !== document.body && i < 6; cur = cur.parentElement, i++) {
          const ct = (cur.innerText || '');
          if (/发货状态/.test(ct) && /成交价|实付/.test(ct)) { card = cur; break; }
        }
        const cardTxt = card ? (card.innerText || '').replace(/\s+/g, ' ').trim() : '';
        const imgs = [];
        if (card) card.querySelectorAll('img').forEach((im) => {
          imgs.push({ alt: (im.getAttribute('alt') || '').trim().slice(0, 40), w: im.offsetWidth || 0, src: (im.getAttribute('src') || '').split('/')[2] || '' });
        });
        orders.push({
          oid: oidM ? oidM[1] : '',
          head: headTxt,
          cardCls: card ? String(card.className || '').slice(0, 70) : '',
          cardTxt: cardTxt.slice(0, 480),
          imgs: imgs.slice(0, 4),
        });
      }
      // 补充：订单卡常以"去发货/已付款"动作按钮的紧凑形态渲染（无"订单编号"整卡）。
      // 从动作按钮反查所属订单卡，抓商品标题/图。
      const shipCards = [];
      const seenCard = new Set();
      document.querySelectorAll('div,span,a,button').forEach((el) => {
        const t = ((el.innerText || el.textContent) || '').trim();
        if (el.children.length || !/^(去发货|已付款)$/.test(t)) return;
        let card = null;
        for (let cur = el.parentElement, i = 0; cur && cur !== document.body && i < 7; cur = cur.parentElement, i++) {
          const ct = (cur.innerText || '').replace(/\s+/g, ' ');
          if (/¥|订单|发货|收货/.test(ct) && ct.length >= 15 && ct.length <= 500) { card = cur; break; }
        }
        if (!card || seenCard.has(card)) return;
        seenCard.add(card);
        const imgs = [...card.querySelectorAll('img')].map((im) => ({
          alt: (im.getAttribute('alt') || '').trim().slice(0, 40),
          w: im.offsetWidth || 0,
          src: (im.getAttribute('src') || '').split('/')[2] || '',
        }));
        shipCards.push({ btn: t, cls: String(card.className || '').slice(0, 70), txt: (card.innerText || '').replace(/\s+/g, ' ').trim().slice(0, 480), imgs: imgs.slice(0, 4) });
        if (shipCards.length >= 3) return;
      });
      chat = { taCls: String(chatTa.className || '').slice(0, 80), orders, shipCards, sbtns };
    }

    const out = {
      href: location.href.slice(0, 260),
      title: (document.title || '').slice(0, 50),
      bodyLen: (document.body ? document.body.innerText : '').length,
      inputs,
      rows: rows.slice(0, 4),
      chat,
    };
    console.log('[xda] PROBE ' + JSON.stringify(out));
  }

  // ---------- 事件上报 ----------

  function reportPageEvent(extra) {
    const health = domHealth();
    const payload = Object.assign(
      {
        order_id: extractOrderId(),
        conversation_id: '',
        product_id: '',
        title: '',
        status: '',
        page_url: location.href,
        dom_healthy: health.healthy,
      },
      extra || {}
    );
    chrome.runtime.sendMessage({ v: 1, type: 'page_event', payload }).catch(() => {});
  }

  /** css-module 词汇指纹（校准时用）：枚举与"等待卖家发货"状态标签同组件的元素。
   *  只报标签/类名/文本长度/是否含图（均 ASCII 或计数，不含聊天文本，脱敏安全）。 */
  function orderFingerprint() {
    // 状态标签最小容器（文本恰为"等待卖家发货"）
    let chip = document.body;
    while (chip && chip.children.length) {
      const kids = [...chip.children].filter((k) => (k.textContent || '').includes('等待卖家发货'));
      if (kids.length === 1) chip = kids[0];
      else break;
    }
    const chipCls = chip && chip !== document.body ? String(chip.className) : '';
    // css-module 哈希：取类名里最后一个 --后缀（同组件共享）；退化为末段 token
    let token = '';
    const m = chipCls.match(/(--[A-Za-z0-9]+)$/) || chipCls.match(/([A-Za-z0-9_]{6,})$/);
    if (m) token = m[1];
    const members = [];
    if (token) {
      document.querySelectorAll(`[class*="${token}"]`).forEach((el) => {
        if (members.length >= 22) return;
        const cls = String(el.className);
        if (el === chip) return;
        members.push({
          t: el.tagName,
          c: cls.slice(0, 90),
          l: (el.textContent || '').length,
          img: !!el.querySelector('img'),
        });
      });
    }
    // 状态标签的父链类名（4 层）
    const chain = [];
    for (let a = chip && chip.parentElement, i = 0; a && a !== document.body && i < 4; a = a.parentElement, i++) {
      const cls = String(a.className);
      if (cls) chain.push(`${a.tagName}.${cls.slice(0, 90)}`);
    }
    return { chip: chipCls.slice(0, 80), token, members, chain };
  }

  function scan() {
    if (navLock) return; // 自动定位点开会话行期间不上报，避免为错开的会话生成任务
    const text = document.body ? document.body.innerText : '';
    // 触发面：1) 列表行出现"等待卖家发货"；2) 打开的聊天里有未发货订单卡
    //   （虚拟列表会把行移出 DOM，聊天内订单卡更可靠）。
    const pending = chatPendingOrder();
    if (!pending && !text.includes('等待卖家发货')) return;
    consoleProbe();
    if (paused) return;
    const productId = extractProductId();
    const title = pending ? pending.title : (extractTitle() || '');
    const orderId = pending ? pending.order_id : extractOrderId();
    const key = `${orderId}|${title}|等待卖家发货`;
    if (key === lastKey) return;
    lastKey = key;
    if (!title && !productId) {
      // 缺商品信息：上报 css-module 词汇指纹（仅结构，供桌面端校准提取逻辑）。
      // 按指纹去重——页面水合加深后指纹变化会再报一次。
      const fpJson = JSON.stringify(orderFingerprint());
      if (fpJson === lastFpJson) return;
      lastFpJson = fpJson;
      post({
        v: 1,
        type: 'status_report',
        payload: { logged_in: true, dom_healthy: true, detail: `F2 ${fpJson}` },
      });
      return;
    }
    reportPageEvent({ product_id: productId, title, order_id: orderId, status: '等待卖家发货' });
    console.log(`[xda] SEND_PAGE_EVENT title=${JSON.stringify(title)} order=${JSON.stringify(orderId)} via=${pending ? 'chatOrderCard' : 'listText'}`);
  }

  // ---------- 发送执行（仅持一次性批准令牌） ----------

  /** 轮询等待发送按钮从禁用态变为可点。React 会在输入框有值后重渲染并可能替换按钮节点，
   *  因此每次都重新 findSendButton。最多等约 4 秒；返回可点按钮或 null。 */
  function waitSendButtonEnabled() {
    return new Promise((resolve) => {
      let tries = 0;
      const timer = setInterval(() => {
        tries += 1;
        const btn = findSendButton();
        if (btn && !btn.disabled && !btn.hasAttribute('disabled')) {
          clearInterval(timer);
          resolve(btn);
          return;
        }
        if (tries >= 40) {
          clearInterval(timer);
          resolve(null);
        }
      }, 100);
    });
  }

  function setNativeValue(el, value) {
    const proto = el instanceof HTMLTextAreaElement ? HTMLTextAreaElement.prototype : HTMLInputElement.prototype;
    const setter = Object.getOwnPropertyDescriptor(proto, 'value')?.set;
    if (setter) setter.call(el, value);
    else el.value = value;
    el.dispatchEvent(new Event('input', { bubbles: true }));
  }

  async function executeSend(payload) {
    const { task_id: taskId, approval_token: token, content } = payload;
    if (!taskId || !token || typeof content !== 'string' || !content.trim()) return;
    if (sending) {
      chrome.runtime.sendMessage({
        v: 1, type: 'send_result',
        payload: { task_id: taskId, ok: false, reason: '上一笔发送尚未完成' },
      }).catch(() => {});
      return;
    }
    sending = true;
    finishWithinTimeout(taskId);
    try {
      const input = findChatInput();
      if (!input) throw new Error('未找到聊天输入框');
      input.focus();
      if (input.isContentEditable) {
        document.execCommand('selectAll', false, null);
        document.execCommand('insertText', false, content);
      } else {
        setNativeValue(input, content);
      }
      // 发送按钮初始多为禁用态（React 依据输入框是否有值启用）。填值后需轮询
      // 等待按钮可点再点击，否则命中 disabled 按钮会静默失败。
      const btn = await waitSendButtonEnabled();
      if (!btn) throw new Error('发送按钮迟迟未启用（填值未被页面接受？）');
      btn.click();
      watchForEvidence(taskId, content);
    } catch (e) {
      sending = false;
      chrome.runtime.sendMessage({
        v: 1, type: 'send_result',
        payload: { task_id: taskId, ok: false, reason: String(e.message || e) },
      }).catch(() => {});
    }
  }

  let evidenceTimer = null;
  function finishWithinTimeout(taskId) {
    clearTimeout(evidenceTimer);
    evidenceTimer = setTimeout(() => {
      if (!sending) return;
      sending = false;
      chrome.runtime.sendMessage({
        v: 1, type: 'send_result',
        payload: { task_id: taskId, ok: false, reason: '30 秒内未在页面看到已发送内容' },
      }).catch(() => {});
    }, 30000);
  }

  /** 成功证据：页面出现包含完整内容的新的外发消息节点。 */
  function watchForEvidence(taskId, content) {
    const target = normalize(content);
    const observer = new MutationObserver(() => {
      const nodes = document.querySelectorAll('[class*="msg"], [class*="message"], [class*="bubble"]');
      for (const n of nodes) {
        const t = normalize(n.textContent);
        if (t.includes(target)) {
          observer.disconnect();
          sending = false;
          clearTimeout(evidenceTimer);
          chrome.runtime.sendMessage({
            v: 1, type: 'send_result',
            payload: { task_id: taskId, ok: true, evidence: target.slice(0, 40) },
          }).catch(() => {});
          return;
        }
      }
    });
    observer.observe(document.documentElement, { childList: true, subtree: true });
  }

  // ---------- 自动定位目标会话（无需用户预先点开对应聊天窗） ----------
  //
  // 桌面端批准后，若当前打开的会话不是目标订单，就回到会话列表逐个点开会话、
  // 核对订单编号与目标一致后才填入并发送。核对成功才发——最多只是点开错的会话，
  // 绝不会把消息发给错误买家。全部失败时回传结构化原因，不让桌面端干等看门狗。

  const MAX_NAV_TRY = 4; // 最多点开几个会话核对
  const NAV_WAIT_MS = 4500; // 点击一个会话后等待其打开/水合

  function waitMs(ms) {
    return new Promise((resolve) => setTimeout(resolve, ms));
  }

  function sendResult(taskId, ok, reason) {
    const payload = ok
      ? { task_id: taskId, ok: true, evidence: '' }
      : { task_id: taskId, ok: false, reason: reason || '' };
    try {
      chrome.runtime.sendMessage({ v: 1, type: 'send_result', payload }).catch(() => {});
    } catch (e) {
      console.warn(`[xda] sendResult fail: ${e && e.message}`);
    }
  }

  /** 收集当前页可见的会话行（左侧列表）。排除右栏聊天气泡/消息节点，
   *  它们只是"长得像行"；虚拟列表只挂载可视区行，按 DOM 序（≈消息新旧序）。 */
  function collectConversationRows() {
    const rows = [];
    const seen = new Set();
    // 右栏会话区（含聊天输入框的祖先链）：其中的消息气泡不是可点的列表行
    let pane = null;
    const ta = findChatInput();
    if (ta) {
      pane = ta;
      for (let i = 0; pane && pane !== document.body && i < 7; i++) pane = pane.parentElement;
    }
    const pushRow = (el) => {
      if (!el || seen.has(el)) return;
      if (pane && pane.contains(el)) return; // 在聊天栏里 → 不是列表行
      seen.add(el);
      if (!(el.offsetParent !== null || el.getClientRects().length)) return; // 只留可见行
      const txt = normalize(el.innerText || '');
      if (txt.length < 8) return;
      rows.push(el);
    };
    // 会话行用经实测校准的 conversation-item 类；未知结构的页面宁可 rows=0
    // 走失败回执，也不要去误点订单/数据页里的通用列表项。
    const sel = '[class*="conversation-item"],[class*="conversationItem"]';
    document.querySelectorAll(sel).forEach(pushRow);
    // 兜底：列表行常含"等待卖家发货/待发货"状态 chip，从其向上推出行容器
    if (!rows.length) {
      document.querySelectorAll('[class*="order-wait"],[class*="wait"],[class*="orderStatus"],[class*="order-status"]').forEach((chip) => {
        const t = normalize(chip.textContent);
        if (t !== '等待卖家发货' && !t.includes('待发货') && !t.includes('等待发货')) return;
        for (let cur = chip.parentElement, i = 0; cur && cur !== document.body && i < 6; cur = cur.parentElement, i++) {
          const ct = normalize(cur.innerText || '');
          if (ct.length >= 8 && ct.length <= 300) { pushRow(cur); break; }
        }
      });
    }
    return rows;
  }

  /** 当前打开会话对应的订单号：优先订单卡解析，其次聊天区内文本。 */
  function readOpenOrderId() {
    const card = chatPendingOrder();
    if (card && card.order_id) return card.order_id;
    let paneText = '';
    const ta = findChatInput();
    if (ta) {
      let node = ta;
      for (let i = 0; node && node !== document.body && i < 6; i++) node = node.parentElement;
      paneText = (node && node.innerText) ? node.innerText : '';
      if (paneText.length < 30) paneText = document.body ? document.body.innerText : '';
    } else {
      paneText = document.body ? document.body.innerText : '';
    }
    const m = paneText.match(/订单编号\s*(\d{6,})/) || paneText.match(/\b\d{15,}\b/);
    return m ? m[1] : '';
  }

  /** 尽力点击会话行：同时派发指针与鼠标事件，覆盖 React 各类绑定。 */
  function clickRow(row) {
    const a = row.closest('a[href]') || row.querySelector('a[href]');
    const target = a || row;
    try {
      const opts = { bubbles: true, cancelable: true, composed: true, view: window };
      target.dispatchEvent(new PointerEvent('pointerdown', opts));
      target.dispatchEvent(new PointerEvent('pointerup', opts));
      target.dispatchEvent(new MouseEvent('mousedown', opts));
      target.dispatchEvent(new MouseEvent('mouseup', opts));
      target.dispatchEvent(new MouseEvent('click', opts));
    } catch (e) {
      try { target.click(); } catch (e2) { /* ignore */ }
    }
  }

  /** 点击一个会话行后，等待右侧会话打开（订单号出现 / 订单卡出现 / URL 变化）。 */
  async function waitConversationOpens(prev, ms) {
    const deadline = Date.now() + ms;
    while (Date.now() < deadline) {
      const oid = readOpenOrderId();
      const card = chatPendingOrder();
      if (oid && oid !== prev.oid) return { ok: true, oid, card };
      if (!prev.card && card) return { ok: true, oid, card };
      if (location.href !== prev.href && (oid || card)) return { ok: true, oid, card };
      await waitMs(200);
    }
    const oid = readOpenOrderId();
    const card = chatPendingOrder();
    return { ok: oid !== prev.oid || (!!card && !prev.card), oid, card };
  }

  /** 确保处于会话列表视图：若当前在无列表的全屏会话内，尝试 history.back() 返回。 */
  async function ensureListView() {
    if (collectConversationRows().length) return true;
    if (!findChatInput()) return false; // 连聊天输入框都没有 → 不是会话页
    try {
      history.back();
      await waitMs(1400);
    } catch (_) { /* ignore */ }
    return collectConversationRows().length > 0;
  }

  /** 轮询等待已打开的会话渲染出“待发货订单卡”（右侧订单面板水合有延迟）。 */
  async function waitOrderCard(ms) {
    const deadline = Date.now() + ms;
    while (Date.now() < deadline) {
      const c = chatPendingOrder();
      if (c) return c;
      await waitMs(250);
    }
    return chatPendingOrder();
  }

  /** 页面结构快照：失败回执与审计定位用（订单号等脱敏由桌面端统一处理）。 */
  function pageSurvey(target) {
    const rows = collectConversationRows();
    const rowTexts = rows.slice(0, 4).map((r) =>
      normalize(r.innerText || '').slice(0, 40) + (r.querySelector('a[href]') || r.closest('a') ? '(L)' : '')
    );
    const card = chatPendingOrder();
    return {
      target,
      rows: rows.length,
      hasInput: !!findChatInput(),
      hasCard: !!card,
      cardOrder: card ? card.order_id : '',
      chip: document.querySelectorAll('[class*="order-wait"]').length,
      url: location.href.slice(0, 140),
      rowsSample: rowTexts.join(' | '),
    };
  }

  /** 自动定位入口：目标会话已开则直接发送，否则逐会话核对订单号后发送。 */
  async function autoSendToOrder(payload) {
    const taskId = payload && payload.task_id;
    const content = payload && payload.content;
    if (!taskId || typeof content !== 'string' || !content.trim()) return;
    const target = String((payload && payload.order_id) || '').trim();
    if (navLock) {
      sendResult(taskId, false, '已有一次自动定位正在执行，请稍后重试');
      return;
    }
    navLock = true;
    try {
      // 0) 当前会话即目标待发货卡 → 直接发送（保持原有即时路径）
      const openCard = chatPendingOrder();
      if (openCard && (!target || openCard.order_id === target || readOpenOrderId() === target)) {
        console.log(`[xda] auto: 当前会话即目标订单 ${target}`);
        await executeSend(payload);
        return;
      }
      if (!target) {
        sendResult(taskId, false, '任务缺少订单号(order_id)，无法自动定位会话');
        return;
      }
      const survey = pageSurvey(target);
      console.log(`[xda] auto: 开始定位 ${JSON.stringify(survey)}`);
      if (!survey.hasInput && survey.rows === 0) {
        // 既不在会话内也无列表：立刻失败并给出页面结构，避免干等看门狗
        sendResult(taskId, false, `找不到会话列表或聊天输入框（rows=${survey.rows} chip=${survey.chip} url=${survey.url}）`);
        return;
      }
      await autoLocateAndSend(payload, target);
    } catch (e) {
      sendResult(taskId, false, '自动定位异常: ' + String((e && e.message) || e));
    } finally {
      navLock = false;
    }
  }

  /** 逐个点开会话、核对订单号；命中即发送，未命中继续，全部失败给失败回执。 */
  async function autoLocateAndSend(payload, target) {
    const taskId = payload.task_id;
    const started = Date.now();
    const tried = [];
    if (!(await ensureListView())) {
      sendResult(taskId, false, '无法进入会话列表视图，请刷新卖家聊天页后重试');
      return;
    }
    for (let attempt = 0; attempt < MAX_NAV_TRY; attempt++) {
      if (Date.now() - started > 24000) break; // 整体留余量，避免撞上桌面看门狗
      if (!(await ensureListView())) {
        sendResult(taskId, false, `已核对 ${tried.length} 个会话后无法返回列表，未找到订单 ${target}`);
        return;
      }
      const rows = collectConversationRows().filter((r) => {
        const k = normalize(r.innerText || '').slice(0, 40);
        return !tried.includes(k);
      });
      if (!rows.length) {
        sendResult(taskId, false, `已核对 ${tried.length} 个会话后无更多候选行，未找到订单 ${target}`);
        return;
      }
      const row = rows[0];
      tried.push(normalize(row.innerText || '').slice(0, 40));
      const prev = { oid: readOpenOrderId(), card: !!chatPendingOrder(), href: location.href };
      clickRow(row);
      const opened = await waitConversationOpens(prev, NAV_WAIT_MS);
      await waitMs(350); // 等 React 水合订单卡
      const oid = readOpenOrderId();
      if (oid === target) {
        // 命中目标会话：等右侧订单卡水合（自动点开时面板渲染有延迟，手动打开没这问题）。
        // 等到卡即发送；超时但该会话有聊天输入框也照发——oid 已确认是对的那一单，用户也已批准。
        const card = await waitOrderCard(6000);
        if (card) {
          console.log(`[xda] auto: 命中目标会话 ${target}，执行发送`);
          await executeSend(payload);
          return;
        }
        if (findChatInput()) {
          console.warn(`[xda] auto: 命中 ${target} 但未等到订单卡，按已打开会话继续发送`);
          await executeSend(payload);
          return;
        }
        sendResult(taskId, false, `找到订单 ${target} 的会话但既无订单卡也无聊天输入框，未发送`);
        return;
      }
      console.log(`[xda] auto: 第${attempt + 1}个会话非目标 opened=${opened.ok ? 1 : 0} oid=${oid || '(无订单号)'}`);
    }
    sendResult(taskId, false, `自动定位：点开 ${tried.length} 个会话仍未找到订单 ${target}`);
  }

  // ---------- 二次校验 ----------

  /** 订单级二次校验：只有确认“当前打开的会话就是该订单”时才下结论。
   *  - order_seen && 待发货卡仍在   → 等待卖家发货（任务保留；自动模式可据此发送）
   *  - order_seen && 卡已消失/已发货 → 已变化（卖家可能已手动发货 → 桌面端自动终结任务）
   *  - 当前看不到该订单             → 未知（桌面端不得据此作任何发送/终结结论）
   *  任务缺订单号时一律报未知：核对不上目标订单，宁可降级人工确认也不冒险自动发送。 */
  function handleRecheck(payload) {
    const target = String((payload && payload.order_id) || '').trim();
    const card = chatPendingOrder();
    let order_seen = false;
    let status = '未知';
    if (target) {
      const openOid = readOpenOrderId();
      order_seen = !!openOid && openOid === target;
      if (order_seen) status = card ? '等待卖家发货' : '已变化';
    }
    chrome.runtime.sendMessage({
      v: 1,
      type: 'recheck_result',
      payload: {
        task_id: payload.task_id,
        status,
        order_seen,
        product_id: extractProductId(),
        title: card ? card.title : extractTitle(),
      },
    }).catch(() => {});
  }

  // ---------- 消息入口 ----------

  chrome.runtime.onMessage.addListener((msg, _sender, sendResponse) => {
    if (!msg || msg.v !== 1) return;
    switch (msg.type) {
      case 'pause_state':
        paused = !!msg.payload?.paused;
        chrome.storage.local.set({ [PAUSE_KEY]: paused });
        break;
      case 'approve_send':
        // 自动定位：目标会话已打开则直接发送，否则回到会话列表逐会话核对订单号后发送。
        autoSendToOrder(msg.payload || {});
        break;
      case 'approve_probe': {
        // 轻量能力探测（同步）：background 据此决定把 approve_send 投给哪个标签页。
        const oid = String((msg.payload && msg.payload.order_id) || '');
        const card = chatPendingOrder();
        const rows = collectConversationRows();
        let score = 0;
        if (card && (!oid || card.order_id === oid)) score += 1000; // 目标会话已开 → 直接发
        else if (card) score += 100; // 开着别的待发货会话 → 可返回列表再自动定位
        if (rows.length) score += 60; // 有会话列表 → 可自动点开核对
        if (findChatInput()) score += 30; // 在某个会话内
        try {
          sendResponse({ v: 1, can: score > 0, score });
        } catch (_) { /* ignore */ }
        return true;
      }
      case 'recheck':
        handleRecheck(msg.payload || {});
        break;
      case 'rescan':
        lastKey = '';
        lastFpJson = '';
        reportDiag();
        scan();
        break;
      default:
        break;
    }
  });

  // ---------- 启动 ----------

  // 启动即发一个最小信标：判断 content 是否注入、是否在后续引导期崩溃。
  // （post/reportDiag 为函数声明已提升，此处即可调用。）
  function bootBeacon(tag, detail) {
    try {
      post({ v: 1, type: 'status_report', payload: { logged_in: true, dom_healthy: true, detail: `${tag} ${detail || ''}` } });
    } catch (_) { /* ignore */ }
  }

  try {
    chrome.storage.local.get(PAUSE_KEY).then((v) => {
      paused = !!v[PAUSE_KEY];
    });

    // MutationObserver 高频触发，节流 + 尾随执行
    let scanTimer = null;
    function scanThrottled() {
      if (scanTimer) return;
      scanTimer = setTimeout(() => {
        scanTimer = null;
        scan();
        maybeShipFingerprint();
      }, 500);
    }

    const observer = new MutationObserver(scanThrottled);
    observer.observe(document.documentElement, { childList: true, subtree: true, characterData: true });
    scan();
    bootBeacon('content_boot_ok', `body=${document.body ? document.body.innerText.length : -1} url=${location.pathname}`);
  } catch (e) {
    // 引导期任何异常都要让桌面端能看到，而不是静默无声
    bootBeacon('content_boot_err', String((e && e.message) || e));
    throw e; // 保留原始行为，便于 DevTools 里看到
  }

  // ---------- 页面诊断（只报结构与关键词标志，不上报聊天内容） ----------

  function collectDiag() {
    const text = document.body ? document.body.innerText : '';
    const iframes = [];
    document.querySelectorAll('iframe').forEach((f) => {
      try {
        const h = new URL(f.src, location.href).host;
        if (h) iframes.push(h);
      } catch (_) { /* ignore */ }
    });
    return {
      url: location.href.slice(0, 220),
      title: (document.title || '').slice(0, 60),
      is_top: window.top === window,
      url_path: location.pathname + location.search,
      iframes: iframes.slice(0, 8),
      has_wait_ship: text.includes('等待卖家发货'),
      has_dai_fahuo: text.includes('待发货'),
      has_deng_fahuo: text.includes('等待发货'),
      has_fahuo: text.includes('发货'),
      has_daifu: text.includes('待付款'),
      body_len: text.length,
      input_found: !!findChatInput(),
      send_found: !!findSendButton(),
    };
  }

  // 安全发送：扩展上下文失效时 chrome.runtime.sendMessage 会**同步抛错**（.catch 拦不住），
  // 残留的旧实例定时器会因此刷屏。统一 try/catch + 可见日志，方便在 DevTools 定位。
  function post(msg) {
    try {
      const p = chrome.runtime.sendMessage(msg);
      if (p && typeof p.catch === 'function') p.catch(() => {});
      console.log(`[xda] send ${msg.type} OK`);
      return true;
    } catch (e) {
      console.warn(`[xda] send ${msg.type} FAIL: ${e && e.message}`);
      return false;
    }
  }

  function reportDiag() {
    post({ v: 1, type: 'page_diag', payload: collectDiag() });
  }
  reportDiag();

  /** 发货流程按钮定位指纹：每个“发货”相关按钮的 文本 + 是否禁用 + 所属层(plain=普通页 /
   *  [xxx]=所在弹层) + 自身前两个类名 token。用来分辨多个同名按钮谁在哪个层。 */
  function shipButtonDetail() {
    const out = [];
    document.querySelectorAll('button, [role="button"]').forEach((el) => {
      if (!(el.offsetParent !== null || el.getClientRects().length)) return;
      const t = normalize(el.textContent || '').slice(0, 10);
      if (!t || t.length < 2 || t.length > 8) return;
      if (!/发货|确认|物流|快递|虚拟/.test(t)) return;
      let layer = 'plain';
      for (let n = el.parentElement; n && n !== document.body && n !== document.documentElement && n.tagName !== 'BUTTON'; n = n.parentElement) {
        const c = String(n.className || '');
        const hit = c.split(/\s+/).find((x) => /modal|drawer|dialog|popup/i.test(x));
        if (hit) { layer = hit; break; }
      }
      const cls = String(el.className || '').split(/\s+/).slice(0, 2).join('.') || 'nc';
      const dis = el.disabled || el.hasAttribute('disabled') ? '!' : '';
      // 注意：分隔符不能用 '@' —— 桌面审计脱敏会把含 @ 的片段当邮箱整段屏蔽
      out.push(`${t}${dis}|${layer}|${cls}`);
    });
    return out.slice(0, 8).join(';');
  }

  /** 页面出现发货相关按钮且按钮集合变化时上报一次定位指纹（人工走流程时逐页捕获）。 */
  let lastShipKey = '';
  function maybeShipFingerprint() {
    if (paused) return;
    const d = collectDiag();
    if (!d.has_fahuo && !d.has_dai_fahuo && !d.has_wait_ship) return;
    const key = shipButtonDetail();
    if (!key || key === lastShipKey) return;
    lastShipKey = key;
    console.log('[xda] SHIPDETAIL ' + key); // DevTools 兜底：若审计仍被脱敏，可从此行复制
    post({
      v: 1,
      type: 'status_report',
      payload: { logged_in: true, dom_healthy: true, detail: `SHIP path=${d.url_path} btns=${key}` },
    });
  }

  // 定期健康报告（低频）+ SPA 路由变化时补报诊断
  let lastUrl = location.href;
  // detail 里附带结构诊断标志（关键词布尔/iframe 数/路径），供桌面端审计定位识别问题；
  // 只含布尔与计数，不含任何聊天内容。
  function reportHealth() {
    const health = domHealth();
    const d = collectDiag();
    if (d.has_wait_ship || d.has_dai_fahuo) consoleProbe(); // 订单页时打印结构探查
    const diag =
      `wait_ship=${d.has_wait_ship ? 1 : 0} dai_fahuo=${d.has_dai_fahuo ? 1 : 0}` +
      ` deng_fahuo=${d.has_deng_fahuo ? 1 : 0} fahuo=${d.has_fahuo ? 1 : 0}` +
      ` daifu=${d.has_daifu ? 1 : 0} body=${d.body_len} frames=${d.iframes.length}` +
      ` top=${d.is_top ? 1 : 0} path=${d.url_path}` +
      ` url=${d.url} title=${d.title}`;
    post({
      v: 1,
      type: 'status_report',
      payload: { logged_in: true, dom_healthy: health.healthy, detail: `${health.detail} ${diag}` },
    });
  }
  setInterval(() => {
    reportHealth();
    if (location.href !== lastUrl) {
      lastUrl = location.href;
      lastKey = '';
      reportDiag();
      scan();
    }
  }, 60000);
  reportHealth();
})();
