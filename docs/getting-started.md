# 入门：从聊天到人工交付

Agenct 让每个人在自己的聊天窗口中与管家协作，再把经过确认的任务交给另一位参与者。Rust Hub 保存私聊、任务、草稿和回执；浏览器里的 `dist/` 页面是独立脚本演示，两者不共享数据。

当前试用方式是一台可信主机运行 Hub，每位参与者通过 SSH 隧道使用自己的浏览器和令牌。真实模型模式使用 DeepSeek；默认 `mock` 是离线演示，不调用模型。服务尚未完成公网生产部署，也没有启用代码执行或自动发布。接口、运维和验收门槛见[参考](reference.md)。

## 1. 启动本机服务

在 Linux 或 WSL 环境准备 Python 3.9+、Rust stable 和 Cargo。开发测试另需 Node.js 22+。从仓库根目录执行：

```sh
python3 scripts/private-staging.py start --state-dir "$HOME/agenct-private"
```

首次启动会创建私有状态目录、生成 Alice/Bob 的独立令牌并构建 release Hub；后续启动沿用数据库和身份，不重新初始化。默认从 8080 起选择空闲端口；以终端显示地址为准。

用可信的本地编辑器打开状态目录中的 `credentials.json`，把自己的令牌填入页面登录框。不要把整个文件发给参与者；每人只取得自己的令牌。令牌保存在当前标签页的会话存储（sessionStorage）中，刷新会自动验证并恢复登录；主动退出或验证失败会清除令牌。不会写入长期存储（localStorage）。浏览器 OIDC 模式是另行配置的功能。

服务通过 `Ctrl+C` 停止。更改端口使用：

```sh
python3 scripts/private-staging.py set-port --state-dir "$HOME/agenct-private" --port 8081
```

如果只想看不带服务端的旧演示，可执行 `python3 -m http.server 8000 --directory dist`。那里切换 Alice/Bob 只是切换模拟身份，不代表真实访问控制。WSL 安装与诊断细节保留在[历史环境指南](archive/wsl-local.md)。

## 2. 接入真实模型

通过自己的秘密管理方式把 `DEEPSEEK_API_KEY` 注入当前 shell，然后执行：

```sh
python3 scripts/private-staging.py set-runtime --state-dir "$HOME/agenct-private" --runtime dsh
python3 scripts/private-staging.py start --state-dir "$HOME/agenct-private"
```

launcher 在状态目录安装固定版本的 DSH 依赖，并一起管理 adapter 与 Hub。密钥交给 adapter，不交给网页，也不由 launcher 写入磁盘。真实模型调用和长材料摘要会产生费用；缺少配置或模型失败时不会退回 mock。`set-runtime --runtime mock` 可切回离线演示。

如要单独运行 adapter 或调整服务器模型，参阅[参考中的模型与上下文](reference.md#模型与上下文)及[详细接入记录](archive/dsh-adapter.md)。

## 3. 完成一笔委派

两个人分别使用自己的浏览器；本机演练可用两个独立浏览器配置文件。

1. Alice 输入 `请让 bob 帮我检查公开文档`，或 `/delegate bob 检查公开文档`。mock 只识别有限固定句式，真实模型才负责一般自然语言理解。
2. 页面生成 Alice 私有的委派提案。确认接收人、共享摘要；需要调整时点击“修改后确认”，由 Alice 手动改写后预览。确认之前 Bob 看不到提案。
3. 按需附带共享材料。可以粘贴文字、选择 UTF-8 文本/Markdown，或上传可提取文字的 PDF。查看提取内容后再确认共享；完整私聊不会随任务转发。
4. 点击“确认共享并委派”。只有这一步创建任务。拒绝提案不共享内容；超过一天的待确认提案收在折叠组中，可批量拒绝。
5. Bob 在任务区接手任务。标签标题显示待处理数；可主动开启系统通知。页面定期读取更新，后台浏览器节流和断网会影响到达时间。
6. Bob 点击“接手”后，Hub 会在后台用系统推荐的角色（按任务内容，从本人已启用的写作、审阅或研究角色中选）自动起草，卡片显示“Agent 正在自动起草”。完成后点“审阅 Agent 草稿”打开。草稿只有 Bob 可见，不会自动交付；任务缺少关键信息时，草稿开头会列出“需要补充的信息”。也可以手写结果，或在草稿面板里改选角色后重新起草。被退回的任务重新接手后，会针对新版本再自动起草一次。
7. 对某一草稿填写修改要求并生成新版本，例如“更简短，保留依据”。修改基于该版已保存的草稿；历史保留。草稿框里的临时手改不会自动成为模型修改的输入。
8. Bob 人工审阅并编辑，选择“使用此草稿填入人工交付”，再点击“提交人工交付”。在卡片内核对多行文本、预览并“确认交付”，对方才看到结果。
9. Alice 验收或退回。退回后 Bob 可继续处理；历史交付和双方可见时间线保留。取消任务需要填写原因并确认。

任务状态变化时，旧版本的按钮或草稿不能悄悄覆盖新状态。遇到提示后先刷新和重新审阅。

## 4. 常用交互与恢复

- “工作”汇总提案与任务；任务列表可以筛选待我处理、我发起的和已完成。完成项默认收起，归档只影响显示，不删除记录。
- 管家回复支持安全 Markdown，代码块可复制。消息操作支持复制、生成委派提案和“重新问”；重新问只填入输入框，仍需自己发送。
- 个人角色设置以卡片管理角色的名称、启用状态和说明。角色影响提示词，不增加工具权限或数据访问权。每次聊天仍只选择一个角色。
- “讨论此任务”只把明确选择的任务快照附给下一条普通聊天；不会接受、交付或验收任务。
- `Enter` 发送，`Shift+Enter` 换行；中文输入法确认候选时不会触发发送。`Esc` 关闭当前面板；快捷键和新手引导以界面提示为准。
- 刷新后登录，页面会自动只读查询未完成聊天的回执。查到结果就显示回复；未知时保留恢复面板。不会因刷新、轮询或重新登录自动重发。
- 起草请求未知时先“核对起草请求”。主动开始新尝试可能重复模型工作和费用；旧编号仍可读取，但不能再次调度。

## 5. 两人分别在自己的电脑上使用

先在一台双方有权访问的主机启动 Hub。每位参与者在自己的电脑建立隧道：

```sh
ssh -N -o ExitOnForwardFailure=yes -L 127.0.0.1:18080:127.0.0.1:8080 YOUR_EXISTING_SSH_HOST
```

打开本机 `http://127.0.0.1:18080`。把远端 8080 改成 Hub 的实际端口。正常校验 SSH 主机密钥，保持隧道运行；无需将 Hub 监听改成公网地址。

常驻运行使用仓库的 [systemd 模板](../deploy/agenct-private.service.example)。先构建，再由操作者调整服务账号、仓库和状态目录绝对路径，安装到共享主机的 `/etc/systemd/system/agenct-private.service` 后启动：

```sh
sudo systemctl daemon-reload
sudo systemctl enable --now agenct-private
sudo systemctl status agenct-private
```

模板配置崩溃重启和开机启动，默认针对 mock；DSH 的独立 adapter 与密钥管理须另行配置。模板存在不代表已在任何远端机器安装。

## 6. 管理参与者、备份和试用记录

在线加人或更换令牌不需要重新初始化：

```sh
python3 scripts/private-staging.py add-user --state-dir "$HOME/agenct-private" --user carol
python3 scripts/private-staging.py rotate-token --state-dir "$HOME/agenct-private" --user alice
python3 scripts/private-staging.py backup --state-dir "$HOME/agenct-private" --backup-file "$HOME/agenct-backup.sqlite3"
```

换令牌成功后，旧令牌在下一次请求失效；已经通过鉴权的请求可能继续完成。备份包括 SQLite WAL 中已提交的数据。恢复会替换数据库，先停止 Hub/服务：

```sh
python3 scripts/private-staging.py restore --state-dir "$HOME/agenct-private" --backup-file "$HOME/agenct-backup.sqlite3"
python3 scripts/private-staging.py run --state-dir "$HOME/agenct-private"
```

另行保护 `config.json`、`credentials.json`，数据库备份不包含这些文件。恢复权限、升级和日志说明见[参考](reference.md#运行与数据保护)。

每天可以生成不含正文的试用报告，日期使用 UTC：

```sh
python3 scripts/trial-report.py --state-dir "$HOME/agenct-private" --date 2026-11-06
```

可用 `--missed-notifications` 和 `--model-timeouts` 填入人工记录的漏提醒、模型超时次数；未填写显示未知。确认率/拒绝率按当日创建提案的当前状态统计；人工编辑后交付可能无法对应原草稿，采用率是可识别部分的下界。

试用验收需要两个人在各自电脑完成委派、交付、验收，重启共享主机并核对数据，实际恢复备份，再记录一周使用情况。单机测试和自动浏览器回归不能代替这些步骤。
