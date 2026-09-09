import { useCallback, useEffect, useState } from 'react';
import { createRoot } from 'react-dom/client';
import './style.css';
import { api, events, type AppStatus } from './api';
import Wizard from './Wizard';
import Dashboard from './Dashboard';

/** 根组件：配置不完整 → 向导；完整 → 工作台。 */
function App() {
  const [status, setStatus] = useState<AppStatus | null>(null);
  const [err, setErr] = useState('');
  const [tick, setTick] = useState(0);
  const refresh = useCallback(() => setTick((t) => t + 1), []);

  useEffect(() => {
    api.status().then(setStatus).catch((e) => setErr(String(e)));
    const offs = [
      events.extConn(refresh),
      events.feishuOauth((ok) => { if (ok) refresh(); }),
    ];
    return () => { offs.forEach((p) => p.then((f) => f())); };
  }, [refresh]);

  useEffect(() => {
    api.status().then(setStatus).catch((e) => setErr(String(e)));
  }, [tick]);

  if (err && !status) return <main className="center"><p className="err">{err}</p></main>;
  if (!status) return <main className="center"><p className="muted">加载中…</p></main>;

  const ready = status.feishu_authorized && status.bitable_selected;
  return ready
    ? <Dashboard status={status} onStatus={refresh} />
    : <Wizard status={status} onStatus={refresh} />;
}

createRoot(document.getElementById('root')!).render(<App />);
