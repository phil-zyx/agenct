# Agenct

一个以个人管家聊天窗口为入口的跨人 Agent 协作原型：私聊 → 私有委派提案 → 人工确认共享 → 对方接手 → 私密起草与多轮修改 → 人工交付 → 验收或退回。

Rust Hub 提供真实服务端存储与身份边界；默认 `mock` 不调用模型，`dsh` 接入 DeepSeek。`dist/` 是独立的浏览器脚本演示，不代表真实账号或多人同步。当前用于可信主机上的小范围试用，尚未完成公网生产部署；代码执行和自动发布保持关闭。

## 开始使用

在 Linux/WSL 准备 Python 3.9+、Rust stable，从仓库根目录启动：

```sh
python3 scripts/private-staging.py start --state-dir "$HOME/agenct-private"
```

打开终端显示的本机地址，通过私有状态目录 `credentials.json` 中自己的令牌登录。初次构建需要等待；后续启动保留身份和数据。

- [入门](docs/getting-started.md)：模型接入、完整委派与草稿修改流程、双电脑 SSH 使用、在线加人和备份恢复。
- [技术与运行参考](docs/reference.md)：架构、API、角色与摘要、文件材料、数据迁移、日志、测试与验收门槛。
- [开发计划](docs/next-steps-plan.md)：本轮实现范围及待真实环境验收的项目。
- [个人助手与成员协作](docs/collaboration.md)：公开资料、私有偏好、联系人会话、助手代答与交付验收。
- [界面用语](docs/ui-terminology.md)：产品用词约定。

## 当前能力

通过聊天整理并确认个人资料、私有背景偏好和助手权限；双人联系人会话、未读提示及明确标记的公开资料自动答复；私聊持久化与只读回执恢复；可编辑确认的委派提案；文本/Markdown/PDF 共享材料预览；任务筛选和提醒；角色设置与起草推荐；长聊天和大材料摘要；保留版本的私密改稿；人工多行交付预览；共享时间线；在线用户/令牌管理；一致数据库备份恢复和无正文运行日志。可选 OIDC 组织映射限制跨组织委派；通过本地固定边界检查后，可配置白名单仓库的只读提交上下文。

模型角色不增加权限，草稿不会自动交付，私聊不会自动转发。通知是轮询加可选浏览器通知，后台节流和断网仍需在试用设备验证。真实浏览器回归使用模拟 API；它不能代替两位真人在各自电脑完成试用。

## 开发

```sh
npm ci
npm test
npm run check
cargo test --locked --manifest-path hub/Cargo.toml
npx playwright install chromium
npm run test:browser
npm run test:visual
```

`hub/` 是服务与网页，`adapters/dsh/` 是模型适配器，`scripts/` 和 `deploy/` 是运维工具，`sandbox/` 保存执行边界验证。旧静态演示可用 `python3 -m http.server 8000 --directory dist` 运行，其中 Alice/Bob、费用、MR 与执行结果均是模拟数据。

请阅读[贡献指南](CONTRIBUTING.md)和[安全说明](SECURITY.md)。项目使用 [MIT License](LICENSE)，不隶属于 DeepSeek，也不声明符合 A2A。不要在 Issue、日志、截图或提交中放入令牌、提供商密钥或真实私聊。
