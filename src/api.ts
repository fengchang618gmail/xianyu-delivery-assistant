import { invoke } from '@tauri-apps/api/core';
import { listen } from '@tauri-apps/api/event';

export type Mode = 'confirm' | 'auto';

export interface AppStatus {
  mode: Mode;
  paused: boolean;
  ext_connected: boolean;
  feishu_app_configured: boolean;
  feishu_authorized: boolean;
  bitable_selected: boolean;
  data_dir: string;
}

export interface Task {
  id: string;
  order_or_conversation_id: string;
  product_id: string | null;
  product_title: string | null;
  delivery_content: string | null;
  delivery_content_hash: string | null;
  mode: string;
  status: string;
  fail_reason: string | null;
  created_at: string;
  completed_at: string | null;
}

export interface Audit {
  id: number;
  task_id: string | null;
  event_type: string;
  details_redacted: string;
  created_at: string;
}

export interface TableValidation {
  ok: boolean;
  missing_fields: string[];
  duplicate_ids: string[];
  duplicate_titles: string[];
  empty_content_rows: number;
  total_rows: number;
  sample_redacted: string[];
  error: string | null;
}

export interface ExtStatus {
  registered: boolean;
  manifest_path: string;
  host_path: string;
  host_exists: boolean;
  chrome_found: boolean;
  edge_found: boolean;
  extension_dir: string | null;
  extension_id: string;
}

export interface Preview {
  product_id: string;
  product_title: string;
  delivery_content: string;
  note: string;
}

export const api = {
  status: () => invoke<AppStatus>('app_status'),
  setMode: (mode: Mode) => invoke<void>('set_mode', { mode }),
  setPaused: (paused: boolean) => invoke<void>('set_paused', { paused }),

  saveApp: (appId: string, appSecret: string) => invoke<void>('feishu_save_app', { appId, appSecret }),
  startOauth: () => invoke<string>('feishu_start_oauth'),
  listTables: (link: string) => invoke<[string, string][]>('feishu_list_tables', { link }),
  validateTable: (link: string, tableId: string) => invoke<TableValidation>('feishu_validate_table', { link, tableId }),
  preview: () => invoke<Preview>('feishu_preview_template'),
  syncProducts: () => invoke<number>('feishu_sync_products'),
  disconnectFeishu: () => invoke<void>('feishu_disconnect'),

  extStatus: () => invoke<ExtStatus>('extension_status'),
  extRegister: () => invoke<ExtStatus>('extension_register'),
  extOpen: (browser: 'chrome' | 'edge') => invoke<void>('extension_open_page', { browser }),

  tasksActive: () => invoke<Task[]>('tasks_active'),
  tasksRecent: (limit = 50) => invoke<Task[]>('tasks_recent', { limit }),
  confirm: (taskId: string, content: string) => invoke<void>('task_confirm', { taskId, content }),
  cancel: (taskId: string) => invoke<void>('task_cancel', { taskId }),
  records: (limit = 100) => invoke<Audit[]>('records_recent', { limit }),
  wipe: () => invoke<void>('wipe_records'),
};

/** 订阅后端事件；返回取消函数。 */
export const events = {
  tasksChanged: (cb: () => void) => listen('tasks-changed', cb),
  extConn: (cb: () => void) => listen('ext-conn', cb),
  feishuOauth: (cb: (ok: boolean, error?: string) => void) =>
    listen<{ ok: boolean; error?: string }>('feishu-oauth', (e) => cb(e.payload.ok, e.payload.error)),
};
