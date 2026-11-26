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
- **单一大脑**：`SOUL.md` + `MEMORY.md` + 专项活账本（`ledgers`）+ 定时任务 + 分发规则 + 工具调用（`update_memory` / `update_ledger` / `manage_cron` / `manage_rule` / `fetch_gitlab_commits` / `dispatch_node` / `delegate_colleague`）。
- **单一存储**：SQLite v1（13 张规范表，WAL 模式，`spawn_blocking` 读写隔离），附件二进制落盘 + 显式 ACL 授权表，只追加审计表。
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

#### 跨平台执行后端探测（`--exec-backend`）

`node/agenct-node.py` 支持通过 `--exec-backend {auto,tmux,conpty,wt-tail,pipe}`（默认 `auto`，兼容别名 `--tmux {auto,on,off}` 分别映射到 `auto`/`tmux`/`pipe`，与 `--codex-tmux` 正交）自动选择并静默降级执行后端，并在任务回执中附带实际命中的 `backend` 字段：

- **Linux / macOS / WSL2**：`auto` 优先探测 `tmux`（命名会话 `agenct-<task>-<id>`，支持 `tmux attach -t <session>`，输出经 Wrapper 原始管道采集不折行 JSONL）；不可用时静默降级为 `pipe`。WSL2 严格按 Linux 路径处理，**不会**调用 `/mnt/c` 下的 `wt.exe`。
- **Windows**：`auto` 按 `conpty`（需探测到 `pywinpty` 或 `node-pty`，经 Wrapper 采集原始输出避免 ConPTY 折行切碎 JSONL）→ `wt-tail`（白名单调用 `wt.exe -w 0 new-tab --title "agenct:<task>" pwsh -NoExit -Command "Get-Content -Wait <log>"`，缺失或 Store 别名不可用时降级）→ `pipe` 顺序降级；运行日志写入 `%USERPROFILE%\.agenct\runs\` 而非 `/tmp`。

各平台验收步骤：

```sh
# 1. Linux / macOS / WSL2：验证默认 auto 探测（有可用 tmux 走 tmux，无则静默降级 pipe）
python3 -m unittest discover -s node -p 'test_*.py' -v
python3 node/agenct-node.py --self-check --agents codex --exec-backend auto
# 兼容别名验证（与 --codex-tmux 互不影响）
python3 node/agenct-node.py --self-check --agents codex --tmux off
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
