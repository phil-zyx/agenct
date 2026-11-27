# Windows 真机验收清单

本清单用于**原生 Windows 10/11**（非 WSL2）真机环境下，逐项验证 `node/agenct-node.py` 的跨平台执行后端探测/降级、参数与环境变量行为、静默超时队列保护，以及日志与 Windows Terminal 实时滚动能力。

> **说明**：所有命令默认在仓库根目录（例如 `C:\Users\<you>\projects\agenct`）的原生 PowerShell 7（`pwsh`）中执行；若仅使用单测或 `--cmd` 冒烟，无需安装全部 AI CLI。

---

## 0. 前置环境与基线检查

- [ ] **0.1 确认运行在原生 Windows（非 WSL2）且 Python 3 可用**
  ```powershell
  py -3 -c "import platform; print(platform.system(), platform.release())"
  ```
  - **预期结果**：输出 `Windows 10` 或 `Windows 11`（不出现 `Linux` / `microsoft`）。
  - **判定标准**：`platform.system()` 为 `Windows`。

- [ ] **0.2 检查 `pwsh`（PowerShell 7+）与 Windows Terminal（`wt.exe`）**
  ```powershell
  Get-Command pwsh -ErrorAction SilentlyContinue | Select-Object Source
  Get-Command wt.exe -ErrorAction SilentlyContinue | Select-Object Source
  wt.exe --version
  ```
  - **预期结果**：`pwsh` 可解析，`wt.exe --version` 返回版本号且 `$LASTEXITCODE` 为 `0`。
  - **判定标准**：`wt-tail` 后端拉起标签页时固定调用 `pwsh -NoExit -Command "Get-Content -Wait ..."`；若本机缺少 `pwsh`，需先安装 PowerShell 7 再验收场景 2 与场景 6。

- [ ] **0.3 运行节点全量单元测试基线**
  ```powershell
  py -3 -m unittest discover -s node -p "test_*.py" -v
  ```
  - **预期结果**：全部测试用例通过，退出码为 `0`。
  - **判定标准**：无 `FAIL` / `ERROR`。

---

## 1. ConPTY 正常路径验收（需 `pywinpty`）

- [ ] **1.1 安装 `pywinpty` 并确认探测命中 `pywinpty`**
  ```powershell
  py -3 -m pip install pywinpty
  py -3 -c "import importlib.util, pathlib; spec = importlib.util.spec_from_file_location('n', pathlib.Path('node/agenct-node.py')); m = importlib.util.module_from_spec(spec); spec.loader.exec_module(m); print('probe_conpty =', m.probe_conpty()); print('resolved =', m.resolve_exec_backend('auto').name)"
  ```
  - **预期结果**：打印 `probe_conpty = pywinpty` 与 `resolved = conpty`。
  - **判定标准**：`auto` 优先命中 `conpty` 且 provider 为 `pywinpty`。（如使用 `node-pty` 替代测试，应输出 `probe_conpty = node-pty` 与 `resolved = conpty`。）

- [ ] **1.2 通过 `--self-check` 或真实任务验证 `conpty` 执行与 JSONL 完整性**
  ```powershell
  # 若本机已安装 codex/claude/agy/opencode 之一（以 codex 为例）：
  py -3 node\agenct-node.py --self-check --agents codex --exec-backend auto
  py -3 node\agenct-node.py --self-check --agents codex --exec-backend conpty
  ```
  - 无 AI CLI 时可用 `ConPtyBackend` 真实 spawn 冒烟（验证长 JSON 行不被 ConPTY 80/120 列折行切碎）：
  ```powershell
  py -3 -c "import importlib.util, json, os, pathlib, threading; spec = importlib.util.spec_from_file_location('n', pathlib.Path('node/agenct-node.py')); m = importlib.util.module_from_spec(spec); spec.loader.exec_module(m); meta = {}; long_val = 'x' * 260; code = 'import json; print(json.dumps({\"type\":\"item.completed\",\"item\":{\"type\":\"agent_message\",\"text\":\"' + long_val + '\"}}))'; rc, out, err, c = m.run_monitored([os.sys.executable, '-c', code], shell=False, cwd=os.getcwd(), env=dict(os.environ), timeout=30, cancel_event=threading.Event(), progress_cb=lambda *a: None, stage_label='smoke', backend=m.resolve_exec_backend('conpty'), task_id='win-conpty-1', meta_out=meta); obj = json.loads(out.decode('utf-8').strip()); print('rc=', rc, 'backend=', meta.get('backend'), 'len=', len(obj['item']['text']))"
  ```
  - **预期结果**：`--self-check` 首行打印 `exec_backend=auto(conpty)` / `exec_backend=conpty(conpty)` 且检查 `PASS`；冒烟脚本打印 `rc= 0 backend= conpty len= 260`。
  - **判定标准**：`backend` 为 `conpty`，260+ 字符单行 JSON 未被终端折行破坏、解析成功，且任务结束后 `%USERPROFILE%\.agenct\runs\conpty-*` 临时目录被自动清理。

---

## 2. ConPTY 缺失时降级到 `wt-tail`（只读 tail）

- [ ] **2.1 移除 `pywinpty`（并确保无可用 `node-pty`），验证 `auto` 自动降级为 `wt-tail`**
  ```powershell
  py -3 -m pip uninstall -y pywinpty
  py -3 -c "import importlib.util, pathlib; spec = importlib.util.spec_from_file_location('n', pathlib.Path('node/agenct-node.py')); m = importlib.util.module_from_spec(spec); spec.loader.exec_module(m); print('probe_conpty =', m.probe_conpty()); print('probe_wt_exe =', m.probe_wt_exe()); print('resolved =', m.resolve_exec_backend('auto').name)"
  ```
  - **预期结果**：`probe_conpty = None`，`probe_wt_exe` 为本机 `wt.exe` 绝对路径，`resolved = wt-tail`。
  - **判定标准**：无 ConPTY provider 且 `wt.exe --version` 正常时，`auto` 静默降级至 `wt-tail` 且不抛异常。

- [ ] **2.2 执行 `wt-tail` 任务，验证日志落盘与 Windows Terminal 只读 tail 标签页**
  ```powershell
  # 方式 A：CLI 自检
  py -3 node\agenct-node.py --self-check --agents codex --exec-backend auto
  # 方式 B：免 CLI 实时输出冒烟（持续打印 5 行，便于观察实时滚动）
  py -3 -c "import importlib.util, os, pathlib, threading; spec = importlib.util.spec_from_file_location('n', pathlib.Path('node/agenct-node.py')); m = importlib.util.module_from_spec(spec); spec.loader.exec_module(m); meta = {}; code = 'import time\nfor i in range(1, 6):\n print(f\"tick-{i}\", flush=True)\n time.sleep(0.5)'; rc, out, err, c = m.run_monitored([os.sys.executable, '-c', code], shell=False, cwd=os.getcwd(), env=dict(os.environ), timeout=30, cancel_event=threading.Event(), progress_cb=lambda *a: None, stage_label='wt-smoke', backend=m.resolve_exec_backend('wt-tail'), task_id='win-wt-1', meta_out=meta); print('rc=', rc, 'backend=', meta.get('backend'), 'attach=', meta.get('attach_cmd'))"
  dir "$env:USERPROFILE\.agenct\runs"
  ```
  - **预期结果**：
    1. 返回 `rc= 0 backend= wt-tail`，`attach_cmd` 为白名单命令 `['...wt.exe', '-w', '0', 'new-tab', '--title', 'agenct:win-wt-1', 'pwsh', '-NoExit', '-Command', 'Get-Content -Wait ...']`。
    2. Windows Terminal 当前窗口自动新建标题为 `agenct:win-wt-1` 的标签页，逐行实时滚动显示 `tick-1` 至 `tick-5`（只读查看，不影响子进程管道回收）。
    3. `%USERPROFILE%\.agenct\runs\` 下存在 `agenct-win-wt-1-<6hex>.log` 且内容包含完整 5 行输出。
  - **判定标准**：标签页正常打开并实时滚动、无报错弹窗、日志落盘于 `%USERPROFILE%\.agenct\runs\` 而非 `/tmp`，且任务结束后日志文件保留。

---

## 3. `wt.exe` 同样不可用时再降级到 `pipe`

- [ ] **3.1 在无 `pywinpty`/`node-pty` 且屏蔽 `wt.exe` 的环境下验证 `auto → pipe` 静默降级**
  - 可在 Windows「设置 → 应用 → 高级应用设置 → 应用执行别名」临时关闭 `wt.exe`，或在当前 PowerShell 会话中临时移除 `WindowsApps` 路径测试：
  ```powershell
  $oldPath = $env:PATH
  $env:PATH = ($env:PATH -split ';' | Where-Object { $_ -notmatch 'WindowsApps' }) -join ';'
  try {
    py -3 -c "import importlib.util, pathlib; spec = importlib.util.spec_from_file_location('n', pathlib.Path('node/agenct-node.py')); m = importlib.util.module_from_spec(spec); spec.loader.exec_module(m); print('probe_wt_exe =', m.probe_wt_exe()); print('auto ->', m.resolve_exec_backend('auto').name); print('conpty ->', m.resolve_exec_backend('conpty').name); print('wt-tail ->', m.resolve_exec_backend('wt-tail').name)"
  } finally {
    $env:PATH = $oldPath
  }
  ```
  - **预期结果**：`probe_wt_exe = None`，`auto -> pipe`、`conpty -> pipe`、`wt-tail -> pipe`。
  - **判定标准**：所有探测失败均静默降级至 `pipe`，无未捕获异常或 traceback。

- [ ] **3.2 验证 `pipe` 后端端到端执行正常**
  ```powershell
  py -3 -c "import importlib.util, os, pathlib, threading; spec = importlib.util.spec_from_file_location('n', pathlib.Path('node/agenct-node.py')); m = importlib.util.module_from_spec(spec); spec.loader.exec_module(m); meta = {}; rc, out, err, c = m.run_monitored([os.sys.executable, '-c', 'print(\"PIPE_OK\")'], shell=False, cwd=os.getcwd(), env=dict(os.environ), timeout=30, cancel_event=threading.Event(), progress_cb=lambda *a: None, stage_label='pipe-smoke', backend=m.resolve_exec_backend('pipe'), task_id='win-pipe-1', meta_out=meta); print('rc=', rc, 'backend=', meta.get('backend'), 'out=', out.decode().strip())"
  ```
  - **预期结果**：输出 `rc= 0 backend= pipe out= PIPE_OK`。
  - **判定标准**：不弹窗、不写本地运行日志、标准输出完整回收。

---

## 4. `--exec-backend` 各取值、环境变量与历史 `--tmux` 映射核验

- [ ] **4.1 逐项验证 `--exec-backend` 全部 5 个合法取值（`auto` / `tmux` / `conpty` / `wt-tail` / `pipe`）**
  ```powershell
  foreach ($b in @('auto','tmux','conpty','wt-tail','pipe')) {
    py -3 -c "import importlib.util, pathlib; spec = importlib.util.spec_from_file_location('n', pathlib.Path('node/agenct-node.py')); m = importlib.util.module_from_spec(spec); spec.loader.exec_module(m); ns = m.parse_args(['--self-check', '--exec-backend', '$b']); print('$b -> parsed=', ns.exec_backend, 'resolved=', m.resolve_exec_backend(ns.exec_backend).name)"
  }
  ```
  - **预期结果**：
    - `auto`：按当前环境解析为 `conpty` / `wt-tail` / `pipe`；
    - `tmux`：原生 Windows 上始终静默降级为 `pipe`；
    - `conpty`：有 `pywinpty`/`node-pty` 为 `conpty`，否则降级 `pipe`；
    - `wt-tail`：有可用 `wt.exe` 为 `wt-tail`，否则降级 `pipe`；
    - `pipe`：始终为 `pipe`。
  - **判定标准**：5 个取值均能正常解析且降级结果与矩阵一致。

- [ ] **4.2 验证 `AGENCT_EXEC_BACKEND` 环境变量与 CLI 优先级**
  ```powershell
  $env:AGENCT_EXEC_BACKEND = "wt-tail"
  py -3 -c "import importlib.util, pathlib; spec = importlib.util.spec_from_file_location('n', pathlib.Path('node/agenct-node.py')); m = importlib.util.module_from_spec(spec); spec.loader.exec_module(m); print('env only:', m.parse_args(['--self-check']).exec_backend); print('cli wins:', m.parse_args(['--self-check', '--exec-backend', 'pipe']).exec_backend)"
  $env:AGENCT_EXEC_BACKEND = "invalid-backend"
  py -3 -c "import importlib.util, pathlib; spec = importlib.util.spec_from_file_location('n', pathlib.Path('node/agenct-node.py')); m = importlib.util.module_from_spec(spec); spec.loader.exec_module(m); print('invalid fallback:', m.parse_args(['--self-check']).exec_backend)"
  Remove-Item Env:AGENCT_EXEC_BACKEND -ErrorAction SilentlyContinue
  ```
  - **预期结果**：`env only: wt-tail`、`cli wins: pipe`；非法环境变量打印 `Warning: invalid AGENCT_EXEC_BACKEND` 到 stderr 并回退 `auto`。
  - **判定标准**：环境变量生效、CLI 优先于环境变量、非法值不崩溃且安全回退 `auto`。

- [ ] **4.3 核对历史 `--tmux auto / on / off` 映射与已移除状态**
  > **代码事实**：历史上 `--tmux auto` / `--tmux on` / `--tmux off` 曾分别映射至 `--exec-backend auto` / `tmux` / `pipe`；按「零双轨」规范，该旧参数已在提交 `8191759` 移除，不再作为 CLI 参数保留。
  ```powershell
  foreach ($legacy in @('auto','on','off')) {
    py -3 node\agenct-node.py --self-check --tmux $legacy
    Write-Host "legacy --tmux $legacy exit=$LASTEXITCODE"
  }
  ```
  - **预期结果**：三次调用均返回非零退出码（`2`），stderr 输出 `unrecognized arguments: --tmux ...`。
  - **判定标准**：确认旧参数已彻底下线（无双轨残留），且迁移文档与启动脚本均统一改用 `--exec-backend`（`auto` / `tmux` / `pipe`）。

---

## 5. `AGENCT_NODE_IDLE_TIMEOUT` / `--idle-timeout` 超时终止与让出队列验收

- [ ] **5.1 验证环境变量解析、CLI 覆盖与 `0` 禁用**
  ```powershell
  $env:AGENCT_NODE_IDLE_TIMEOUT = "12"
  py -3 -c "import importlib.util, pathlib; spec = importlib.util.spec_from_file_location('n', pathlib.Path('node/agenct-node.py')); m = importlib.util.module_from_spec(spec); spec.loader.exec_module(m); print('env=', m.parse_args(['--self-check']).idle_timeout, 'cli=', m.parse_args(['--self-check', '--idle-timeout', '4']).idle_timeout, 'disable=', m.parse_args(['--self-check', '--idle-timeout', '0']).idle_timeout)"
  Remove-Item Env:AGENCT_NODE_IDLE_TIMEOUT -ErrorAction SilentlyContinue
  ```
  - **预期结果**：输出 `env= 12 cli= 4 disable= 0`（未设置时默认 `300`）。
  - **判定标准**：CLI `--idle-timeout` 优先级高于 `AGENCT_NODE_IDLE_TIMEOUT`，`0` 合法解析。

- [ ] **5.2 验证静默挂起任务被强制终止并让出队列给下一条任务**
  ```powershell
  py -3 -c "import importlib.util, os, pathlib, threading, time; spec = importlib.util.spec_from_file_location('n', pathlib.Path('node/agenct-node.py')); m = importlib.util.module_from_spec(spec); spec.loader.exec_module(m); t0 = time.monotonic(); st1, out1, _ = m.execute_job('custom:py -3 -c \"import time; time.sleep(30)\"', {'task_id':'idle-hang-1','title':'hang','instruction':'hang'}, os.getcwd(), 'workspace-write', 60, threading.Event(), lambda *a: None, idle_timeout=3, exec_backend='pipe'); dt1 = time.monotonic() - t0; st2, out2, _ = m.execute_job('custom:py -3 -c \"print(\x27NEXT_JOB_OK\x27)\"', {'task_id':'idle-next-2','title':'next','instruction':'next'}, os.getcwd(), 'workspace-write', 60, threading.Event(), lambda *a: None, idle_timeout=3, exec_backend='pipe'); print(f'job1: status={st1} dt={dt1:.1f}s msg={out1.splitlines()[0]}'); print(f'job2: status={st2} msg={out2}')"
  ```
  - **预期结果**：
    1. `job1` 在约 `3~4s`（远小于 30s）即返回 `status=timeout`，首行包含 `[idle-timeout] No output for 3s; terminated hung process to unblock queue. Adjust AGENCT_NODE_IDLE_TIMEOUT or --idle-timeout (0 disables).`（实际提交给 Hub 时 Protocol 2 映射为 `status="failed"`）。
    2. 紧接着 `job2` 立即执行并返回 `status=completed`、`msg=NEXT_JOB_OK`。
  - **判定标准**：挂起子进程被及时终止无残留阻塞、返回明确的 `[idle-timeout]` 提示、后续任务可立即顺畅出队执行。

- [ ] **5.3 （可选）联调真 Hub 验证队列让出与心跳静默告警**
  - 启动节点时传入 `--idle-timeout 70`（或设置 `$env:AGENCT_NODE_IDLE_TIMEOUT="70"`），先后向该节点派发 1 条静默挂起任务 + 1 条简单回显任务。
  - **预期结果**：第 1 条任务在静默满 60s 时网页侧阶段文案出现 `（已静默 60s）`，70s 后转为失败并显示 `[idle-timeout]`，随后第 2 条任务自动开始并完成。
  - **判定标准**：节点无需重启，队列自动恢复畅通。

---

## 6. 日志路径 `%USERPROFILE%\.agenct\runs\` 与 `wt.exe new-tab` 实时滚动专项验收

- [ ] **6.1 验证 `get_runs_dir()` 路径与含空格路径转义**
  ```powershell
  py -3 -c "import importlib.util, pathlib; spec = importlib.util.spec_from_file_location('n', pathlib.Path('node/agenct-node.py')); m = importlib.util.module_from_spec(spec); spec.loader.exec_module(m); rd = m.get_runs_dir(); print('runs_dir =', rd); print('wt_argv_plain =', m.build_wt_tail_argv('wt.exe', 'task-01', rd + '\\agenct-task-01.log')); print('wt_argv_space =', m.build_wt_tail_argv('wt.exe', 'task-02', 'C:\\Users\\Alice Smith\\.agenct\\runs\\a b.log'))"
  ```
  - **预期结果**：
    - `runs_dir` 为 `<当前 USERPROFILE>\.agenct\runs`；
    - 含空格路径的 `-Command` 参数自动以单引号包裹：`Get-Content -Wait 'C:\Users\Alice Smith\.agenct\runs\a b.log'`。
  - **判定标准**：路径指向 `%USERPROFILE%\.agenct\runs` 且含空格/特殊字符用户名不会破坏 PowerShell 命令。

- [ ] **6.2 验证 `wt.exe new-tab` 手动/自动实时滚动与中文 UTF-8 显示**
  ```powershell
  $runs = Join-Path $env:USERPROFILE ".agenct\runs"
  New-Item -ItemType Directory -Force -Path $runs | Out-Null
  $log = Join-Path $runs "manual-wt-check.log"
  "" | Set-Content -Path $log -Encoding UTF8
  wt.exe -w 0 new-tab --title "agenct:manual-check" pwsh -NoExit -Command "Get-Content -Wait '$log'"
  1..5 | ForEach-Object {
    "[$(Get-Date -Format HH:mm:ss)] 中文输出行 $_" | Add-Content -Path $log -Encoding UTF8
    Start-Sleep -Milliseconds 600
  }
  ```
  - **预期结果**：Windows Terminal 打开标题为 `agenct:manual-check` 的新标签页，每 0.6s 实时滚动追加一条中文日志，循环结束后标签页保持打开（`-NoExit`）便于回看。
  - **判定标准**：新标签页标题正确、中文无乱码、内容实时刷新；验收完毕可手动删除测试日志 `Remove-Item "$env:USERPROFILE\.agenct\runs\manual-wt-check.log"`。

---

## 验收记录汇总

| 验收项 | 结果（PASS / FAIL / SKIP） | 实际命中后端 / 现象备注 |
| --- | --- | --- |
| 0. 环境与单测基线 | | |
| 1. ConPTY（`pywinpty`）正常路径 | | |
| 2. ConPTY 缺失降级 `wt-tail` | | |
| 3. `wt.exe` 缺失降级 `pipe` | | |
| 4. `--exec-backend` 全取值 / env / `--tmux` 下线确认 | | |
| 5. `--idle-timeout` 超时终止与队列让出 | | |
| 6. `%USERPROFILE%\.agenct\runs\` 与 `wt.exe new-tab` 滚动 | | |
