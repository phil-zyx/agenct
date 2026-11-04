# Agenct

Repository: https://github.com/phil-zyx/agenct

一个以个人管家聊天窗口为入口的跨人 Agent 协作原型。每个人只关注自己的会话，任务、状态与交付在后台协调。

**当前版本是浏览器内脚本模拟，不是真实 AI 服务。** Alice 与 Bob 是虚构演示角色。没有真实账号认证、服务端、多人同步、模型调用、GitLab 连接或安全隔离。两个角色的数据都在同一浏览器 localStorage 中。不要输入个人信息、凭据或敏感代码。

## 快速开始

需要 Python 3 运行静态服务器；Node.js 22 或更高版本用于测试。没有第三方运行依赖。

```sh
python -m http.server 8000 --directory dist
# 打开 http://localhost:8000
npm test
npm run check
```

以 Alice 身份输入“请 Bob 修复报告单位显示问题”，切换 Bob 接单并推进，再切回 Alice 验收。角色切换仅用于演示，不代表访问控制。刷新保留本浏览器数据，重置会清空演示数据。

## 已实现

- 单窗口聊天、预设意图匹配、跨角色任务摘要路由。
- 接单、模拟执行与发布、证据核验、验收和退回。
- CI 失败、超时、未知结果、SHA 失效、取消和重试。
- 可折叠的关注事项、通信事件及审计导出。

## 目录

- `dist/`：可直接运行的静态源文件，无构建步骤。
- `test.cjs`、`test-chat.cjs`：状态机与聊天路由测试。
- `docs/architecture.md`：当前实现与未来边界。
- `docs/roadmap.md`：尚未实现的里程碑。

旧看板保留在 `/dashboard.html`，只作原型对照；聊天始终是主入口。

## 开发状态

这是实验性原型，不能作为生产系统的认证或授权基础。网页中的状态、费用、MR 和提交均为模拟数据。测试通过不意味着完成安全审计。协议不声明符合 A2A，也不隶属于 DeepSeek 或其他执行器厂商。

## 贡献与安全

请先阅读 [贡献指南](CONTRIBUTING.md) 和 [安全说明](SECURITY.md)。请勿在 Issue、日志或截图中提交秘密或真实私聊。

## 许可证

本项目采用 [MIT License](LICENSE)。第三方工具与服务仍适用各自条款；本仓库不包含任何第三方执行器。
