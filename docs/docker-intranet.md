# 内网 Docker 部署（GitLab 登录）

适用于企业内部试用：一台内网主机运行容器，同事用浏览器访问 `http://<内网IP>:5176`，以公司 GitLab 账号登录。
GitLab 中处于 active 状态的用户首次登录即自动建号；“账号与成员”里的“邀请同事”可复制入口链接和说明文字。

当前实例：`deploy@10.0.0.11`，目录 `/srv/agenct`（或 `~/agenct`），端口 5176。

## 一次性准备

1. GitLab（`https://git.example.com`）→ 用户设置或管理中心 → Applications，新建应用：
   - Redirect URI：`http://10.0.0.11:5176/identity/v1/auth/oidc/gitlab/callback`
   - Confidential：勾选；Scopes：只选 `read_user`
2. 在主机目录准备：

   ```sh
   mkdir -p ~/agenct/data && chmod 700 ~/agenct/data
   cp agenct.env.example ~/agenct/agenct.env && chmod 600 ~/agenct/agenct.env
   # 填入 GITLAB_OAUTH_CLIENT_ID / SECRET、DEEPSEEK_API_KEY
   ```

3. 构建或导入镜像（在仓库根目录）：`docker build -t agenct:latest .`；
   主机拉取依赖慢时可在工作站构建后 `docker save agenct:latest | ssh deploy@10.0.0.11 docker load`。

## 启动与日常

```sh
cd ~/agenct
docker compose up -d          # 首次启动在 data/ 生成登录配置与数据库
docker compose logs -f        # 不含令牌或聊天正文
./backup.sh                   # 一致备份到 data/backups，保留 14 天
```

- 容器使用 host 网络：Hub 依安全策略只能绑定明确的内网 IP，模型适配器只在 127.0.0.1。
- 会话有效期默认 8 小时（`AGENCT_SESSION_TTL_SECONDS`，300–86400）；登录状态在内存中，容器重启后需重新登录，数据不受影响。
- `AGENCT_MODEL_RUNTIME=mock` 可不调用模型做演示。
- 首次启动生成的 `data/browser-oidc.json`、`data/launch.json` 之后不会被环境变量覆盖；修改须编辑文件后 `docker compose restart`。

## 恢复

```sh
docker compose stop
cp data/backups/hub-<时间>.sqlite3 data/hub.sqlite3 && rm -f data/hub.sqlite3-wal data/hub.sqlite3-shm
docker compose start
```

## 边界

内网 HTTP 明文传输登录 Cookie，仅适用于可信内网；数据库未加密，主机管理员和 docker 组成员可读取全部数据，备份同等敏感。
自动注册模式不支持管理员和多组织隔离：所有 GitLab active 用户处于同一个工作区。需要限制成员时改用固定成员模式（见 [gitlab-login.md](gitlab-login.md)）。

## 完整快照（短暂停机）

默认 `backup.sh` 仍是在线数据库备份。执行 `./backup.sh --full` 会停止原本正在运行的服务，通过一次性容器创建 `/data/backups/state-*.zip`，结束或失败后恢复原服务；原本已停止的服务不会被自动启动。此模式包含数据库、附件及应用配置，适合维护窗口。

也可先手动停止服务，再使用 `docker compose run --rm --no-deps agenct backup-state /data/backups/state.zip`。恢复应使用新卷目录，并在运行 `scripts/state_backup.py restore` 时通过 `--runtime-state-dir /data` 保留容器最终挂载路径。确认恢复成功、外部 `agenct.env` 等依赖齐备后，再切换 Compose 的数据卷；不要覆盖运行中的数据目录。归档含凭据与私有对话，需按原数据保护。

恢复后的定时任务和 GitLab 自动轮询暂停，未完成节点派发不会自动重跑。完整操作与恢复边界见 [技术参考](reference.md#完整应用状态备份与恢复)。
