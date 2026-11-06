# WSL2 本地启动：先跑 mock，再单独验收真实模型

本指南把仓库拉到自己的 Windows + WSL2 电脑，在 **WSL 的 Linux 文件系统**中运行现有 Rust Hub。默认是明确标记的 `mock`，不会调用模型、安装 DSH/Claude Code、执行编码任务或发布代码。这里提供的是准备步骤，**不是已在你的 WSL、Windows 浏览器或真实提供商上验收通过的证明**。

## 1. 确认终端与目录

在 **Windows PowerShell** 运行 `wsl --list --verbose`，确认所用发行版的 VERSION 是 `2`。以下所有命令改在该发行版的 **Linux 终端**执行，不要直接在 PowerShell 或 Windows Python 中运行。

- 准备 Git、Python 3.9+、Rust stable / Cargo；Node.js 22+ 和 npm 用于前端离线测试。按各工具的官方安装说明自行准备，本指南和 doctor 不会自动安装。
- 仓库放在 Linux 的 `$HOME/src/agenct`，不要放在 `/mnt/c`、`/mnt/d` 或其他 Windows/共享挂载中。
- 私有状态放在 checkout **之外**的 `$HOME/agenct-private`。父目录应由自己拥有且可信；不要把它放到 OneDrive、共享目录或 Git 中。DrvFS 上的 `chmod` 不能单独证明 Windows ACL 已限制访问。

首次拉取，目标目录必须尚不存在：

```sh
mkdir -p "$HOME/src"
git clone https://github.com/phil-zyx/agenct.git "$HOME/src/agenct"
cd "$HOME/src/agenct"
python3 scripts/local-doctor.py --state-dir "$HOME/agenct-private"
```

首次 clone 时 doctor 会提示 release 文件尚未构建（NOTE，不是阻碍项）；只有缺少 cargo/rustc 等真正阻碍时才返回 `1`。

doctor 只检查 OS/WSL 标记、正在使用的 Python、PATH 中的工具、现有 release 文件/UI 资产和路径挂载信息。它不执行发现的工具、不确认 Rust/Node 版本、不读取私有配置/令牌、不创建文件、不安装软件、不启动服务、不探测网络。WSL 版本和挂载识别只是启发式检查；未知情况需自己确认。退出码 `1` 表示发现阻碍项，`0` 也不代表运行时或安全验收通过。

## 2. 一条命令启动

launcher 会继承 shell 中的其他环境变量。若 doctor 提示存在 OIDC/browser/worker 配置，请先检查自己的 shell 设置，简单 mock 试用应移除 `AGENCT_OIDC_CONFIG_FILE`、`AGENCT_BROWSER_OIDC_CONFIG_FILE`、`AGENCT_WORKER_SCOPES_FILE` 的遗留设置；不要把环境变量内容或秘密贴到聊天/日志中。`runtime=mock` 并不保证这些额外配置已关闭。

```sh
python3 scripts/private-staging.py start --state-dir "$HOME/agenct-private"
```

`start` 依次完成：状态目录不存在时初始化一次（生成 alice/bob 令牌，不会覆盖或轮换已有状态）→ `cargo build --locked --release`（已是最新时只需约 1 秒）→ 检查端口 → 启动。启动前会打印访问地址、登录用户列表和令牌文件位置；令牌本身不会打印。

- 首次初始化时如果默认端口 8080 已被占用（例如本机 nginx），会自动选择 8081 起的空闲端口并打印出来。也可以用 `--port` 指定。
- 已有状态目录的端口被占用时会拒绝启动并说明原因；此时浏览器在该端口看到的页面**不是**本 Hub。用 `set-port` 改端口，不影响令牌和数据：

  ```sh
  python3 scripts/private-staging.py set-port --state-dir "$HOME/agenct-private" --port 8090
  ```
- 同一状态目录的 Hub 已在运行时，再次 `start` 会提示使用已有窗口，而不是报端口错误。

Cargo 首次构建会下载锁文件中的依赖，需网络和本机允许的构建环境。可选的离线检查（不是 WSL 浏览器或真实 DSH 验收）：

```sh
python3 -m unittest discover -s scripts -p 'test_*.py' -v
npm test
npm run check
```

`init`、`check`、`run` 分步命令仍然可用，见 [私有试运行指南](private-staging.md)。

## 3. 登录与重启

服务留在前台运行。在 Windows 浏览器打开启动时打印的地址（默认 `http://localhost:8080`）；WSL2 通常支持 localhost 转发，但这里没有验证你的机器配置。若打不开，先检查 WSL 终端是否报错、服务是否仍在运行及本机 WSL 的 localhost 转发配置。**不要改成 `0.0.0.0`、开放防火墙或用公网隧道来绕过问题。** Hub 始终仅监听 loopback。

用可信的本地私有编辑器打开 `$HOME/agenct-private/credentials.json`，取 Alice 的令牌填入登录表单。令牌不要放入 URL、截图、命令参数或提交到仓库。两人演示可用不同浏览器配置文件分别登录 Alice/Bob；只输入虚构测试内容。试用 `请让 bob 帮我检查公开文档`，检查私有提案、确认共享、接单和人工交付流程，见 [Hub 验收步骤](rust-hub.md#controlled-delegation-acceptance-offline-test-double)。

按 `Ctrl+C` 停止。以后重启使用同一条命令即可，它会复用已有状态、不会重新生成令牌：

```sh
cd "$HOME/src/agenct"
python3 scripts/private-staging.py start --state-dir "$HOME/agenct-private"
```

更新前先停止 Hub、私下备份完整状态目录，并记下当前提交。运行 `git status --short` 检查本地改动，保留自己的工作，再执行 `git pull --ff-only`；不要为更新强制丢弃本地改动。更新后 `start` 会自动重新构建；单独使用 `run` 时，若源码比 release 文件新会给出警告。数据库升级也不保证能回退。备份/恢复边界见 [私有试运行指南](private-staging.md#optional-service-operation)。

## 4. DSH / Claude Code 的边界

先验收 mock 聊天和人工委派，再切换到真实模型。需要 Python 3.11+ 和你自己的 DeepSeek API key（真实调用，会产生费用，聊天内容会发送给提供商）：

```sh
export DEEPSEEK_API_KEY=...   # 从你的密钥管理工具读取，不要写进仓库或聊天
python3 scripts/private-staging.py set-runtime --state-dir "$HOME/agenct-private" --runtime dsh
python3 scripts/private-staging.py start --state-dir "$HOME/agenct-private"
```

首次会在状态目录内创建 `dsh-venv` 并安装 `adapters/dsh/requirements.txt` 中固定版本的 SDK；之后由 launcher 同时管理 adapter 和 Hub，Ctrl+C 一起停止。key 只传给 adapter 进程，不会写入磁盘，也不会传给 Hub。没有 key 时直接拒绝启动，不会退回 mock。改回离线演示：`set-runtime --runtime mock`。详见 [DSH 适配器](dsh-adapter.md#launcher-managed-mode-recommended)。

launcher 不证明账户余额、运行时工具限制或 WSL 架构兼容性。已审查的 Linux x86_64 依赖不能推断适用于 ARM64，实际平台支持须单独核实。不得把不确定的调用自动重试，mock 也不会静默接替真实模式。

Claude Code 目前没有接入此 Hub 的可运行执行链路。你可以按它自己的说明在 WSL 中独立使用，但安装或登录它不会让本项目自动编码、上传分支、创建 MR 或运行 Agent 团队。
