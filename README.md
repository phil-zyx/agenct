# Agenct v2

极简单栈的 7×24h 个人超级智能体平台：**一个 Rust 二进制（Hub） + 一套原生 ESM 网页 + 一套统一执行节点协议（PC / 云端容器通用）**。

## 架构一览

```text
Web (原生 ESM + 单连接 SSE) ──▶ Rust Hub (单一 SQLite v1) ──▶ OpenAI 兼容 LLM (DeepSeek/OpenAI/Ollama)
                                      │
                    ┌─────────────────┴─────────────────┐
                    ▼                                   ▼
         PC Node (个人电脑)                  Cloud Node (独立 Docker 容器)
      本地代码 / tmux / 私有环境              7×24h 云端脚本 / 浏览器 / 通用执行
```

**只保留一套（零双轨、零历史包袱）：**
- **认证**：单人模式的本地登录令牌（零配置），或可选的 GitLab OAuth 2.0 团队登录（PKCE S256）；均为 SQLite 会话 + CSRF/Origin/Host 校验；节点使用 `agn_` Bearer Token。
- **单一运行时**：Hub 直连任意 OpenAI 兼容 `/chat/completions` 接口（支持 DeepSeek、OpenAI、Ollama）。每个用户可在「设置 → 大脑模型」配置自己的大脑模型（可标记支持图片）和可选的识图模型，未配置时使用服务器默认（`AGENCT_LLM_*` / `AGENCT_VISION_*`）。
- **单一大脑**：`SOUL.md` + `MEMORY.md` + 专项活账本（`ledgers`）+ 定时任务 + 分发规则 + 工具调用（`update_memory` / `update_ledger` / `manage_cron` / `manage_rule` / `manage_role` / `manage_node` / `manage_thread` / `fetch_gitlab_commits` / `dispatch_node` / `delegate_colleague`）。
- **单一存储**：SQLite（18 张规范表 + FTS5 全文检索，WAL 模式，`spawn_blocking` 读写隔离），附件二进制落盘 + 显式 ACL 授权表，只追加审计表。
- **单一执行协议**：本地 PC 节点与云端常驻容器节点共用同一份 `node/agenct-node.py` 和排他租约协议 2。
- **单一实时通道**：`/api/bootstrap` 初始加载 + `/api/events` 真 SSE 推送。

## 快速启动

### 0. 最快试用（无需 GitLab）

```sh
docker run -d --name agenct -p 127.0.0.1:8080:8080 -v agenct-data:/data \
  -e AGENCT_LLM_API_KEY=sk-... ghcr.io/phil-zyx/agenct:latest
docker logs agenct 2>&1 | grep "login token"   # 首次启动打印的登录令牌
```

打开 http://localhost:8080 ，输入登录令牌即可。不配 GitLab 时为单人（owner）模式；令牌丢失可停掉容器后执行 `agenct-hub reset-login-token`。

### 1. Docker 部署（推荐）

```sh
cp deploy/agenct.env.example deploy/agenct.env
# 编辑 deploy/agenct.env，填入 GitLab OAuth 与 LLM API Key

cd deploy
docker compose up -d --build
```

如需同时启用 7×24h **云端常驻执行节点**（个人电脑关机时也能跑代码/脚本）：
1. 在网页「工作台 → 任务与节点」新增一个 `Cloud` 类型节点并复制令牌；
2. 设置 `AGENCT_CLOUD_NODE_TOKEN=agn_...` 并启动：

```sh
docker compose --profile cloud-node up -d --build
```

### 2. 本地源码运行

```sh
export AGENCT_BIND=127.0.0.1:8080
export AGENCT_PUBLIC_ORIGIN=http://127.0.0.1:8080
export AGENCT_ALLOW_HTTP_ORIGIN=1
export AGENCT_GITLAB_URL=https://git.example.com
export AGENCT_GITLAB_CLIENT_ID=...
export AGENCT_GITLAB_CLIENT_SECRET=...
export AGENCT_LLM_URL=https://api.deepseek.com/v1/chat/completions
export AGENCT_LLM_API_KEY=...
export AGENCT_LLM_MODEL=deepseek-chat

cargo run --manifest-path hub/Cargo.toml
```

### 3. 接入本地 PC 节点

在网页「工作台 → 任务与节点」点击添加 `PC` 节点，复制一次性接入命令在目标电脑运行：

```sh
curl -fsSL https://agenct.example.com/node/install.sh | sh -s -- agn_...
```

节点默认运行在 `workspace-write` 沙箱模式；仅当显式传入 `--unsafe-full-access` 时才开放全主目录访问。

对于 `codex` 节点，默认显式传入 `sandbox_workspace_write.network_access=false` 关闭外网（不依赖 Codex 隐式默认值）。Linux 下 Codex 关闭网络时 seccomp 会一并拦截 `AF_UNIX connect()`（导致无法连接 `/tmp/tmux-*/default` 执行 `tmux ls` 等）；如确需让 Codex 访问本地 tmux，可在启动节点时显式传入 `--codex-tmux`（仅在 `--sandbox workspace-write` 下生效，`readonly` 与 `unsafe-full` 下会自动忽略，且会同时开放 `workspace-write` 内的外网访问）。验收探测步骤：

```sh
# 1. 默认关闭外网：tmux socket 与外网探测均应被拦截
codex exec --skip-git-repo-check -s workspace-write -c sandbox_workspace_write.network_access=false \
  'Run `tmux ls 2>&1 || true` and `curl -sfI --max-time 5 https://example.com 2>&1 || echo NET_BLOCKED`'

# 2. 启用 --codex-tmux（仅 workspace-write 生效）：自检并探测 tmux socket 与外网
python3 node/agenct-node.py --self-check --agents codex --sandbox workspace-write --codex-tmux
codex exec --skip-git-repo-check -s workspace-write -c sandbox_workspace_write.network_access=true \
  'Run `tmux ls 2>&1 || true` and `curl -sfI --max-time 5 https://example.com | head -n 1`'
```

#### 跨平台执行后端矩阵（`--exec-backend`）

`node/agenct-node.py` 通过 `--exec-backend {auto,tmux,conpty,wt-tail,pipe}`（默认 `auto`，可用环境变量 `AGENCT_EXEC_BACKEND` 设置，CLI 参数优先于环境变量，非法环境变量会打印告警并回退 `auto`；与 `--codex-tmux` 正交）自动选择并静默降级执行后端，并在任务回执中上报实际命中的 `backend` 字段：

| 后端 | 适用平台 | 探测条件 | 探测失败 / 运行期 `spawn` 失败静默降级顺序 | 日志与观察方式 |
| --- | --- | --- | --- | --- |
| `auto` | 全平台（Linux / macOS / WSL2 / Windows） | Linux / macOS / WSL2 探测 `tmux`；Windows 依次探测 `conpty`、`wt-tail` | **Linux / macOS / WSL2**：`tmux` → `pipe`<br>**原生 Windows**：`conpty` → `wt-tail` → `pipe` | 按实际命中的后端决定 |
| `tmux` | Linux / macOS / **WSL2**（不含原生 Windows） | `shutil.which("tmux")` 存在、`tmux -V` 返回 0、且 `tmux list-sessions` 未出现权限/建 socket 失败 | `pipe` | `/tmp/agenct-runs/tmux-<task>-<rand>/{stdout.log,stderr.log}`（Wrapper 原始管道采集，结束后自动清理）；支持 `tmux attach-session -t agenct-<task>-<id>` |
| `conpty` | 仅原生 Windows | 优先检测 `import winpty`（`pywinpty`，需含 `PtyProcess` 或 `PTY`），其次检测 `node -e "require.resolve('node-pty')"` | 显式指定 `--exec-backend conpty` 失败直接降级 `pipe`（`auto` 模式下会先尝试 `wt-tail` 再降级 `pipe`） | `%USERPROFILE%\.agenct\runs\conpty-<task>-<rand>/{stdout.log,stderr.log}`（Wrapper 原始管道采集避免 ConPTY 折行切碎 JSONL，结束后自动清理）；无 `attach` 命令 |
| `wt-tail` | 仅原生 Windows | `shutil.which("wt.exe" / "wt")` 存在、路径不以 `/mnt/` 开头、且 `wt.exe --version` 返回 0（过滤失效 Store 别名） | `pipe` | `%USERPROFILE%\.agenct\runs\agenct-<task>-<id>.log`（实时 tee 落盘，结束后保留）；白名单拉起 `wt.exe -w 0 new-tab --title agenct:<task> pwsh -NoExit -Command "Get-Content -Wait <log>"` |
| `pipe` | 全平台 | 无（兜底后端，始终可用） | 无（最底兜底） | 仅内存管道收集，不写本地运行日志，无 `attach` 命令 |

- **WSL2 平台判定**：`is_windows()` 仅在原生 Windows 返回 `True`；WSL2 严格按 **Linux** 路径处理（通过 `tmux → pipe`），**不会**调用 `/mnt/c` 下的 `wt.exe`（`probe_wt_exe` 亦会主动拒绝 `/mnt/` 路径）。
- **旧参数 `--tmux` 迁移映射**：历史版本曾提供 `--tmux {auto,on,off}` 兼容别名，对应关系为 `--tmux auto` → `--exec-backend auto`、`--tmux on` → `--exec-backend tmux`、`--tmux off` → `--exec-backend pipe`。按仓库「零双轨」原则，**`--tmux` 参数已于提交 `8191759` 移除**（继续传入会触发 `unrecognized arguments` 参数错误），请统一改用 `--exec-backend` 或 `AGENCT_EXEC_BACKEND`。
- **静默超时保护（`--idle-timeout`）**：默认 `300s`（环境变量 `AGENCT_NODE_IDLE_TIMEOUT`，CLI 优先，`0` 禁用）。子进程连续静默满 `60s` 起每 `15s` 上报 `（已静默 Ns）` 进度；达到阈值后强制终止进程组/PTY，返回 `[idle-timeout]` 失败回执（Protocol 2 上报 `status="failed"`）并让出队列继续执行后续任务。

各平台自检步骤（Windows 真机完整逐条验收见 [docs/windows-acceptance.md](docs/windows-acceptance.md)）：

```sh
# 1. Linux / macOS / WSL2：验证默认 auto 探测（有可用 tmux 走 tmux，无则静默降级 pipe）
python3 -m unittest discover -s node -p 'test_*.py' -v
python3 node/agenct-node.py --self-check --agents codex --exec-backend auto
python3 node/agenct-node.py --self-check --agents codex --exec-backend pipe
# 若命中 tmux，另开终端可查看/附着会话：
tmux list-sessions | grep '^agenct-'
```

```powershell
# 2. Windows（原生 PowerShell）：
# 2a) 默认 auto 探测（已装 pywinpty/node-pty 走 conpty；否则若 wt.exe 可用走 wt-tail；否则降级 pipe）
py -3 -m unittest discover -s node -p "test_*.py" -v
py -3 node\agenct-node.py --self-check --agents codex --exec-backend auto

# 2b) 指定 wt-tail：确认日志落盘在 %USERPROFILE%\.agenct\runs\ 且 Windows Terminal 新标签页打开 tail
py -3 node\agenct-node.py --self-check --agents codex --exec-backend wt-tail
dir "$env:USERPROFILE\.agenct\runs"

# 2c) 卸载 pywinpty / 禁用 wt.exe Store 别名时，确认 --exec-backend auto 静默降级为 pipe 不抛错
py -3 node\agenct-node.py --self-check --agents codex --exec-backend pipe
```

#### 沙箱边界说明

- **默认收敛、只收紧不放开**：节点默认以 `--sandbox workspace-write` 运行，仅允许写入当前工作区、任务临时 I/O 与引擎会话状态目录，默认**不开放**全主目录访问。Hub 下发的任务 `sandbox` 参数经过 `tighten_sandbox()` 校验，**只能向下收紧**（例如 `workspace-write` → `readonly`），无法单方面提升节点沙箱级别。
- **全量主机访问与单次人工放行（Elevation）**：
  - 永久开放全主目录访问（`unsafe-full`）必须由节点操作者在宿主机显式传入 `--unsafe-full-access`。
  - 日常保持 `workspace-write` 时，如需偶发执行特权操作，可在节点启动参数中显式开启 `--allow-elevated`（或 `AGENCT_ALLOW_ELEVATED=1`）；由 Hub 通过 `require_elevation=true` 创建 `pending` 任务并生成 6 位 `/approve <审批码>`（或对失败单人工发起 elevated 重试），节点仅在 `allow_elevated=True` 且 `job.sandbox == "elevated"`、`job.authorization == "manual"` 同时满足时，对该单次任务放行至 `direct / unsafe-full`。
- **外层 OS 隔离与宿主机凭据不可见**：`--sandbox-backend auto`（或 `AGENCT_SANDBOX_BACKEND`）在 Linux/WSL2 自动启用 `bwrap`（mount + PID 命名空间），macOS 启用 `seatbelt`（`sandbox-exec`），不支持 OS 隔离的宿主（如 Windows 或禁用 userns 的容器）降级为 `cli-native`。所有非 `unsafe-full` 任务均执行双重凭据隔离：
  - **路径遮蔽**：`bwrap` 对目录挂载空 `--tmpfs`、对文件 `--ro-bind /dev/null`，`seatbelt` 写入 `deny file-read* file-write*`，统一隐藏 `~/.ssh`、`~/.agenct`（及节点 `--state-dir`）、`~/.aws`、`~/.gnupg`、`~/.kube`、`~/.netrc`、`~/.git-credentials`、`~/.npmrc`、`~/.pypirc`、`~/.docker`、`~/.config/gh`、`~/.config/glab-cli`、`~/.config/git/credentials`。
  - **环境变量清洗**：非 `unsafe-full` 下自动剥离 `SSH_AUTH_SOCK`、`SSH_AGENT_PID`、`GIT_ASKPASS`、`SSH_ASKPASS`、`GH_TOKEN`、`GITHUB_TOKEN`、`GH_ENTERPRISE_TOKEN`、`GITHUB_ENTERPRISE_TOKEN`、`GITLAB_TOKEN`、`GITLAB_PRIVATE_TOKEN`、`GLAB_TOKEN`、`AWS_ACCESS_KEY_ID`、`AWS_SECRET_ACCESS_KEY`、`AWS_SESSION_TOKEN`、`NPM_TOKEN`（节点自身密钥如 `AGENCT_NODE_TOKEN`、`AGENCT_MASTER_KEY` 等在**任何**模式下均会从子进程剥离）。
- **需要凭据的操作走正门或宿主机**：因上述隔离，沙箱内执行 `git push`、远端 SSH / 部署等必然缺少凭据；请通过 Hub 人工确认的 elevation 单次放行（`--allow-elevated` + `/approve`）或在宿主机直接执行，不要在沙箱内绕行探测凭据。
- **PID 命名空间限制**：Linux/WSL2 下 `bwrap` 启用了 `--unshare-pid` 与独立 `--proc /proc`，因此在沙箱内执行 `ps` 只能看到当前任务 PID 命名空间内的进程，无法查看宿主机其他进程。

#### 已知坑与排查说明

- **`bwrap` 下 `~/.ssh` 为空导致 `git@github.com` 报 `Permission denied (publickey)`**：这是 `bwrap` 将 `~/.ssh` 遮蔽为空 `tmpfs` 且剥离 `SSH_AUTH_SOCK` 导致的预期隔离行为。推送代码请在**宿主机**直接执行（若宿主机 SSH 不通，可在宿主机侧切换为 `gh` 的 HTTPS 凭据通道如 `gh auth setup-git` 完成推送），或在节点开启 `--allow-elevated` 后经 `/approve` 单次放行；不要误判为 SSH 密钥损坏（注意沙箱内 `~/.config/gh` 与 `GH_TOKEN` 同样被遮蔽）。
- **各平台日志路径差异**：
  - Linux / macOS / WSL2：`tmux` 后端临时运行目录位于 `/tmp/agenct-runs/tmux-<task>-<rand>/`（含 `stdout.log` / `stderr.log`，正常退出后自动清理）；若需持久化留档，可在宿主机独立 tmux 会话内自行用 `| tee <logfile>` 落盘。
  - 原生 Windows：运行日志统一位于 `%USERPROFILE%\.agenct\runs\`（而非 `/tmp`）。其中 `conpty` 使用子目录 `conpty-<task>-<rand>\`（结束后自动清理），`wt-tail` 写入 `agenct-<task>-<id>.log`（实时 tee 落盘且结束后保留）。
  - `pipe` 后端仅内存收集，不生成本地运行日志文件。
- **节点任务输出为管道收集、执行完一次性回传**：`run_monitored()` 将子进程输出持续收入内存缓冲，运行期间仅周期性（≥3s 有进展或每 15s 心跳）向 Hub 推送最近 ≤5 行精简摘要（`log_tail`，上限约 850 字节），完整正文在任务结束后才经 `/node/v1/tasks/{id}/submit` 一次性提交（超出 16,000 字节会截断）。若需实时观察完整滚动输出，Linux / macOS / WSL2 请 `tmux attach-session -t <agenct-...>`（或在 tmux 会话内配合 `tee`），Windows 请使用 `wt-tail` 自动拉起的 Windows Terminal `Get-Content -Wait` 标签页。

## 备份与恢复

```sh
python3 scripts/backup.py backup --data-dir ./deploy/data --archive ./agenct-backup.zip
python3 scripts/backup.py restore --archive ./agenct-backup.zip --data-dir ./deploy/data-restored
```

## 开发验证

```sh
cargo fmt --manifest-path hub/Cargo.toml -- --check
cargo clippy --manifest-path hub/Cargo.toml --all-targets -- -D warnings
cargo test --manifest-path hub/Cargo.toml
python3 -m unittest discover -s node -p 'test_*.py' -v
npm run check
```
