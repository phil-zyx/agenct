# 技术与运行参考

本文与[入门](getting-started.md)是当前维护入口。`archive/` 保存原专题合同、决策和历史验收记录，避免把历史阶段的限制误当作现状。当前开发项与外部验收见[计划](next-steps-plan.md)，界面用词见[用语表](ui-terminology.md)。

## 架构与边界

| 部分 | 职责 | 不授予的能力 |
| --- | --- | --- |
| `dist/` | 单浏览器静态演示、固定脚本状态机 | 真实身份、多人同步、模型和安全隔离 |
| `hub/src/` | Axum API、身份、SQLite、私聊、提案、任务、草稿、回执、角色 | 自动执行代码、模型替人确认、自动发布 |
| `hub/web/` | 聊天、任务、索引、角色与草稿界面 | 客户端自报身份或跳过服务端版本校验 |
| `adapters/dsh/` | 本机 DSH SDK 协议、固定角色提示、无工具模型请求 | 接收浏览器密钥、扩大共享范围、创建任务 |
| `scripts/`、`deploy/` | 启动、身份管理、备份恢复、PDF 提取、试用统计、服务模板 | 已部署的共享主机或公网生产保障 |
| `sandbox/`、Worker/发布库 | 沙箱边界、入队、证据与发布合同 | 未经真实门槛验证的执行和发布 |

Hub 只监听 loopback。试用由 SSH 隧道连接，不公开开发令牌。SQLite 和同一 OS 账号下的文件不抵御主机管理员；数据未做静态加密。每个用户的私聊和提案隔离；任务只有参与双方可见；草稿仅归接收人。确认共享的摘要、附带材料、人工交付和任务状态才进入双方视图。

模型可以生成文字和私有提案，不能确认提案、交付、验收、执行代码或发布。显式任务动作均校验身份、状态和 `expected_revision`。历史设计依据见[架构](archive/architecture.md)、[策略层](archive/policy-layer.md)、[运行时边界](archive/runtime-boundary.md)。

## 身份与配置

私有试用使用独立高熵 bearer token。服务配置只保存哈希；令牌文件单独保存在私有状态目录，不能放在 URL 或日志中。使用新版 launcher 启动时，每次 bearer 鉴权重新读取受控 `config.json`，因此 `add-user` 与 `rotate-token` 在线生效。配置不可读或格式错误时拒绝鉴权，不沿用旧身份缓存。

可选 OIDC bearer 验证与浏览器 OIDC 会话已有实现和本地测试基础，但真实身份提供方、TLS、租户策略、配额、运维和生产安全审查仍是部署门槛。`AGENCT_OIDC_CONFIG_FILE` 与 `AGENCT_BROWSER_OIDC_CONFIG_FILE` 是两种不同模式，不能同时启用；浏览器模式禁用开发/API bearer 登录，使用绑定会话与 CSRF 防护。它们不能与私有开发令牌热加载模式混用。

常用启动配置：

| 配置 | 含义 |
| --- | --- |
| `AGENCT_BIND` | loopback 地址与端口；launcher 由状态配置生成 |
| `AGENCT_DATABASE` | SQLite 路径；launcher 固定在状态目录 |
| `AGENCT_RUNTIME` | `mock` 或 `dsh`，显式选择，不自动回退 |
| `AGENCT_ADAPTER_URL` / `AGENCT_ADAPTER_TOKEN` | 手动管理 adapter 时的本机地址与独立认证 |
| `DEEPSEEK_API_KEY` | 仅由 adapter 使用的提供商凭据 |
| `AGENCT_DSH_MODEL` | adapter 的服务器模型设置；个人角色只引用 `server-default` |
| `AGENCT_TENANTS_FILE` | 可选完整用户→组织映射；须与OIDC结合且禁用热成员变更 |
| `AGENCT_WORKER_SCOPES_FILE` | 按任务限制入队权限；不能启用执行 |

### 可选组织隔离

先配置上述一种 OIDC 模式，再设置 `AGENCT_TENANTS_FILE` 为受控 JSON 文件，格式是完整的全局用户 ID 到组织 ID 的映射，例如：

```json
{"alice":"north","bob":"north","carol":"south","dave":"south"}
```

所有配置用户必须恰好出现一次，重复 JSON key、遗漏/额外用户和无效组织名都会拒绝启动。组织名限 1–64 位 ASCII 字母、数字、连字符、下划线。`Hub::with_tenants` 提供同样的库配置入口；此模式不能与 `AGENCT_PRIVATE_CONFIG` 在线成员修改混用，修改组织配置需要停止并重新启动服务。

数据库持久记录用户→组织绑定，不能通过重启后更换映射把原身份改分给另一组织。已启用组织绑定的数据库若未加载组织配置，API 拒绝服务（503），不会因漏配环境变量回退到无组织隔离。需要组织迁移时必须另行制定明确的数据迁移方案，当前不提供在线重分配。

组织模式仅允许同组织用户互相委派；接收人建议也只列同组织，`GET /api/me` 仅返回本人 `tenant`。原有每用户私聊/提案/草稿和任务参与人权限继续有效：同组织不代表可以看同事的私聊或草稿。启动扫描已有任务和提案，发现跨组织或未映射用户的历史记录时拒绝启动，不自动迁移或删除。身份仍是全局唯一用户 ID，不支持各组织重用同一用户名；不要将原用户 ID 重新分配给另一个人。

本地测试使用真实签名 OIDC 测试令牌覆盖跨组织提案、确认、任务、草稿和历史隔离，以及旧数据启动拒绝；这不等于真实身份提供方/TLS 已上线，也不替代生产租户审计、配额、密钥和备份隔离。

### 本地 TLS 与 OIDC 服务模板

[本地 Caddy 模板](../deploy/Caddyfile.local.example) 只监听 `127.0.0.1:8443`，使用 `tls internal`，关闭全局证书信任安装，并转发到 loopback Hub。[OIDC 服务模板](../deploy/agenct-oidc.service.example) 直接启动 Hub，读取受控的 OIDC 与组织配置，不通过开发令牌热加载 launcher。两者都需要操作者填写绝对路径和身份提供方配置；仓库没有安装服务或创建公开入口。

浏览器 OIDC 的 `public_origin` 必须与外部 HTTPS 源完全一致。模板保留客户端 Host，让 Hub 校验预设源；Caddy 对 HTTP upstream 默认透传包括 Host 在内的请求头，详见 [Caddy reverse_proxy 官方文档](https://caddyserver.com/docs/caddyfile/directives/reverse_proxy)。不要把 Hub 的 OIDC HTTPS 客户端改成跳过证书校验。

```sh
caddy validate --config deploy/Caddyfile.local.example --adapter caddyfile
cargo build --locked --manifest-path hub/Cargo.toml
python3 scripts/smoke-local-tls.py --caddy /path/to/caddy
```

本次本机 Caddy 2.11.9 → Hub 烟雾检查通过：测试客户端显式信任临时 CA 后，证书主机名校验、`/health` 200、无令牌 `/api/me` 401、有令牌 200 均符合预期；默认信任库拒绝该测试证书，监听仅在 loopback。此烟雾使用开发 bearer 验证入口认证，OIDC/组织另由签名令牌及本地提供方 fixture 测试覆盖。它不注册真实 IdP、不安装系统 CA、不使用公网域名，也不等于生产 TLS/OIDC 已部署。

精确配置格式见 [OIDC bearer](archive/oidc-auth.md)、[浏览器 OIDC](archive/browser-oidc.md)、[私有运行](archive/private-staging.md)和[用户目录](archive/user-directory.md)。

## API 合同

下面是开发 bearer 模式的主要接口。浏览器 OIDC 模式使用对应会话与 CSRF 规则。未授权资源返回通用拒绝，不泄漏他人的任务或草稿。启用组织映射后，委派还要求双方属于同一组织。未知字段拒绝；模型响应在 adapter 和 Hub 两端校验。

| 接口 | 内容与写入边界 |
| --- | --- |
| `GET /api/me` | 当前服务端身份与模式 |
| `GET /api/messages` | 本人近期消息；模型上下文压缩不删除历史记录 |
| `POST /api/messages` | `{message, request_id}`；可选角色与明确选择的任务上下文 |
| `GET /api/turns/{request_id}` | 只读查询聊天回执，不重发 |
| `GET/POST /api/proposals` | 本人提案；创建接受 `{owner, summary, request_id?}` |
| `POST /api/proposals/{id}/actions` | `confirm` 或 `reject`，要求 `expected_revision`；确认可带手写 `owner`、`summary` 和 `details` |
| `GET /api/tasks`、`GET /api/tasks/{id}` | 参与任务及共享历史；不返回对方私密草稿 |
| `POST /api/tasks/{id}/actions` | `accept`、`deliver`、`approve`、`return`、`cancel`；带 `expected_revision`，交付带 `result`、取消带 `reason` |
| `POST /api/materials/extract` | 本机 PDF 提取，仅返回本人预览文本，不自动共享 |
| `GET /api/activity` | 按当前身份投影的提案与任务索引；草稿内容不共享 |
| `GET/PUT /api/profile`、`GET /api/profile/catalog` | 本人角色设置与固定目录；修改要求配置版本 |
| `POST /api/tasks/{id}/drafts` | 任务接收人的私密起草或改稿，详见下节 |
| `GET /api/tasks/{id}/drafts`、`GET /api/tasks/{id}/drafts/turns/{request_id}` | 私密草稿历史分页与恢复 |

直接 `POST /api/tasks` 返回 405。自然语言模型提案的摘要必须来自当前用户消息；手动修改摘要由请求者在确认界面明确提交。确认提案、创建任务和事件在同一事务内完成；不同编辑内容不能用同一确认重放覆盖已有任务。

所有共享摘要最多 4000 UTF-8 字节，材料最多 256 KiB。提案创建和确认、聊天和草稿各有独立的幂等边界；不要把它们当作同一编号空间。任务状态变更递增版本，过期动作返回冲突并且不写事件。

### 聊天恢复与明确上下文

`request_id` 为当前用户内唯一的 1–128 位 ASCII 字母、数字、连字符或下划线。相同编号和输入读取原回执；不同消息、任务选择或角色绑定返回冲突。已登记请求不会再次调用模型；无编号的旧客户端调用没有这一去重保证。

回执为 `pending`、`complete` 或 `uncertain`。持久化登记早于任何模型/摘要调用；完成时回复、提案和回执一起提交。进程重启把未完成请求标为未知，不重新调度。网页刷新后自动只读核对，只有用户主动的新编号才允许新尝试；这仍可能重复外部费用，不等于提供商 exactly-once。

可选 `selected_task: {task_id, expected_revision}` 只附带用户明确选择的一项参与任务，绑定当时版本。普通聊天默认不会自动读取任务。角色选择用 `agent_id` 与 `expected_profile_revision`；过期或禁用角色不会静默替换。详见[回执合同](archive/chat-turn-recovery.md)、[任务讨论](archive/task-discussion.md)、[角色选择](archive/role-selection.md)。

### 私密草稿与多轮修改

接收人先接手，使任务处于 `working`。首次起草请求：

```json
{"request_id":"draft-1","expected_revision":1}
```

修改时绑定本人同任务、同任务版本的已保存草稿：

```json
{
  "request_id":"draft-2",
  "expected_revision":1,
  "revision":{"base_draft_id":"previous-draft-id","instruction":"更简短，保留证据"}
}
```

`instruction` 为 1–4000 UTF-8 字节。服务端按草稿 ID 取原文，不接受客户端提供任意他人草稿；跨用户、跨任务或过期任务版本均不能进入模型输入。可选角色字段与普通起草相同。请求绑定包括修改条件，修改要求变了不能重用原请求编号。

每次成功新增不可变草稿，响应包含 `draft.id`、`seq`、`base_draft_id`、`instruction`、文本和任务版本。旧草稿不覆盖；分页沿用 request ID 游标，`seq` 才是草稿生成顺序。列表返回 `recommended_profile`：从本人已启用角色中，按审阅/研究关键词优先选择，否则推荐写作；没有对应角色时用管家。界面展示推荐并允许覆盖。

模型只收到任务共享数据、所选角色和明确的修改基稿/要求，普通私聊不会进入起草。草稿结果限 8000 UTF-8 字节，不能携带委派提案。`current_stale` 在每次读取时重新判断；过期内容只能查看。模型过程中任务变更不会把私人草稿强行交付。

`source_task_context` 记录原始来源快照，`task_context` 记录实际送入草稿模型的上下文；大材料可能先变成标注过的摘要。登记回执先于摘要调用，摘要失败同样留下未知结果，不会重放同一编号再次计费。人工交付是另一次显式 `deliver` 操作，会追加共享交付记录。使用所选草稿预填时，客户端可随交付带上 `source_draft_id`；服务端校验来源属于本人、同任务和当前任务版本，以记录经过人工编辑的采用关系。该来源不自动交付；草稿 ID、修改要求和私人历史不进入双方公共视图。详见[历史草稿合同](archive/recipient-draft-api.md)和[交付账本](archive/database-schema.md)。

## 文件材料

文本和 Markdown 在浏览器读取后预览。PDF 使用服务端本地 `scripts/extract-pdf.py`，需要 Python 3 和 `poppler-utils` 的 `pdftotext`。不调用模型提取、不持久保存上传文件；提取文字返回给本人预览，只有确认提案时才进入共享材料。

限制为 PDF 输入 2 MiB、提取文本 256 KiB、墙钟 8 秒、CPU 5 秒、地址空间 512 MiB，以及输出文件大小上限。扫描图片 PDF 没有可用文字时拒绝，需先自行 OCR。提取失败不会把原 PDF 转给模型，也不会绕过确认共享。资源限制不是完整的不可信文档沙箱验收，不能据此启用代码执行。

## 模型与上下文

角色是固定提示词：研究强调证据、对比、未知项与置信度；写作直接给成稿并保持事实、受众和语气；审阅按严重程度列具体问题、影响和修改建议；管家负责澄清目标。角色没有独立工具权限，个人配置不能提供端点或密钥。目前模型目录只有服务器控制的 `server-default`。

长聊天超过 40 条或预算约束时，Hub 将较早完成的私聊分块摘要，保留最近上下文；摘要缓存按用户隔离，原始消息不删除。输入块最多 40000 字节，累计摘要最多 8000 UTF-8 字节，最终传输受 60 KB 总预算限制。摘要继承原角色和无工具约束；不能生成委派、改任务或读另一人的历史。transport 超限明确报错，不再静默删掉最旧记录。mock 使用确定性标记测试该路径，不代表真实模型摘要质量。

大于原始任务快照额度的共享材料会完整分块进入摘要，不再只把最前面 12 KB 给模型。草稿/讨论模型收到带派生摘要标记的材料；原文保留供人工核对。摘要可能遗漏细节，完整输入经过摘要不等于主模型逐字读到完整原文。每个摘要块都可能增加调用次数、等待和费用；相关界面、失败和恢复需要真实模型试用核对。

更详细的 SDK、超时与独立进程边界见 [DSH 接入记录](archive/dsh-adapter.md)、[配置审计](archive/dsh-runtime-config-audit.md)。历史文件中的“尚未真实模型验收”代表其记录时点；本轮离线测试不新增真实提供商证据。

## 受控只读仓库上下文

该功能独立于 Worker。操作者先在目标环境执行固定 Docker 边界 probe，生成新的私有 gate 文件：

```sh
python3 sandbox/docker_smoke/run.py --report-file /private/new-gate.json
```

gate 文件权限为 0600、拒绝覆盖，绑定当前 probe 哈希并限制为七天内证据。它是本地主机操作者可写的运行记录，不是独立安全审计，也不能启用任意执行器。

`AGENCT_REPOSITORY_GATE_FILE` 指向该文件，`AGENCT_REPOSITORIES_FILE` 指向受控的 0600 JSON 配置数组：

```json
[{"id":"project","label":"Project","owners":["alice"],"objects_dir":"/abs/repo/.git/objects"}]
```

只允许列明账号读取仓库。`GET /api/repositories` 列出本人可用项目，`POST /api/repositories/context` 获取获准的只读上下文；聊天 `/repo project <完整提交SHA> file` 或 `/repo-search project <完整提交SHA> literal` 后换行写问题，明确把结果附给下一轮聊天并绑定回执。操作者必须显式选择授权目录；配置不会自动开放用户电脑上的任意仓库。

读取使用完整提交哈希校验 commit/tree/blob，仅支持 loose Git objects，不运行 git、hooks 或仓库代码，不解析 packed objects 或可变 refs。拒绝路径穿越、symlink、submodule、特殊文件及二进制；文件最多 256 KiB，输出最多 4 KB/40 行并标记截断，搜索为字面文本。helper 限两并发、256 MiB 内存、CPU 3 秒及墙钟 5 秒。API 仍做身份和白名单校验；文件内容是待分析的数据，不会成为授权指令。更多执行边界保留在[沙箱集成记录](archive/executor-integration.md)。

## 存储与迁移

SQLite 由 `Hub::open` 迁移，当前 `PRAGMA user_version = 13`。第 12 版增加本人私聊摘要缓存 `context_summaries`，第 13 版增加不可变 `tenant_memberships` 用户→组织绑定；此前迁移保留角色、聊天回执、版本化任务、只追加交付、事件、私密草稿回执、不可变共享材料及消息/提案时间。所有待执行迁移和版本标记在同一事务内；不支持自动降级，未来版本或异常结构拒绝打开。

文件数据库持有独占进程锁；WAL 中的数据同样属于数据库。消息、提案、交付、任务及草稿在重启后保留，私密草稿与公开交付序号分别计数，避免从公开序号推断私人活动。事件及交付历史只追加，但主机管理员仍可直接改库；不宣称防篡改。旧记录缺失的时间或版本不伪造补全。

结构验证、旧版本兼容和审计回放细节见[迁移记录](archive/database-schema.md)、[任务审计](archive/task-audit.md)、[时间线](archive/task-timeline.md)。

## 运行与数据保护

状态目录应由运行账号独占，目录 0700、配置与数据库 0600。新版 launcher 校验路径、权限和配置，拒绝已有目录上的隐式重初始化；`start` 构建后启动，`run` 使用已有 release。系统服务用专用非 root 账号；模板设置失败重启、SIGINT 停止和 journal 日志，DSH 管理需另配。

`add-user`/`rotate-token` 在 `admin.lock` 下串行修改。凭据先写、配置再原子激活；进程已经通过鉴权的请求可能完成。若机器在两文件写入之间崩溃，可能出现凭据领先配置：已有用户重新换令牌可修复；加人中断须停服务并恢复一致配置/凭据对后重试。不要同时手动编辑文件，也不要把旧用户 ID 分配给另一个人。

本次本地 systemd 用户 transient 服务验证了主进程被 SIGKILL 后按 `Restart=on-failure` 自动恢复、更换 PID 且聊天仍在；另在隔离临时 root 用 `systemctl enable` 验证生成 `multi-user.target.wants` 链接。运行命令是 `python3 scripts/smoke-local-service.py`。测试没有安装宿主系统服务、没有重启主机，结束后清理临时服务，因此不能替代共享主机实际开机自启验收。

`backup` 使用 SQLite 在线备份 API，包含已提交 WAL，目标必须是新文件，不覆盖已有备份。`restore` 持有 Hub 独占锁、拒绝运行中的服务，验证完整性和必需表，删除陈旧 WAL/SHM 后替换数据库；源数据库被删掉也可恢复。数据库备份不包含身份配置和提供商密钥，需单独保护这些文件，并使用兼容的应用版本恢复。

HTTP 完成事件记录时间、路由模板、方法、状态码和耗时；不记录正文、令牌、请求头、查询串或实际任务 ID。启动消息仍可能是纯文本，adapter 日志另有边界。systemd 下用 `journalctl -u agenct-private` 查看运行日志。

```sh
python3 scripts/private-staging.py check --state-dir "$HOME/agenct-private"
python3 scripts/trial-report.py --state-dir "$HOME/agenct-private" --date 2026-11-06
```

试用报告以只读一致快照输出 UTC 日聚合：委派数、当日创建提案的当前确认/拒绝率、草稿被精确采用或显式关联的比例下界、当天验收时长、失败/未知请求数。漏提醒与模型超时分别由 `--missed-notifications N`、`--model-timeouts N` 补入人工观测；未知不是零。没有明示关联的手改草稿不能推断采用。完整恢复步骤见[入门](getting-started.md#6-管理参与者备份和试用记录)。

## 开发验证

```sh
npm ci
npm test
npm run check
cargo fmt --manifest-path hub/Cargo.toml -- --check
cargo test --locked --manifest-path hub/Cargo.toml
cargo clippy --locked --manifest-path hub/Cargo.toml --all-targets -- -D warnings
python3 -m unittest discover -s scripts -p 'test_*.py' -v
python3 -m unittest discover -s adapters/dsh -p 'test*.py' -v
python3 -m unittest discover -s sandbox -p 'test*.py' -v
npx playwright install chromium
npm run test:browser
npm run test:visual
cargo build --locked --manifest-path hub/Cargo.toml
npm run test:local
```

Chromium 使用真实渲染器与本机 fixture API，覆盖聊天发送/恢复、提案、起草/交付、身份隔离与长 Markdown 防溢出。视觉覆盖桌面 1330/1440、手机 400 和浅/深色；聊天、任务共 12 张基准。CI 固定 Ubuntu 22.04、Playwright 和 DejaVu/Droid 字体，保存实际截图、报告及失败 trace 14 天。审阅有意的界面修改后才运行 `npm run test:visual:update` 更新基准。

`npm run test:local` 启动临时真实 Hub 和两个独立 Chromium 身份，实际执行材料导入、委派、草稿修改、人工交付、验收、在线加人/换令牌、备份删除恢复流程；模型仍为 mock，临时状态自动清理。

安装 Orca、Xvfb、xauth 后，可运行 `python3 scripts/check-screen-reader.py --full`（需先构建 debug Hub，安装 poppler-utils）。该检查使用隔离的 Xvfb、D-Bus、AT-SPI 与 headed Chromium，检查 Orca 生成的朗读日志。当前本机 Orca 42 路径已覆盖登录进入、三步引导、消息输入、发送、工作入口，以及真实本地 Hub 的接手、修改草稿、交付和验收；日志保存在 `test-results/screen-reader/orca.log`。生成朗读日志证明读屏路径可达，不等于人工听觉可用性或每种桌面环境的验收。

模拟 API、单元测试、截图和本机恢复演练分别证明对应层面的行为；不证明真实模型质量、双人远端使用、实体手机键盘、后台通知到达或读屏软件体验。不要把历史测试数量当作持续有效的验收证据。

### 本机并发观测（2026-11-06）

在本次工作环境用实际 debug Hub、临时 SQLite WAL 数据库与 mock 模式，20 个独立用户各顺序发起 50 次聊天写入和 50 次消息读取；并发跨用户，不人为触发同用户重入保护。共 2000 次 HTTP 请求，全部返回 200，耗时 27.674 秒，吞吐 72.27 请求/秒。

| 操作 | 次数 | 中位数 ms | P95 ms | 最大 ms |
| --- | ---: | ---: | ---: | ---: |
| 读取消息 | 1000 | 153.63 | 339.64 | 533.37 |
| 聊天写入 | 1000 | 370.96 | 654.80 | 1051.22 |

该短时本机观测没有真实模型、生产网络、长期历史或发布构建，也没有定义生产延迟目标；它说明此负载下未出现错误或锁超时，不能证明生产容量上限或 PostgreSQL 迁移必要性。仍需按实际试用瓶颈决定是否迁移。

## 后续能力的验收门槛

1. 共享主机试用：选定主机和参与者，真实双电脑登录与交付，系统服务开机/崩溃恢复，恢复数据库后重走流程；记录至少一周的使用数据。
2. 任意代码执行：本地固定沙箱 probe 与受限只读仓库上下文的验收不授权 Worker 执行。仍需独立验证不可信测试无法修改验证器，并验证网络/文件/资源限制、取消、清理与证据绑定。内置执行器继续关闭，不能用宿主 shell 替代验收。
3. 真实远端发布：另行授权分支上传、绑定精确 SHA 的确认、草稿 MR、真实 CI 与未知结果恢复；不自动合并。
4. 生产部署：真实 OIDC/会话与 TLS 联调、租户隔离、速率限制、密钥管理、配额、日志保留与安全审查。
5. PostgreSQL/连接池：以实际并发瓶颈和迁移演练为依据，而不是仅因为有迁移文档。

这些门槛没有因本轮功能扩展自动通过。密钥按用户本轮明确决定不执行撤销，也不列为待操作；仓库修改没有更改提供商凭据。历史验收和约束保留在[路线图](archive/roadmap.md)、[本地验收](archive/roadmap-local-verification.md)、[独立验证器](archive/independent-verifier.md)、[沙箱集成](archive/executor-integration.md)、[分阶段监督器](archive/staged-supervisor.md)、[Worker 合同](archive/worker-contract.md)、[GitLab 传输](archive/gitlab-transport.md)与[PostgreSQL 方案](archive/postgres-migration.md)。
