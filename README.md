# Agenct

一个以个人管家聊天窗口为入口，连接成员、项目与本地 PC 节点的 Agent 协作原型。默认流程是：私聊 → 私有委派提案 → 人工确认共享 → 对方接手 → 私密起草与多轮修改 → 人工交付 → 验收或退回；也可单独授权节点自动接单、执行与交付。

Rust Hub 提供真实服务端存储与身份边界；默认 `mock` 不调用模型，`dsh` 接入 DeepSeek。`dist/` 是独立的浏览器脚本演示，不代表真实账号或多人同步。当前用于可信主机上的小范围试用，尚未完成公网生产部署；内置 Worker 沙箱执行仍关闭。本地 PC 节点可调用已安装的 Agent CLI 执行任务；跨人自动接单、自动交付、GitLab 自动执行与评论回写须在账号设置中分别授权，默认关闭。

## 开始使用

在 Linux/WSL 准备 Python 3.9+、Rust stable，从仓库根目录启动：

```sh
python3 scripts/private-staging.py start --state-dir "$HOME/agenct-private"
```

打开终端显示的本机地址，通过私有状态目录 `credentials.json` 中自己的令牌登录。初次构建需要等待；后续启动保留身份和数据。

- [入门](docs/getting-started.md)：模型接入、完整委派与草稿修改流程、双电脑 SSH 使用、在线加人和备份恢复。
- [技术与运行参考](docs/reference.md)：架构、API、角色与摘要、文件材料、数据迁移、日志、测试与验收门槛。
- [GitLab 接入](docs/gitlab-login.md)：登录与成员管理、Webhook、自动执行和评论回写权限。
- [Docker 内网部署](docs/docker-intranet.md)：容器运行、升级与备份恢复。
- [历史开发计划](docs/next-steps-plan.md)：早期规划与验收记录，当前能力以本文和技术参考为准。
- [个人助手与成员协作](docs/collaboration.md)：公开资料、私有偏好、联系人会话、助手代答与交付验收。
- [界面用语](docs/ui-terminology.md)：产品用词约定。

## 当前能力

通过聊天整理并确认个人资料、私有背景偏好和助手权限；双人联系人会话、未读提示及明确标记的公开资料自动答复；私聊持久化与只读回执恢复；可编辑确认的委派提案；文本/Markdown/PDF 共享材料预览；任务筛选和提醒；角色设置与起草推荐；长聊天和大材料摘要；保留版本的私密改稿；人工多行交付预览；共享时间线；在线用户/令牌管理；在线数据库备份、离线完整状态备份恢复和无正文运行日志。可选 OIDC 组织映射限制跨组织委派；通过本地固定边界检查后，可配置白名单仓库的只读提交上下文。

临时或持续项目支持目标、分工、阻塞与待决定项的[共同状态](docs/project-shared-state.md)。私聊中可逐条选择“提为项目候选”，明确选项目、编辑公开文字、预览并确认后才发布为待确认建议。共享条目支持明确引用版本的“共享问答”：成员预览确认后提问或回复，反馈不自动变成承诺或共识，旧版本问答仅供历史参考。 明确选择项目后，可与自己的 Agent 私下讨论有界的共同状态和共享问答；保留作者、版本和撤回标记，不读取他人的私聊或任务正文，也不会自动发布建议或确认分工。

模型角色不增加权限，DSH 私密草稿不会自动交付，私聊不会自动转发；节点结果仅在启用自动交付权限时自动共享。通知是轮询加可选浏览器通知，后台节流和断网仍需在试用设备验证。真实浏览器回归使用模拟 API；它不能代替两位真人在各自电脑完成试用。

## 接入本地 PC 节点

1. 在账号设置的“我的 PC 节点”中添加电脑，复制对应系统的一键接入命令，在该电脑终端运行。节点电脑须能访问 Hub；支持 Linux/macOS 与 Windows。
2. 节点会识别本机已安装的 Claude Code、Codex、Gemini CLI 或 Ollama；未配置可用引擎时使用内置草稿模板。实际执行使用该电脑上的工具与权限。
3. 在任务卡片上选择 PC 节点并指派。跨人任务的结果默认保持私有草稿，由本人审阅后交付。

账号设置提供四项独立授权，**默认全部关闭**：自动接手他人委派并调度节点、自动交付节点草稿、GitLab 提及后自动执行、GitLab 评论回写。注册节点、配置分发规则或保存 PAT 均不会开启这些权限。Webhook 必须鉴权，评论回写只使用用户配置且与事件来源匹配的 GitLab 地址，详见 [GitLab 接入](docs/gitlab-login.md#gitlab-webhook-与回写边界)。

节点协议 2 使用执行租约和本地持久化记录：提交超时只重传已保存的结果，重启发现执行状态不明时保留记录供人工核对，不自动重跑。撤销授权或租约过期会阻止新结果提交，但不能终止已经启动的本机进程或撤回外部操作。详见[节点执行与恢复](docs/reference.md#本地节点执行租约与结果恢复)。

## 备份、恢复与升级

`backup` 只生成在线 SQLite 快照。需要同时保存附件、自动化配置与私有身份凭据时，先停止 Hub，再执行完整备份：

```sh
python3 scripts/private-staging.py backup-state --state-dir "$HOME/agenct-private" --backup-file "$HOME/agenct-state.zip"
python3 scripts/state_backup.py verify --archive "$HOME/agenct-state.zip"
python3 scripts/private-staging.py restore-state --state-dir "$HOME/agenct-restored" --backup-file "$HOME/agenct-state.zip"
```

备份文件须使用新路径，恢复目录也必须尚不存在。归档包含私有数据与凭据，需妥善保护；状态目录外的文件、环境变量中的密钥和各 PC 的本地执行记录需单独备份。恢复后未完成节点派发标记为失败，定时任务与 GitLab 自动轮询暂停；核对数据和外部依赖后再启动恢复实例、重新指派或开启自动化。完整边界见[技术参考](docs/reference.md#完整应用状态备份与恢复)。

从旧版升级前先备份。数据库迁移会停止不符合新授权或租约要求的旧派发，保留已有任务和结果；节点协议 2 要求重新安装 Hub 提供的节点脚本，旧客户端无法领取任务。重新指派前应确认旧进程已停止。

## 开发

前端工具需要 Node.js 22+。从仓库根目录执行：

```sh
npm ci
npm test
npm run check
cargo test --locked --manifest-path hub/Cargo.toml
python3 -m unittest discover -s scripts -p 'test_*.py'
npx playwright install chromium
npm run test:browser
npm run test:hub-browser
npm run test:visual
```

`hub/` 是服务与网页，`adapters/dsh/` 是模型适配器，`scripts/` 和 `deploy/` 是运维工具，`sandbox/` 保存执行边界验证。旧静态演示可用 `python3 -m http.server 8000 --directory dist` 运行，其中 Alice/Bob、费用、MR 与执行结果均是模拟数据。

请阅读[贡献指南](CONTRIBUTING.md)和[安全说明](SECURITY.md)。项目使用 [MIT License](LICENSE)，不隶属于 DeepSeek，也不声明符合 A2A。不要在 Issue、日志、截图或提交中放入令牌、提供商密钥或真实私聊。
