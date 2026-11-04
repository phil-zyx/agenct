# Agenct

Repository: https://github.com/phil-zyx/agenct

一个以个人管家聊天窗口为入口的跨人 Agent 协作原型。每个人只关注自己的会话，任务、状态与交付在后台协调。

**在线演示仍是浏览器内脚本模拟，不是真实 AI 服务。** Alice 与 Bob 是虚构演示角色。该静态页面没有真实账号认证、服务端、多人同步、模型调用、GitLab 连接或安全隔离；仓库中的 Rust Hub 需另行启动。两个角色的数据都在同一浏览器 localStorage 中。不要输入个人信息、凭据或敏感代码。

## 快速开始

需要 Python 3 运行静态服务器；Node.js 22 或更高版本用于测试。没有第三方运行依赖。

```sh
python -m http.server 8000 --directory dist
# 打开 http://localhost:8000
npm test
npm run check
```

以 Alice 身份输入“请 Bob 修复报告单位显示问题”，切换 Bob 接单并推进，再切回 Alice 验收。角色切换仅用于演示，不代表访问控制。刷新保留本浏览器数据，重置会清空演示数据。

## 静态演示已实现

- 单窗口聊天、预设意图匹配、跨角色任务摘要路由。
- 接单、模拟执行与发布、证据核验、验收和退回。
- CI 失败、超时、未知结果、SHA 失效、取消和重试。
- 可折叠的关注事项、通信事件及审计导出。

## 目录

- `dist/`：可直接运行的静态源文件，无构建步骤。
- `test.cjs`、`test-chat.cjs`：状态机与聊天路由测试。
- `hub/`：Rust 服务、单窗口客户端、未接通的 Worker/发布库。
- `adapters/dsh/`：DSH SDK 适配器与离线协议测试。
- `scripts/`、`deploy/`：私有试运行工具和服务模板。
- `docs/architecture.md`：当前实现与未来边界。
- `docs/roadmap.md`：尚未实现的里程碑。

旧看板保留在 `/dashboard.html`，只作原型对照；聊天始终是主入口。

## 开发状态

这是实验性原型，不能作为生产系统的认证或授权基础。网页中的状态、费用、MR 和提交均为模拟数据。测试通过不意味着完成安全审计。协议不声明符合 A2A，也不隶属于 DeepSeek 或其他执行器厂商。

## 贡献与安全

请先阅读 [贡献指南](CONTRIBUTING.md) 和 [安全说明](SECURITY.md)。请勿在 Issue、日志或截图中提交秘密或真实私聊。

## 许可证

本项目采用 [MIT License](LICENSE)。第三方工具与服务仍适用各自条款；本仓库不包含任何第三方执行器。

## Rust Hub + DSH 技术验证（v0.2）

新增 `hub/`：Rust/Axum 本地开发服务，开发令牌身份认证、SQLite 持久消息与任务、跨身份隔离及人工接单/交付/验收。它与 `dist/` 静态演示分开运行。真实 DSH 适配层见 `adapters/dsh/`；默认不启用模型或危险工具，mock 与真实模式明确区分且不会自动回退。

开始运行与局限见 [Rust Hub 指南](docs/rust-hub.md)。生产登录、PostgreSQL、可运行的 Agent 军团、可信代码执行和完整远端交付链路尚未完成。不要将开发服务器暴露到公网。


### Controlled delegation demo

The Rust chat UI now supports sender-private delegation proposals, explicit sharing confirmation, recipient acceptance/manual reply and requester acceptance. In `mock` mode, try `请让 bob 帮我检查公开文档` (optional whitespace after `帮我`) or `ask bob to review the public docs`. This is a **deterministic test double**, not live DSH language understanding. `/delegate bob ...` also prepares a proposal rather than immediately sharing. See [the acceptance walkthrough and limitations](docs/rust-hub.md#controlled-delegation-acceptance-offline-test-double). The browser-only `dist/` simulation remains separate.

## 当前能力与明确缺口

- 单窗口服务端聊天、持久任务、私有提案确认和人工交付可在本机运行；开发令牌不是生产账号。UI 自动轮询，不是实时推送。
- [个人配置 API](docs/personal-profiles.md)可保存角色与模型引用；目前仅支持 `server-default`，不改变运行时、不启动 Agent，也不提供个人密钥输入或配置 UI。
- [DSH 适配器](docs/dsh-adapter.md)实现 SDK 路径与严格结构化提案校验；真实模型调用和有效工具限制仍未验收。无静默 mock 回退。
- [Worker 队列](docs/worker-contract.md)已提供按任务授权的 HTTP 入队、查询和取消接口，须由接单人明确确认；仅记录排队，不授权执行。接单人的聊天任务卡可查看、确认入队和取消，尚无运行中的消费者。[发布合同与 GitLab 传输](docs/gitlab-transport.md)仍是库。内置执行器因证据可信性问题保持禁用；没有自动编码、分支上传、真实 MR 或 CI 验收结果。
- [私有试运行工具](docs/private-staging.md)可准备本地服务和 SSH 访问；未部署远端实例，没有已验证的云端后端 URL。不要公开开发令牌或将服务暴露到公网。
- 确认委派是聊天 UI 的流程保证；兼容 `POST /api/tasks` 接口可直接创建任务且不幂等。普通模型聊天超时也不能自动重试。

完整边界见 [架构](docs/architecture.md)，后续验收门槛见 [路线图](docs/roadmap.md)。离线/API/模拟 DOM 测试通过不等于真实浏览器、提供商或沙箱验收。

### Durable chat recovery

The Rust chat client now retains owner-scoped turn receipts across a tab refresh
and checks an uncertain request's status instead of resending it. Keyed API calls
are dispatched at most once per owner/request ID; replies and private proposals
commit atomically with their receipt. Legacy API calls without a request ID are
not deduplicated. This does not enable code execution or publication. See
[recovery behavior and limits](docs/chat-turn-recovery.md).
