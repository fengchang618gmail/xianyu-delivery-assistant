import { useCallback, useEffect, useState } from 'react';
import { api, events, type AppStatus, type Audit, type Task } from './api';

/** 工作台：连接状态、模式切换、任务卡片、审计记录。 */
export default function Dashboard({ status, onStatus }: { status: AppStatus; onStatus: () => void }) {
  const [active, setActive] = useState<Task[]>([]);
  const [recent, setRecent] = useState<Task[]>([]);
  const [records, setRecords] = useState<Audit[]>([]);
  const [diag, setDiag] = useState('');

  const reload = useCallback(async () => {
    setDiag('loading…');
    try {
      const [a, r, rec] = await Promise.all([api.tasksActive(), api.tasksRecent(30), api.records(100)]);
      setActive(a);
      setRecent(r);
      setRecords(rec);
      setDiag(`ok active=${a.length} recent=${r.length} records=${rec.length}`);
    } catch (e) {
      setDiag(`LOAD ERROR: ${String(e)}`);
    }
  }, []);

  useEffect(() => {
    reload();
    const off = events.tasksChanged(() => reload());
    return () => { off.then((f) => f()); };
  }, [reload]);

  const pendingConfirm = active.filter((t) => t.status === '待确认');
  const autoPending = active.filter((t) => t.status === '待自动发送' || t.status === '待发送');
  const others = active.filter((t) => !pendingConfirm.includes(t) && !autoPending.includes(t));

  const setMode = async (mode: 'confirm' | 'auto') => {
    await api.setMode(mode);
    onStatus();
  };
  const togglePause = async () => {
    await api.setPaused(!status.paused);
    onStatus();
  };

  return (
    <div className="dash">
      <header className="topbar">
        <div className="conn">
          <span className={status.ext_connected ? 'ok' : 'warn'}>
            {status.ext_connected ? '● 浏览器扩展已连接' : '○ 扩展未连接'}
          </span>
          <span className="muted small">数据目录：{status.data_dir}</span>
        </div>
        <div className="row">
          <label className="radio">
            <input type="radio" checked={status.mode === 'confirm'} onChange={() => setMode('confirm')} />
            手动确认
          </label>
          <label className="radio" title="自动模式下只有当订单仍处于等待发货且商品完全一致时才会发送，否则降级为手动确认">
            <input type="radio" checked={status.mode === 'auto'} onChange={() => setMode('auto')} />
            自动模式
          </label>
          <button className="ghost" onClick={togglePause}>{status.paused ? '恢复监控' : '暂停监控'}</button>
          {status.paused && <span className="warn">已暂停 · 所有新订单将被忽略</span>}
        </div>
      </header>

      <main>
        <p className="muted small mono">{diag || ' '}</p>
        <h2>待确认 ({pendingConfirm.length})</h2>
        {pendingConfirm.length === 0 && <p className="muted">暂无待确认任务。订单出现后会在这里列出，等待你点击发送。</p>}
        {pendingConfirm.map((t) => <TaskCard key={t.id} task={t} onDone={reload} />)}

        <h2>进行中 ({autoPending.length})</h2>
        {autoPending.length === 0 && <p className="muted">没有正在自动处理的任务。</p>}
        {autoPending.map((t) => (
          <div key={t.id} className="card dim">
            <div className="card-head">
              <strong>{t.product_title ?? '(无标题)'}</strong>
              <span className="badge">{t.status === '待自动发送' ? '二次校验中…' : '发送中…'}</span>
            </div>
            <p className="muted small">自动模式：校验通过后自动填入并发送，不确定时降级为手动确认。</p>
          </div>
        ))}

        {others.length > 0 && (
          <>
            <h2>其他 ({others.length})</h2>
            {others.map((t) => (
              <div key={t.id} className="card dim">
                <div className="card-head">
                  <strong>{t.product_title ?? '(无标题)'}</strong>
                  <span className="badge">{t.status}</span>
                  {t.fail_reason && <span className="err small">{t.fail_reason}</span>}
                </div>
              </div>
            ))}
          </>
        )}

        <h2>最近任务</h2>
        <table className="tbl">
          <thead><tr><th>商品</th><th>状态</th><th>模式</th><th>时间</th></tr></thead>
          <tbody>
            {recent.map((t) => (
              <tr key={t.id}>
                <td>{t.product_title ?? '(无标题)'} <span className="muted small">#{t.order_or_conversation_id}</span></td>
                <td>{t.status}{t.fail_reason ? ` · ${t.fail_reason}` : ''}</td>
                <td>{t.mode}</td>
                <td className="muted small">{t.created_at}</td>
              </tr>
            ))}
            {recent.length === 0 && <tr><td colSpan={4} className="muted">暂无任务记录</td></tr>}
          </tbody>
        </table>

        <div className="row spread">
          <h2>安全日志（脱敏）</h2>
          <button className="danger ghost" onClick={async () => { await api.wipe(); reload(); }}>清除全部记录</button>
        </div>
        <div className="records">
          {records.map((r) => (
            <p key={r.id} className="mono small">
              <span className="muted">{r.created_at.slice(0, 19)}</span> [{r.event_type}] {r.details_redacted}
            </p>
          ))}
          {records.length === 0 && <p className="muted">暂无日志</p>}
        </div>

        <footer className="row spread">
          <span className="muted small">
            飞书：{status.feishu_app_configured ? '已配置应用' : '未配置'}
            {status.feishu_authorized ? ' · 已授权' : ''}
            {status.bitable_selected ? ' · 商品库已选定' : ''}
          </span>
          <div className="row">
            <SyncButton
              can={!!(status.feishu_app_configured && status.feishu_authorized && status.bitable_selected)}
              onDone={onStatus}
            />
            <button className="ghost" onClick={async () => { await api.disconnectFeishu(); onStatus(); }}>断开飞书授权</button>
          </div>
        </footer>
      </main>
    </div>
  );
}

/** 待确认任务卡片：内容可编辑，确认时使用编辑后文本。 */
function TaskCard({ task, onDone }: { task: Task; onDone: () => void }) {
  const [content, setContent] = useState(task.delivery_content ?? '');
  const [busy, setBusy] = useState(false);
  const [err, setErr] = useState('');

  const confirm = async () => {
    setBusy(true); setErr('');
    try {
      await api.confirm(task.id, content);
      onDone();
    } catch (e) {
      setErr(String(e));
    }
    setBusy(false);
  };
  const cancel = async () => {
    setBusy(true);
    try { await api.cancel(task.id); onDone(); } catch { /* ignore */ }
    setBusy(false);
  };

  return (
    <div className="card">
      <div className="card-head">
        <strong>{task.product_title ?? '(无标题)'}</strong>
        <span className="muted small">商品 {task.product_id ?? '—'} · 订单 {task.order_or_conversation_id}</span>
      </div>
      <textarea value={content} onChange={(e) => setContent(e.target.value)} rows={3} />
      {err && <p className="err">{err}</p>}
      <div className="row spread">
        <span className="muted small">发送前请核对内容；确认后内容将立即填入聊天窗口。</span>
        <div className="row">
          <button className="ghost" disabled={busy} onClick={cancel}>取消</button>
          <button disabled={busy || !content.trim()} onClick={confirm}>确认发送</button>
        </div>
      </div>
    </div>
  );
}

/** 手动同步商品库按钮：点击强制从飞书重读（配置好且授权后才可点）。 */
function SyncButton({ can, onDone }: { can: boolean; onDone: () => void }) {
  const [busy, setBusy] = useState(false);
  const [msg, setMsg] = useState('');
  const sync = async () => {
    setBusy(true); setMsg('');
    try {
      const n = await api.syncProducts();
      setMsg(`已同步 ${n} 条`);
      onDone();
    } catch (e) {
      setMsg('同步失败：' + String(e));
    }
    setBusy(false);
  };
  return (
    <span className="row">
      <button className="ghost" disabled={busy || !can} onClick={sync}>
        {busy ? '同步中…' : '同步商品库'}
      </button>
      {msg && <span className="muted small">{msg}</span>}
    </span>
  );
}
