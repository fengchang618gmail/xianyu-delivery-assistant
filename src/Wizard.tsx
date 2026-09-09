import { useCallback, useEffect, useState } from 'react';
import { openUrl } from '@tauri-apps/plugin-opener';
import { api, events, type AppStatus, type ExtStatus, type TableValidation } from './api';

const STEPS = ['浏览器连接', '闲鱼登录', '飞书应用', '飞书授权', '商品库校验', '安全测试'];

/**
 * 6 步引导向导。每一步只依赖后端已实现的能力；
 * 任何一步失败都可返回上一步或跳过（安全设计：缺配置时任务不会发送）。
 */
export default function Wizard({ status, onStatus }: { status: AppStatus; onStatus: () => void }) {
  const [step, setStep] = useState(0);
  return (
    <div className="wizard">
      <header>
        <h1>闲鱼发货助手 · 首次配置</h1>
        <ol className="steps">
          {STEPS.map((s, i) => (
            <li key={s} className={i === step ? 'on' : i < step ? 'done' : ''}>{s}</li>
          ))}
        </ol>
      </header>
      <main>
        {step === 0 && <StepBrowser onNext={() => setStep(1)} />}
        {step === 1 && <StepGoofish onNext={() => setStep(2)} onBack={() => setStep(0)} />}
        {step === 2 && <StepApp onNext={() => setStep(3)} onBack={() => setStep(1)} />}
        {step === 3 && <StepOauth status={status} onStatus={onStatus} onNext={() => setStep(4)} onBack={() => setStep(2)} />}
        {step === 4 && <StepBitable onNext={() => setStep(5)} onBack={() => setStep(3)} />}
        {step === 5 && <StepPreview onDone={onStatus} onBack={() => setStep(4)} />}
      </main>
    </div>
  );
}

function StepBrowser({ onNext }: { onNext: () => void }) {
  const [ext, setExt] = useState<ExtStatus | null>(null);
  const [connected, setConnected] = useState(false);
  const [reging, setReging] = useState(false);
  const [checking, setChecking] = useState(false);

  const refresh = useCallback(async () => {
    setChecking(true);
    try {
      const [s, st] = await Promise.all([api.extStatus(), api.status()]);
      setExt(s);
      setConnected(st.ext_connected);
    } catch { /* 应用启动瞬间可能失败，忽略 */ }
    setChecking(false);
  }, []);

  useEffect(() => {
    refresh();
    const off = events.extConn(() => refresh());
    return () => { off.then((f) => f()); };
  }, [refresh]);

  if (!ext) return <p className="muted">加载中…</p>;

  return (
    <section>
      <h2>① 安装浏览器连接组件</h2>
      <ol className="guide">
        <li>点击「注册本机组件」，把 Native Messaging 宿主写入 Chrome / Edge 配置。</li>
        <li>点击「打开扩展页」，在浏览器中开启右上角「开发者模式」。</li>
        <li>点「加载已解压的扩展程序」，选择下方给出的扩展目录。</li>
        <li>扩展加载后会自动连接本应用，下方指示灯变绿即成功。</li>
      </ol>
      <p className="muted small">
        提示：若指示灯长时间不变绿，请在浏览器中点击一次扩展图标或刷新闲鱼卖家页 ——
        浏览器会让扩展休眠，点击/刷新会唤醒它自动重连。
      </p>
      <p className="mono">扩展目录：{ext.extension_dir ?? '（未找到，请重新安装本应用）'}</p>
      <p className="muted">扩展 ID：{ext.extension_id}（已在 manifest 中固定，无需修改）</p>
      <div className="row">
        <button
          disabled={reging}
          onClick={async () => {
            setReging(true);
            try { setExt(await api.extRegister()); } catch { /* 忽略，稍后可重试 */ }
            setReging(false);
          }}
        >{reging ? '注册中…' : '注册本机组件'}</button>
        {ext.chrome_found && <button className="ghost" onClick={() => api.extOpen('chrome')}>打开 Chrome 扩展页</button>}
        {ext.edge_found && <button className="ghost" onClick={() => api.extOpen('edge')}>打开 Edge 扩展页</button>}
        <button className="ghost" disabled={checking} onClick={refresh}>{checking ? '检查中…' : '重新检查'}</button>
      </div>
      <p className={connected ? 'ok' : 'warn'}>
        {connected ? '● 扩展已连接' : '○ 尚未检测到扩展连接'}
      </p>
      <div className="row spread">
        <span className="muted">宿主已注册：{ext.registered ? '是' : '否'} · Chrome：{ext.chrome_found ? '已安装' : '未找到'} · Edge：{ext.edge_found ? '已安装' : '未找到'}</span>
        <button disabled={!connected} onClick={onNext}>下一步</button>
      </div>
    </section>
  );
}

function StepGoofish({ onNext, onBack }: { onNext: () => void; onBack: () => void }) {
  const openSeller = () => {
    // 用系统默认浏览器打开；扩展只装在用户日常浏览器中
    openUrl('https://seller.goofish.com/').catch(() => {});
  };
  return (
    <section>
      <h2>② 登录闲鱼卖家工作台</h2>
      <ol className="guide">
        <li>点击下方按钮，在装有扩展的浏览器中打开闲鱼卖家后台。</li>
        <li>完成登录，并确认能看到聊天 / 订单页面。</li>
        <li>保持浏览器开启即可，应用会在后台配合扩展工作。</li>
      </ol>
      <p className="warn">说明：登录态完全留在你自己的浏览器中，本应用不保存任何闲鱼账号信息。</p>
      <div className="row spread">
        <button className="ghost" onClick={onBack}>上一步</button>
        <div className="row">
          <button onClick={openSeller}>打开闲鱼卖家页</button>
          <button onClick={onNext}>我已登录，下一步</button>
        </div>
      </div>
    </section>
  );
}

function StepApp({ onNext, onBack }: { onNext: () => void; onBack: () => void }) {
  const [appId, setAppId] = useState('');
  const [secret, setSecret] = useState('');
  const [msg, setMsg] = useState('');
  const [busy, setBusy] = useState(false);
  const save = async () => {
    setMsg('');
    setBusy(true);
    try {
      await api.saveApp(appId.trim(), secret.trim());
      onNext(); // 保存成功直接进入下一步
    } catch (e) {
      setMsg(`保存失败：${e}`);
      setBusy(false);
    }
  };
  return (
    <section>
      <h2>③ 填写飞书应用凭据</h2>
      <ol className="guide">
        <li>在飞书开放平台创建「企业自建应用」，在「权限管理」中开通以下权限并<strong>创建版本发布</strong>：
          <br />• 查看、评论、导出多维表格（<code>bitable:app:readonly</code>）
          <br />• 获取知识空间节点信息（<code>wiki:node:read</code>，解析知识库分享链接用）
          <br />• 离线访问（<code>offline_access</code>，用于免重复授权的令牌续期）</li>
        <li>把 App ID 与 App Secret 填到下面。Secret 只存进 Windows 凭据管理器，不落明文文件。</li>
      </ol>
      <label>App ID<input value={appId} onChange={(e) => setAppId(e.target.value)} placeholder="cli_xxxxxxxx" /></label>
      <label>App Secret<input type="password" value={secret} onChange={(e) => setSecret(e.target.value)} placeholder="••••••••" /></label>
      {msg && <p className={msg.startsWith('已') ? 'ok' : 'err'}>{msg}</p>}
      <div className="row spread">
        <button className="ghost" onClick={onBack}>上一步</button>
        <button disabled={!appId.trim() || !secret.trim() || busy} onClick={save}>{busy ? '保存中…' : '保存并下一步'}</button>
      </div>
    </section>
  );
}

function StepOauth({ status, onStatus, onNext, onBack }: { status: AppStatus; onStatus: () => void; onNext: () => void; onBack: () => void }) {
  const [err, setErr] = useState('');
  const [waiting, setWaiting] = useState(false);

  useEffect(() => {
    const off = events.feishuOauth((ok, error) => {
      setWaiting(false);
      if (!ok) setErr(error ?? '授权失败');
      else onStatus();
    });
    return () => { off.then((f) => f()); };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  const start = async () => {
    setErr('');
    setWaiting(true);
    try {
      const url = await api.startOauth();
      openUrl(url).catch((e) => setErr(String(e)));
    } catch (e) {
      setWaiting(false);
      setErr(String(e));
    }
  };

  if (status.feishu_authorized) {
    return (
      <section>
        <h2>④ 飞书授权</h2>
        <p className="ok">● 已授权。Token 已安全保存，应用将自动续期。</p>
        <div className="row spread">
          <button className="ghost" onClick={onBack}>上一步</button>
          <button onClick={onNext}>下一步</button>
        </div>
      </section>
    );
  }
  return (
    <section>
      <h2>④ 飞书授权</h2>
      <p>点击「开始授权」会打开飞书登录页，授权本应用读取你选择的多维表格（仅只读）。若授权页提示某权限未开通，回到第③步核对权限清单并发布新版本后再试。</p>
      <p className="warn">
        前置条件：在飞书开发者后台你的应用下，进入「安全设置」→「重定向 URL」，添加
        <code> http://127.0.0.1:15731/callback </code>
        并保存；否则飞书会报「重定向 URL 有误」。
      </p>
      {waiting && <p className="muted">等待你在浏览器中完成授权…</p>}
      {err && <p className="err">{err}</p>}
      <div className="row spread">
        <button className="ghost" onClick={onBack}>上一步</button>
        <button disabled={waiting} onClick={start}>{waiting ? '等待飞书授权…' : '开始授权'}</button>
      </div>
    </section>
  );
}

function StepBitable({ onNext, onBack }: { onNext: () => void; onBack: () => void }) {
  const [link, setLink] = useState('');
  const [tables, setTables] = useState<[string, string][]>([]);
  const [tableId, setTableId] = useState('');
  const [validation, setValidation] = useState<TableValidation | null>(null);
  // '' | 'reading' | 'validating'——点击瞬间就切换按钮文案，保证即时反馈
  const [phase, setPhase] = useState('');
  const [err, setErr] = useState('');

  const loadTables = async () => {
    setErr(''); setTables([]); setTableId(''); setValidation(null);
    setPhase('reading');
    try {
      const t = await api.listTables(link.trim());
      setTables(t);
      if (t.length === 0) setErr('该文档中没有多维表格数据表');
      if (t.length === 1) setTableId(t[0][0]);
    } catch (e) {
      setErr(String(e));
    }
    setPhase('');
  };

  const validate = async () => {
    setErr(''); setPhase('validating');
    try {
      const v = await api.validateTable(link.trim(), tableId);
      setValidation(v);
    } catch (e) {
      setErr(String(e));
    }
    setPhase('');
  };

  return (
    <section>
      <h2>⑤ 校验商品库</h2>
      <p>粘贴多维表格分享链接（<code>/base/</code> 直链或知识库 <code>/wiki/</code> 链接均可），应用将检查「商品 ID / 商品标题 / 发货内容」三列是否可用。</p>
      <label>多维表格链接<input value={link} onChange={(e) => setLink(e.target.value)} placeholder="https://xxx.feishu.cn/base/…" /></label>
      <div className="row">
        <button disabled={!link.trim() || !!phase} onClick={loadTables}>{phase === 'reading' ? '读取中…' : '读取数据表'}</button>
        {tables.length > 1 && (
          <select value={tableId} onChange={(e) => setTableId(e.target.value)}>
            {tables.map(([id, name]) => <option key={id} value={id}>{name}</option>)}
          </select>
        )}
        <button disabled={!tableId || !!phase} onClick={validate}>{phase === 'validating' ? '校验中…' : '校验字段'}</button>
      </div>
      {err && <p className="err">{err}</p>}
      {validation && (
        <div className="result">
          {validation.ok
            ? <p className="ok">● 校验通过：{validation.total_rows} 行商品，内容已全部就绪</p>
            : <p className="err">✗ 校验未通过</p>}
          {validation.missing_fields.length > 0 && <p className="err">缺少字段：{validation.missing_fields.join('、')}</p>}
          {validation.duplicate_ids.length > 0 && <p className="err">重复商品 ID：{validation.duplicate_ids.slice(0, 5).join('、')}（这些商品将不会被自动匹配）</p>}
          {validation.duplicate_titles.length > 0 && <p className="err">重复标题：{validation.duplicate_titles.slice(0, 5).join('、')}</p>}
          {validation.empty_content_rows > 0 && <p className="err">发货内容为空的行：{validation.empty_content_rows}</p>}
          <details><summary className="muted">查看脱敏样例（链接与提取码已打码）</summary>
            <pre>{validation.sample_redacted.join('\n') || '（无）'}</pre>
          </details>
        </div>
      )}
      <div className="row spread">
        <button className="ghost" onClick={onBack}>上一步</button>
        <button disabled={!validation?.ok} onClick={onNext}>下一步</button>
      </div>
    </section>
  );
}

function StepPreview({ onDone, onBack }: { onDone: () => void; onBack: () => void }) {
  const [preview, setPreview] = useState<Awaited<ReturnType<typeof api.preview>> | null>(null);
  const [err, setErr] = useState('');

  useEffect(() => {
    api.preview().then(setPreview).catch((e) => setErr(String(e)));
  }, []);

  return (
    <section>
      <h2>⑥ 安全测试预览</h2>
      <p>以下是一条脱敏后的真实模板样例（展示用，不会发送到任何聊天）：</p>
      {preview && (
        <div className="result">
          <p className="mono">商品 ID：{preview.product_id}</p>
          <p className="mono">商品标题：{preview.product_title}</p>
          <pre>{preview.delivery_content}</pre>
          <p className="muted">{preview.note}</p>
        </div>
      )}
      {err && <p className="err">{err}</p>}
      <div className="row spread">
        <button className="ghost" onClick={onBack}>上一步</button>
        <button onClick={onDone}>完成，进入工作台</button>
      </div>
    </section>
  );
}
