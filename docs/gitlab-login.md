# GitLab 登录与成员管理

## 当前本地启动

Agenct 当前监听 `127.0.0.1:5176`，浏览器访问 **`http://localhost:5176`**。之前仅绑定 WSL 的 `100.64.0.12` 导致浏览器无法加载，已停止使用该入口。

登录页直接跳转 GitLab，Agenct 请求的回调地址为：

`http://localhost:5176/identity/v1/auth/oidc/gitlab/callback`

GitLab OAuth Application 必须允许这个完整地址；不能把已登记的 IP 回调视为等同于 localhost。当前已验证本地页面加载和跳转 GitLab 登录页，尚未验证真实用户授权后的返回，也未修改 GitLab 应用登记。

现有私有配置目录为 `/home/lee/.local/share/agenct/gitlab-login`。`launch.json` 使用 `AGENCT_BIND=127.0.0.1:5176`，移除 `AGENCT_PRIVATE_BIND_IP`；`browser-oidc.json` 使用 `public_origin=http://localhost:5176`、`allow_loopback_http=true`、`allow_private_http=false` 和上述回调路径。保留原数据库、模型和代理配置。重启：

```sh
python3 scripts/start-gitlab-login.py \
  --state-dir /home/lee/.local/share/agenct/gitlab-login \
  --model-state-dir <已有的模型状态目录>
```

新环境可用配置生成工具。`--reuse-registered-callback` 会沿用源配置中的完整 origin；若源配置仍是 WSL IP，不要直接用于当前 localhost 启动，应先提供已登记的 localhost 回调配置。5176 由 Agenct 独占，棋局 Web 不能同时监听这个端口。

## 网络：服务器到 GitLab 需经代理时

`git.qivators.com` 从本机直连不通（超时），只能经工作站代理访问。Hub 访问身份提供方（换取令牌、读取 `/api/v4/user`）默认不使用环境代理，因此需要显式指定：

- `AGENCT_IDENTITY_PROXY=http://127.0.0.1:10809`：只用于身份提供方请求；HTTPS 经 CONNECT 隧道端到端加密，代理看不到授权码或令牌。模型 adapter 等其他请求不受影响。只接受 http(s) 代理地址，写错会拒绝启动。
- `scripts/start-gitlab-login.py` 在环境中存在 `HTTPS_PROXY` 且未显式配置时自动设置此项，并在日志中打印一行说明。

验证方法：发起一次登录，拿到 state 和登录 Cookie 后，用一个无效授权码访问回调。几秒内看到日志 `{"event":"login_callback_failed","stage":"provider"}`，说明 Hub 已连到 GitLab；如果约 15 秒后才失败，说明仍连不上。

## 登录失败的提示与排查

回调失败不再返回空白 401，而是回到首页 `/?login_error=原因`，登录页显示对应说明；服务日志记录失败阶段（不含授权码、令牌或用户信息）：

| 日志 `stage` | 页面提示 | 常见原因 |
| --- | --- | --- |
| `expired` | 登录已过期 | 超过 5 分钟；或登录过程中服务重启（进行中的登录只保存在内存）；或在另一个浏览器发起 |
| `provider` | 连不上 GitLab | 服务器到 GitLab 的网络或代理；授权码无效或已使用 |
| `userinfo` | 连不上 GitLab | 读取 GitLab 用户信息失败 |
| `account` | 账号不可用 | GitLab 账号非 active，或固定成员模式下未绑定该数字 ID |

## 本地认证边界

私有网络 HTTP 需要显式 `allow_private_http`，同时 `AGENCT_PRIVATE_BIND_IP` 必须与监听 IP 相同，并启用浏览器认证。只允许 RFC1918、CGNAT 或回环范围，不开放任意公网 HTTP 地址。正式部署仍应使用 HTTPS。

GitLab token 与 userinfo 请求始终验证 HTTPS。授权码交换使用 confidential client 的 Basic 认证、PKCE S256、浏览器绑定 state 和登记回调。Cookie 使用 HttpOnly、SameSite=Lax；HTTP 本地 Cookie 与 HTTPS Cookie 名称隔离。API 保留 Host、Origin、CSRF 校验。提供商 access token 不进入浏览器 URL 或浏览器存储。

GitLab 数字 ID 是用户身份；不按邮箱、姓名或用户名匹配历史账号。`gitlab_auto_signup: true`、空 `subjects` 表示 active GitLab 用户首次登录自动建立 `gitlab-ID` 成员；不存在首位用户自动升管理员。独立数据库不继承旧 Alice/Bob 数据。已有停用状态不会被再次登录覆盖。管理员须由操作者显式配置 `AGENCT_MEMBER_ADMINS_JSON`。

## 其他接入方式

默认独立 OAuth 模式支持自己的 HTTPS 回调，需要在 GitLab 登记。可选的 `--shared-login-url` 模式则依赖 Tidalis Identity 的 `code_handoff` 扩展及 origin 白名单；当前本地运行不使用该模式。之前的共享回调 relay 已停止，不应再启动旧的 `Caddyfile.identity-callback`。

## 成员生命周期

左侧「账号与成员」可修改本人显示姓名和头像颜色；管理员可查看同组织成员、修改资料、停用/启用普通账号。管理员角色由启动配置决定；页面不能提升管理员或停用管理员/自己。

自动建号模式下成员首次 GitLab 登录即可加入。以下预配置与邀请流程适用于固定成员模式：

1. 操作者先在 `browser-oidc.json` 中绑定新 GitLab 数字 ID，在 `launch.json` 的 `AGENCT_USERS_JSON` 和 `tenants.json` 中增加对应本地用户，重启服务。新身份必须显式预配置，不开放匿名注册。现有组织归属不可更改。
2. 管理员可直接启用；或先停用该账号，再「生成邀请链接」，由用户使用链接登录并接受。邀请链接只激活其绑定的 GitLab 身份，24 小时有效、一次性使用，服务端只保存链接 token 的 SHA-256。重发会替换旧链接，停用/启用会撤销未使用邀请。应用不会自动发邮件或消息。

联系人显示同组织的启用成员及资料；停用成员不能继续使用已有 Cookie 会话、读取私聊或创建新委派，已有任务与交付历史保留。停用不会取消已经发出的外部模型请求。账号重新启用后旧 Cookie 不会复活，需重新登录。

登录会话目前在服务端内存中，最长 15 分钟且不超过提供商令牌有效期；刷新页面沿用 Cookie。退出或会话到期需重新登录，服务重启也会结束登录。不实现提供商全局退出、refresh token 或跨实例共享会话。

## API 与验收

- `GET /api/me`：包含本人 `member` 资料；`GET /api/people`：同组织启用联系人资料。
- `GET /api/members`：管理员的组织成员目录。
- `PUT /api/members/{id}`：带 `expected_revision` 修改资料或状态，冲突返回 409。
- `POST /api/invitations`：管理员生成指定成员邀请。
- `POST /api/invitations/accept`：已验证身份、Cookie + CSRF，激活对应邀请。

`cargo test --manifest-path hub/Cargo.toml --lib` 覆盖离线 TLS 提供商、GitLab 风格 Basic + PKCE、数字 ID 绑定、邀请错人/重复使用、停用撤销、数据库升级。`cargo test --manifest-path hub/Cargo.toml --test api` 覆盖管理员权限与停用访问。`python3 scripts/test_gitlab_login.py` 覆盖配置生成。`npm test`、`npm run test:browser` 覆盖登录恢复、账号设置和联系人。

当前已验证直接授权跳转、登记回调路由和本地登录测试。真实用户授权完成仍需在浏览器验证。
