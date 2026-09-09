# 闲鱼发货助手：会话交接记录

最后更新：2026-09-04（Asia/Shanghai）

## 用户已确认的需求

- 仅支持 Windows 10/11；一个安装包，桌面应用和浏览器连接组件由同一安装流程引导。
- 后续从开始菜单、桌面快捷方式或可选开机自启启动，无需重装。
- 首次使用是傻瓜式单步骤向导：扩展启用、闲鱼登录、飞书授权、字段校验、安全预览测试。
- 默认“确认后发送”，用户可随时切换“自动发送”；自动模式遇到任一不确定情况必须停止并降级处理。
- 监控闲鱼卖家网页中“等待卖家发货”的订单；获取当前订单宝贝信息并到飞书多维表格读取发货内容。
- 飞书多维表格字段固定为：`商品 ID`、`商品标题`、`发货内容`。
- 匹配规则：商品 ID 精确匹配优先；仅当网页不能提取商品 ID 时，以标准化商品标题精确匹配兜底；零条或多条匹配均不发送。
- 不调用任何大模型 API；不保存闲鱼密码/Cookie/浏览器 profile；不抓取私有接口、不伪造请求、不绕过验证。
- 用户要求：继续直至完成；遇到常规问题自行决定，不要每个阶段要求用户“继续”。

## 权威文档

- `prd.md`：确认后的需求基线。
- `system-design.md`：架构、数据流、状态机、安全边界与工时估算的权威技术方案。
- `tech.md`：因编码技能要求而保留的索引文件，指向 `system-design.md`。
- Canvas 架构图：`C:\Users\fengchang\.cursor\projects\1778147922934\canvases\xianyu-delivery-architecture.canvas.tsx`。

## 当前源码状态（2026-09-04，代码全部写完，进入验证阶段）

### Rust 桌面端（src-tauri/）

- `src/redact.rs`：脱敏与哈希。URL/提取码/邮箱/≥7 位数字替换占位符；`content_hash` 盐化 sha256；`new_salt/new_token/new_state`。
- `src/db.rs`：rusqlite bundled SQLite。四表：`config`、`product_cache`、`delivery_task`、`audit_event`。
  任务去重 `task_dedup_check`（含 attempt 后缀幂等键）、终结清明文 `task_finalize`（仅留加盐摘要）、`wipe_records` 全擦除。
- `src/secrets.rs`：Windows Credential Manager（keyring v3）。飞书 App Secret / Access Token / Refresh Token。
- `src/feishu.rs`：OAuth（authen/v1/authorize + v2/token，本地回调 127.0.0.1:15731）、Token 自动刷新、
  Bitable `records/search` 只读分页读取、三列字段校验（缺列/重复 ID/重复标题/空内容 + 脱敏样例）。
- `src/nm.rs`：命名管道 `\\.\pipe\xianyu-delivery-assistant-nm`（行分隔 JSON，`v:1` 协议），
  hello 令牌认证前拒绝一切消息。
- `src/tasks.rs`：任务状态机
  （已发现→匹配中→待确认/待自动发送→待发送→已完成/异常/已取消/已忽略）。
  匹配：ID 精确优先、标题标准化兜底、零条/多条/空内容一律不发送。
  自动模式：60s 二次校验，状态变化→忽略、商品不一致→降级待确认、一致→批准发送；
  一次性批准令牌；发送失败/180s 超时→异常，不盲目重发。
- `src/state.rs`：AppState（db/http/mode/paused/nm/salt/产品缓存 TTL 300s/待批准令牌表）。
- `src/lib.rs`：17 个 Tauri 命令 + 4 个前端事件（`tasks-changed`/`ext-conn`/`ext-status`/`feishu-oauth`）。
  NM 宿主注册（HKCU Chrome+Edge NativeMessagingHosts）、扩展目录定位（安装态资源目录 / 开发态向上找）。
- `src/bin/nmhost.rs`：纯 std 伴生进程。浏览器 stdio 4 字节长度前缀帧 ↔ 命名管道行协议；
  桌面端未运行时发 `app_not_running` 后 3s 重连。
- 扩展固定 ID：`lkfdpfllnpejncdiggdbiffiebombajd`（manifest key 已嵌入）。
- NM Host 名称：`com.local.xianyu.deliveryassistant`；会话令牌 `%APPDATA%\com.local.xianyu.deliveryassistant\nm_token`。

### 浏览器扩展（extension/，v0.2.0）

- 仅 `seller.goofish.com`；提取商品 ID（data 属性→文本模式）、标题（多防御选择器）、订单号（URL 参数→文本）。
- 发送：一次性令牌必需；原生 setter + input 事件填入；点击发送按钮后 30s MutationObserver 找已发送消息作为证据回传。
- 暂停状态本地持久化；60s 健康报告；SW 冷启动自动 rescan。

### 前端（src/）

- `api.ts`：invoke 封装 + 事件订阅。
- `Wizard.tsx`：6 步向导（浏览器连接→闲鱼登录→飞书应用→飞书授权→商品库校验→安全测试预览）。
- `Dashboard.tsx`：连接状态、手动/自动模式、暂停/恢复、待确认任务卡片（内容可编辑）、最近任务表、脱敏日志、清除记录、断开飞书。

### 构建/打包

- `src-tauri/tauri.conf.json`：NSIS（currentUser 安装）、resources 映射 `../extension`→`extension`、
  externalBin `binaries/nmhost`、CSP 收紧。
- `src-tauri/capabilities/default.json`：core:default + notification:default + opener:allow-open-url。
- `src-tauri/.cargo/config.toml`：显式 MSVC linker（规避 Git Bash GNU link.exe 遮蔽）、`jobs = 4`。
- `scripts/package-windows.cmd`：vswhere 动态定位 MSVC + Windows SDK（10.0.26100.0 已装），
  先 build nmhost 复制到 `src-tauri/binaries/`，再 `tauri build`。
- `[profile.release] strip = true`；**禁用 fat LTO**（8GB 内存机器曾因此死机两次）。
- 图标：`app-icon.png` 已生成全套 `src-tauri/icons/`（含 icon.ico）。
- `.gitignore`、`README.md` 已建。

## 验证状态（2026-09-04）

- `npm run check`（tsc）：✅ 通过。
- `cargo test`：✅ 16/16 通过。曾修复 11 处编译错误（state.rs `use crate::db::{self, Db}`、
  with_db 闭包需包 `Ok(...)`（config_get 返回 Option 而非 Result）、字节字符串字面量须 ASCII（中文响应体改 String + as_bytes）、
  std TcpListener 用 `set_nonblocking(true)`、NamedPipeServer 用 `tokio::io::split`、
  TaskUpdate 加 `#[derive(Default)]`、execute_batch 补 map_err、TASK_SQL 拼接需 to_owned、
  nmhost 补 BufRead import）+ 1 处测试暴露的逻辑 bug（`placeholders()` 生成 `?1..?4` 编号占位符
  与查询前部显式 `?1` 撞号 → 改匿名 `?`）。
- `externalBin` 编译期即要求 `src-tauri/binaries/nmhost-x86_64-pc-windows-msvc.exe` 存在
  （先创建占位文件解锁编译，打包脚本会覆盖为真实产物）。
- `scripts/package-windows.cmd` 末行改为 `npx tauri build`（不依赖全局 tauri 命令）。
- **NSIS 打包 ✅ 已验证**：`src-tauri/target/release/bundle/nsis/闲鱼发货助手_0.1.0_x64-setup.exe`（3.4MB）。
  主程序 14.3MB、nmhost 伴生进程 278KB 均正常产出。
- 打包脚本排坑记录（`.cmd` 文件务必：① 纯 ASCII 注释——cmd 按 ANSI 代码页解析，UTF-8 中文会切碎命令；
  ② CRLF 换行——LF-only 会合并行破坏括号块；③ if 括号块内引用含 `(x86)` 的路径变量必须用延迟展开
  `!VSPATH!`——解析期 `%VSPATH%` 的 `)` 会提前闭合块）。
- NSIS 首次打包需从 GitHub 下载 NSIS 3.11，间歇超时属已知网络问题，重试 `npx tauri build --bundles nsis` 即可。
- **Windows 凭据管理器单条 Blob 上限 2560 字节**（UTF-16 编码），飞书 user_access_token（JWT，
  ~1.2KB 字符 → UTF-16 翻倍）写不进去。实测排坑后拆分存储：
  短凭据（App Secret）仍走 keyring；access/refresh token 对整体 JSON 用 **DPAPI
  （CryptProtectData CurrentUser）加密后存 `%APPDATA%\com.local.xianyu.deliveryassistant\feishu_tokens.bin`**。
  windows-sys 0.60 中 blob 结构体名为 `CRYPT_INTEGER_BLOB`（不是 CRYPTOAPI_BLOB）。
  另：Windows 上 accept 出的套接字继承监听端非阻塞模式，读回调前必须 `set_nonblocking(false)` + 设读超时
  （否则报 os error 10035）。
- **安装时 "Error opening file for writing nmhost.exe"**：浏览器扩展每几秒拉起 nmhost.exe，
  运行中的 exe 无法被覆盖。根治：`src-tauri/installer-hooks.nsh`（tauri.conf.json
  `bundle.windows.nsis.installerHooks` 引入）——PREINSTALL 先把 NM 清单改名（重生失败）
  + taskkill 双进程 + Sleep；POSTINSTALL 恢复清单，扩展 alarm 自动重连。PREUNINSTALL 同理。
  注意 .nsh 必须纯 ASCII（NSIS 按 ANSI 代码页解析）。
- **扩展连接排坑（实测定性）**：① 认证成功的 Hello 原先被管道服务器的认证分支消费，
  不会进业务回调 → `ext-conn` 事件永不触发、审计无记录；已改为认证后继续透传。
  ② MV3 Service Worker 休眠后 `setTimeout` 重连循环一并消失，桌面端重启后扩展永远连不上；
  已改 `chrome.alarms`(30s) 周期唤醒重连（manifest 需加 `alarms` 权限）。
  ③ 浏览器关闭端口后 nmhost 主线程阻塞在管道读循环不退出成僵尸，占住管道实例；
  已改为 stdin EOF 即 `process::exit(0)`。
  ④ 排查管道是否存活不能用 python `open(r'\\.\pipe\...')`（会误报 ENOENT），
  要用 ctypes `CreateFileW`（见审计日志 ext_connected 事件验证端到端）。
  ⑤ **content.js 全部上行消息缺 `v:1` 协议字段（2026-09-05 实锤）**：background 的
  onMessage 入口有 `if (!msg || msg.v !== 1) return` 过滤，而 content.js 的
  page_event / status_report / send_result / recheck_result / page_diag 8 处
  sendMessage 都没带 v:1 → 从扩展加载第一天起所有心跳与订单事件被静默丢弃。
  症状极具迷惑性：SW、nmhost、管道全链路"假活"（hello 是 nmhost 自构造的，
  带 v:1，所以 ext_connected 正常），但零业务事件。已全部补上 v:1；
  另把关键词布尔/iframe 数/路径嵌进 status_report 的 detail 字段
  （运行中的应用会原文写入审计），无需更新桌面端即可定位识别问题；
  status_report 现在注入后立即上报一次（原先要等 60s）。
- **飞书知识库形态分享链接**：多维表格嵌在知识库里时分享出 `/wiki/<节点token>` 链接，
  不是 `/base/` 直链，且节点 token 不能直接调 bitable API——必须先调
  `GET /wiki/v2/spaces/get_node` 换 `obj_token`（应用需开通 `wiki:node:read`
  「获取知识空间节点信息」权限并发布新版本，user_access_token 也需重新授权获取新权限范围）。
  `parse_bitable_url` 现返回 `(token, table_id, is_wiki)`，调用侧统一经
  `resolve_app_token` 换算。
- **字段名容错匹配（1254045 FieldNameNotFound）**：records/search 直接传
  `field_names: ["商品 ID",...]` 时，用户表列名有细微差异（如「商品ID」无空格）即被
  飞书整体拒绝，缺列校验无机会执行。`fetch_products` 已改为先调
  `GET /tables/{id}/fields`（不依赖字段名）拿实际列名，精确匹配失败再做
  squash 匹配（去全部空白 + 小写），用匹配到的实际列名取数；三列全找不到时报出
  该表现有字段清单，1254045 也附操作提示。
- **异步按钮即时反馈**：向导所有异步按钮（注册/检查/保存/授权/读取/校验）点击瞬间
  切换为「xx中…」并禁用——StepBitable 用显式 `phase` 状态（''/reading/validating）
  区分两个共享 busy 的按钮。
- **令牌续期（定论，2026-09-05 修正）**：refresh_token 的下发条件是**授权链接显式声明
  `scope=offline_access` 且应用开通 offline_access 权限**，与商店/自建应用类型无关。
  最初用 v2 端点拿不到 refresh_token，是因为授权 URL 没传 scope 参数（当时误判为
  "v2 只给商店应用下发"并切到 v1 oidc 端点）；而 v1 `authen/v1/oidc/access_token` 是
  旧式接口，要求请求头 `Authorization: Bearer <app_access_token>`，缺失即报
  **code 20014**（实测）。最终方案：令牌端点统一用 v2
  `POST /authen/v2/oauth/token`（JSON body，grant_type=authorization_code /
  refresh_token，扁平响应，失败带 error/error_description），授权 URL 用
  `client_id + response_type=code + scope=bitable:app:readonly wiki:node:read offline_access`
  （scope 为增量授予，历史权限累积保留）。refresh_token 单次有效、刷新时轮换，
  过期（20037）或授权码失效（20003/20004/20065）时错误信息明确提示重新授权。
  「已授权」判定为 `feishu::has_usable_token`（access 未过期或有 refresh_token）。
  排坑：官方文档页可加 `.md` 后缀直接取 markdown 源（HTML 页是 JS 渲染，curl 抓不到正文）。
- **构建看门狗**：`scripts/safe-build.ps1`——预检（commit>80%/空闲<1000MB 拒绝启动）、
  BelowNormal 优先级、运行中每 12s 巡查（commit>88% 或空闲<500MB 即 taskkill /T 杀构建树）。
  2026-09-04 22:37 第三次死机（轻量增量构建 + 全天积累的内存基线）后引入；所有
  cargo/tauri 构建必须经它运行，禁止裸跑 `package-windows.cmd`。

## 变更记录（2026-09-07）

**修复「同步商品库失败：令牌请求失败: error sending request for url (…/authen/v2/oauth/token)」**

- 现场诊断：报错为 reqwest 传输层错误（非飞书业务错误）。已确认本机 DNS/TCP 正常；
  用户正常终端 curl 同端点 HTTP/1.1 + TLS 成功（返回预期 HTTP 400/20063）；安装包为最新构建
  （09-06 18:08，含 `http1_only` 修复）；失败出现在 `ensure_access_token` 的 refresh_token 刷新
  请求（access_token 2h 过期后每次点同步都会触发），此前成功过、最近偶发失败 → 符合
  代码注释记载的「飞书 CDN 到本机偶发不稳」，且 `token_request` 是唯一没有重试的请求路径。
- 修改（`src-tauri/src/feishu.rs` + `lib.rs`）：
  1. `token_request` 对传输层错误自动重试 3 次（与 `send_retry` 同策略，业务错误码不重试）；
  2. 失败信息带错误类别（超时/无法建立连接/请求发送中断/读取响应中断/传输层异常）+
     完整原因链（`e.source()` 遍历，TLS/证书细节可见）；
  3. 手动同步失败写审计事件 `manual_sync_failed`（此前只在成功时记 `manual_sync`），
     便于在应用「最近记录」追溯。
- 状态：新 NSIS 安装包待重装验证；若重装后仍失败，报错会给出真实内因，可直接定位。

## 下一步（按顺序）

1. 安装 `闲鱼发货助手_0.1.0_x64-setup.exe` 并跑通应用内 6 步向导（浏览器扩展手动加载一次）。
2. 在真实闲鱼页面（用户登录态）校验/微调 `extension/content.js` 选择器 —— **必须现场验证**。
3. 用用户真实飞书授权做端到端：OAuth→字段校验→测试行匹配（不得首次就在真实买家订单上测试）。
4. 确认发送 + 自动模式二次校验实测。

## 重要限制（诚实清单）

- **扩展 DOM 选择器未实测**：商品 ID/标题提取、输入框与发送按钮定位均基于常见模式猜测，
  首次使用可能需按真实页面结构调整。README 已标注。
- **飞书端到端未验证**：无用户授权信息，OAuth/Bitable 读取/字段校验代码路径尚未真实跑通。
- 浏览器扩展不能被安装包静默启用；向导引导用户手动"加载已解压的扩展程序"一次。
- 平台条款与授权仍是部署前风险项。
- 8GB 内存纪律：编译/打包严禁并行重负载；`cargo jobs=4`；禁 fat LTO（曾死机两次）。
