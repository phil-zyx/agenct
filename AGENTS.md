# AGENTS.md

本文件面向在 **Agenct v2** 仓库中协作的 AI Agent 与开发者，说明架构边界、代码布局、核心不变量与验证流程。

## 1. 架构定位与核心原则

Agenct v2 是一个极简单栈的 7×24h 个人超级智能体平台，核心原则是**「只保留一套、零双轨、零历史包袱」**：

```text
Web (原生 ESM + 单连接 SSE) ──▶ Rust Hub (单一 SQLite WAL) ──▶ OpenAI 兼容 LLM
                                      │
                    ┌─────────────────┴─────────────────┐
                    ▼                                   ▼
             PC Node (个人电脑)              Cloud Node (隔离容器)
              共用 node/agenct-node.py 与排他租约协议 2
```

- **单一后端**：单个 Rust 二进制 `agenct-hub`（Axum 0.8 + Tokio + rusqlite）。
- **零构建前端**：`hub/web/` 原生 ES Modules + CSS，由 Hub `ServeDir` 直接托管，不引入打包器或前端框架。
- **单一存储**：SQLite（WAL 模式，文件锁独占，`spawn_blocking` 读写隔离），附件二进制落盘 + 显式 ACL 表。
- **统一执行协议**：本地 PC 节点与云端容器节点共用 `node/agenct-node.py` 与 Lease Protocol 2。
- **Chat-First 体验**：所有能力围绕对话流 + 右侧 Muse 抽屉组织，保持改动小而克制。

---

## 2. 目录与模块映射

| 路径 | 职责 |
| --- | --- |
| `hub/src/main.rs` | 入口：加载环境配置、`reset-login-token` / `reencrypt-secrets` 子命令、启动 HTTP 服务与后台调度器 |
| `hub/src/lib.rs` | `Hub` 状态容器、统一 `Error`/`Result`、全部 HTTP 路由与中间件装配 |
| `hub/src/config.rs` | 环境变量解析、Origin 规范化、SSRF/外发 URL 校验、常量时间比较等安全原语 |
| `hub/src/secrets.rs` | `AGENCT_MASTER_KEY` AES-256-GCM 静态加密（按 `scope` + `user_id` 绑定 AAD）、向后兼容读取与离线密钥轮换 |
| `hub/src/db.rs` | SQLite Schema（当前 `SCHEMA_VERSION = 16`，含 `messages_fts` trigram 全文索引、军团角色 `agent_roles`、多引擎节点配置与任务回调字段）、迁移、进程级 `.lock` 排他锁、`audit_logs` 只追加触发器 |
| `hub/src/auth.rs` | 单人本地令牌登录 & 可选 GitLab OAuth 2.0 (PKCE S256)、SQLite Session、Host/Origin/CSRF 守卫中间件与 `AuthUser` 提取器 |
| `hub/src/brain/` | 单一大脑模块（`mod.rs`、`prompt.rs`、`tools.rs`、`parser.rs`、`roles.rs`、`threads.rs`、`compact.rs`）：`SOUL.md` + `MEMORY.md` + 军团角色 `agent_roles` + 隐式工作线程 `threads` + 滚动摘要压缩 + 原生 Tool Calling（带内联 `<tool_call>` 兜底）+ 异步回调闭环（`on_complete`）、`/api/bootstrap`、`/api/messages` 分页与 `/api/search` FTS5 全文搜索 |
| `hub/src/llm.rs` | OpenAI 兼容 `/chat/completions` 客户端（支持原生 `tools`、SSE 流式读取、`<think>` 过滤、图片多模态输入）与 `AGENCT_LLM_STUB=1` 确定性测试桩 |
| `hub/src/models.rs` | 每用户自定义大脑模型与识图（vision）模型配置及连通性测试 |
| `hub/src/nodes.rs` | 节点注册与多引擎配置、令牌轮换、安装脚本分发、Protocol 2 任务拉取（支持长轮询唤醒、`repos:` 与引擎路由）/租约续期/引擎绑定会话隔离与结果提交 |
| `hub/src/collab.rs` | 轻量跨人协作（`@同事` 委派、手动接收/交付、任务状态流转） |
| `hub/src/automation.rs` | 自动化权限开关、关键词分发规则、定时任务（Cron）与并发重试调度器 |
| `hub/src/attachments.rs` | 附件上传/下载、Magic-byte 校验、磁盘落盘与 `attachment_grants` ACL 鉴权 |
| `hub/src/events.rs` | 按 `user_id` 隔离的真 SSE 事件总线（`/api/events`，含对话流式 `delta` 推送） |
| `hub/src/integrations/gitlab.rs` | SSRF 加固的 GitLab Webhook、Todo 轮询、提交摘要与受控回复 |
| `hub/src/integrations/im.rs` | SSRF 加固的企业微信（API 长连接）/ 钉钉（Stream + OpenAPI）双向接入、`/bind` 本人配对及可选群 Webhook 加签通知 |
| `hub/tests/v2_core.rs` | Rust 集成与回归测试（使用内存 SQLite 与 LLM Stub） |
| `hub/web/` | 原生 ESM 前端：`index.html`、`style.css`、`js/{app,api,avatar,chat,drawer,palette,sound,sse,triage,ui}.js`、静态头像/品牌资源 |
| `node/agenct-node.py` | 统一执行节点客户端（支持 `claude`/`codex`/`agy`/`opencode`/自定义 `--cmd`） |
| `node/test_node.py` | 节点单元测试 |
| `scripts/backup.py` | SQLite 在线快照 + 附件打包备份与恢复 |
| `scripts/e2e_*.mjs` | Playwright 端到端冒烟与本地登录测试 |
| `deploy/` & `Dockerfile` | Hub 与 Cloud Node 容器镜像、Compose 编排与环境变量模板 `deploy/agenct.env.example` |

---

## 3. 架构不变量与安全红线（修改代码时必须遵守）

1. **Hub 绝不直接执行不可信 Shell 命令**
   - 任何代码执行/CLI 调用必须通过 `node_jobs` + Protocol 2 租约派发给已注册的 `pc` 或 `cloud` 节点。
   - `node/agenct-node.py` 默认沙箱为 `workspace-write`，不得默认开放全宿主机访问（仅在显式 `--unsafe-full-access` 时放开），且不得硬编码个人/组织路径。
   - 节点运行时创建的临时输入/证据目录应限定在工作目录内，避免外部目录被 CLI 沙箱拒绝。

2. **认证、CSRF、静态加密与最小权限**
   - 浏览器状态变更接口必须经过 `auth::guard` 的 Host、Origin 与 `X-CSRF-Token` 校验；受鉴权路由统一使用 `auth::AuthUser` 提取器（自动复用 `guard` 注入的 `User` 并强制校验 `status == "active"`）；前端统一通过 `hub/web/js/api.js` 的 `api()` 发起请求。
   - 用户自定义模型 API Key、GitLab PAT 与 IM 接入凭据/Webhook 配置在配置 `AGENCT_MASTER_KEY` 时一律经 `hub/src/secrets.rs` 静态加密（`enc:v1:`）落盘；新增敏感用户配置字段时不要绕过 `SecretCipher`。
   - `automation_permissions`（`auto_accept`、`auto_deliver`、`gitlab_execute`、`gitlab_reply`）全部默认关闭；权限撤销必须立即失效对应的排队/运行中节点租约。
   - 路由默认请求体上限为 64 KiB（仅 `/api/attachments` 放宽到 8 MiB、`/node/v1/tasks/{id}/submit` 放宽到 16 MiB），新增接口不要随意放大全局限制。

3. **SSRF 与外发边界**
   - 所有用户可控的外发 URL（自定义 LLM、IM Webhook、GitLab Origin）必须通过 `hub/src/config.rs` 校验；默认禁止非 HTTPS 及环回/私有/元数据 IP（仅在 `AGENCT_ALLOW_PRIVATE_OUTBOUND=1` 时放宽）。
   - `Hub.http` 禁用代理与自动重定向（`no_proxy` + `Policy::none()`）；用户可控外发（自定义 LLM、IM Webhook）统一走挂载 `PublicDnsResolver` 的 `Hub.user_http`，不要绕过这两个客户端直连外部地址。

4. **SQLite 存储与审计约束**
   - 所有数据库访问必须经由 `db::Db` 的阻塞读写隔离接口，严禁在 Tokio 异步线程中直接阻塞操作 SQLite。
   - 修改表结构时必须同步递增 `SCHEMA_VERSION` 并在 `hub/src/db.rs` 中补齐向前迁移逻辑。
   - `audit_logs` 由数据库触发器强制**只追加**，不要尝试更新或删除审计记录。
   - 附件落盘到 `<data_dir>/attachments`，访问必须校验 `attachment_grants`，禁止引入无界内存缓存。

5. **前端安全与同步约束**
   - 保持零构建原生 ESM，不要引入需要编译/打包的前端依赖。
   - 所有用户输入与模型输出插入 DOM 时必须使用 `esc()` 或安全的 `renderMarkdown()`；图片/链接仅放行同源 `/api/attachments/att_[0-9a-f]{32}`。
   - 实时更新统一走 `/api/events` SSE 事件驱动，不要退回多路定时轮询。

---

## 4. 开发与验证命令

提交或交付改动前，按改动范围运行对应检查（与 `.github/workflows/ci.yml` 保持一致）：

```sh
# 1. Rust 格式化、静态检查与全部测试
cargo fmt --manifest-path hub/Cargo.toml -- --check
cargo clippy --manifest-path hub/Cargo.toml --all-targets -- -D warnings
cargo test --manifest-path hub/Cargo.toml

# 2. Python 节点测试、语法检查与无头引擎实测体检
python3 -m unittest discover -s node -p 'test_*.py' -v
python3 -m py_compile node/agenct-node.py scripts/backup.py
python3 node/agenct-node.py --self-check --workdir . --agents codex,agy,opencode --timeout 90

# 3. 前端 ESM 与脚本语法检查
npm run check

# 4. 端到端测试（涉及登录/对话/UI 流程变更时按需运行）
npm run test:e2e
```

本地快速启动（免 GitLab，单人模式 + Stub LLM）：

```sh
AGENCT_LLM_STUB=1 cargo run --manifest-path hub/Cargo.toml
```

---

## 5. 变更与协作约定

- **回归测试**：任何状态流转、权限边界、协议解析或模型交互改动，均应在 `hub/tests/v2_core.rs` 或 `node/test_node.py` 补充对应回归测试。
- **新增 / 升级 CLI 引擎契约**：新增 `CLI_AGENTS` 引擎必须在 `node/agenct-node.py` 的 `CLI_ADAPTERS` 中实现完整无头权限隔离（区分 `readonly` / `workspace-write` / `unsafe-full`）与流式错误/拒权检测，并通过 `node/test_node.py` 契约测试及 `--self-check` 真实无头冒烟后再部署。
- **UI 验证**：修改 `hub/web/` 后，除 `npm run check` 外，应关注重复触发、关闭/重开抽屉、刷新持久化、失败态与取消态的表现。
- **敏感信息**：严禁提交真实 Token、API Key、个人聊天记录、本地数据库/备份包或部署私有配置（`deploy/agenct.env`、`deploy/data/` 等）。
- **诚实边界**：不要用硬编码脚本冒充真实模型能力、账号体系或安全隔离边界。
