> 历史专题记录：保留设计背景、协议细节和当时的验收结果；其中“当前/尚未”描述可能早于最新实现。现行用法以[入门](../getting-started.md)和[参考](../reference.md)为准。

# Rust Hub technical proof of concept

This is a **loopback-only development server**, not an Internet-ready account system. It does not replace or change the hosted browser demo. It supplies real server-side storage and authorization; mock model output is explicitly labelled. SQLite is used to make the single-process experiment runnable without a PostgreSQL service. PostgreSQL, OIDC, production deployment hardening and trustworthy code execution remain future work. Owner-confirmed queue admission is available through opt-in per-task HTTP scopes; no consumer is running and the built-in executor remains disabled. Queue-only receipts cannot be claimed for execution. See [architecture.md](architecture.md).

## Run

Requirements: Rust stable (tested 1.99.1), Python 3 for generating local tokens. From repository root:

```sh
python3 scripts/dev-users.py
# Copy the printed export commands into this shell.
# Keep the two generated tokens private; do not commit output.
cargo run --manifest-path hub/Cargo.toml
# Open http://127.0.0.1:8080
```

Use Alice's token in one browser and Bob's token in another private browser window. Tokens are high-entropy development credentials, not user passwords. The Hub configuration contains only SHA-256 hashes. The UI keeps tokens only in memory and removes rendered conversation data on logout. Neither endpoint nor UI supports arbitrary claimed user IDs. Do not expose the listener through a public proxy: no TLS termination, production identity provider, rate limits or security audit are supplied.

`AGENCT_RUNTIME` must explicitly be `mock` or `dsh`. There is no automatic fallback. Mock replies never claim a real model was called. For the actual DSH adapter, see [dsh-adapter.md](dsh-adapter.md). Real mode requires separately authorized model credentials and an allowlisted provider; no real model was called during the credential-free tests.

The UI supports ordinary chat and `/delegate bob <summary>` (or `/delegate alice <summary>`). `/delegate` now creates a **private proposal**, not a task. In mock mode, the bounded natural-language fixtures `请让 bob 帮我<summary>` (optional whitespace after `帮我`) and `ask bob to <summary>` also create a private proposal. These are deterministic test-double patterns, **not general language understanding or live DSH model tool use**. Review the exact recipient and summary, then click confirm to share and create the task. Rejecting shares nothing. The proposal does not copy prior conversation into the shared summary. All configured participants are allowed to send one-hop tasks to each other. This limited policy is intended only for a consenting two-person trial. Adding a configured user gives them a token identity and the ability to receive/send such tasks; do not treat it as a production multi-tenant policy.

Recipients accept and submit **human-authored** results. Requesters accept/return results. This is a real task lifecycle but not independent artifact/CI verification. Tasks do not read private chat history. Model replies cannot create tasks, approve results, write a repository or execute commands through the Hub.

## API

All `/api/*` routes require `Authorization: Bearer <token>`; no query-string credentials or CORS are supported.

- `GET /api/me`: current server-resolved identity and runtime.
- `GET /api/messages`: most recent 40 personal messages, including failure status.
- `POST /api/messages` with `{ "message": "hello" }`: one turn at a time per identity; 75-second Hub HTTP deadline; the adapter enforces a shorter 45-second process-group deadline, leaving 30 seconds for cleanup and the error response.
- `GET /api/tasks`: only tasks involving the current identity.
- `POST /api/tasks` is no longer supported (405 Method Not Allowed); create a private proposal with `POST /api/proposals`, then explicitly confirm its reviewed revision with `POST /api/proposals/{id}/actions` (see below).
- `GET /api/tasks/{id}`: permitted task and its event log; nonparticipants receive 404.
- `GET /api/activity`: the caller's work index, newest first: one entry per task they take part in (latest shared step; the owner additionally sees their own private draft step) and per own pending/rejected proposal. Read-only, derived from existing records under the same visibility rules; private draft text and other users' proposals are never included.
- `POST /api/tasks/{id}/actions` with `{ "action": "accept", "expected_revision": 0 }`, `deliver` plus a `result`, `approve`, `return`, or `cancel`.

The caller cannot supply the requester, model session ID, task state or authorization. Unknown request fields are rejected. SQL values are parameterized. All state-changing task actions and events share a transaction. Repeated acceptance and terminal-state actions are rejected. Proposal creation supports a sender-scoped `request_id` (1–128 ASCII letters/digits/hyphens/underscores). Same key and same content return the persisted proposal; changed content gives 409. Confirm/reject are atomic and replay-safe at the originally reviewed revision, including after restart. Confirm creates exactly one task/event. Rejection cannot later be confirmed. All supported task creation now requires a sender-private proposal followed by an explicit sender confirmation of its recipient and summary at the reviewed revision (including any requester-written edits submitted with confirmation). The removed `POST /api/tasks` route cannot create tasks or events. Existing tasks remain readable and retain their lifecycle actions; this change does not rewrite stored task data or enable worker execution. Ordinary model chat is not idempotent; do not automatically retry a chat timeout.

## Storage and failures

`AGENCT_DATABASE` chooses the SQLite file; default `agenct.sqlite3`. Place it in a private directory. New database/lock files use mode 0600 on Unix; this does not repair pre-existing file permissions or encrypt data at rest. One process obtains an exclusive sidecar lock. Chat and tasks survive restart; pending turns become `interrupted`, never falsely completed. Failed adapter turns are recorded, no assistant answer is fabricated, and no automatic replay occurs. History sent to the adapter includes only completed messages belonging to the authenticated owner. Current adapter transport is local HTTP with a separate bearer token; it must run on the same trusted machine.

This is synchronous development chat, not a durable background job system. HTTP timeout/client disconnect may leave a pending record until process restart; operators must not infer completion or automatically replay. Restarting the Hub is not a way to cancel any future external action. Database records/events are not tamper-proof against machine administrators.

## Tests

```sh
cargo fmt --manifest-path hub/Cargo.toml -- --check
cargo test --manifest-path hub/Cargo.toml
cargo clippy --manifest-path hub/Cargo.toml --all-targets -- -D warnings
npm test
npm run check
node --check hub/web/client.js
```

Tests cover unknown identities, rejected caller-supplied identity, chat privacy, persistence, task participant filtering, action ownership, result requirement, cancellation and adapter failure with no fallback. They do not prove full security, live model compatibility or production readiness.

DSH 上下文仅传 role/content，不传消息状态或他人内容；请求超过 60,000 字节时从最旧消息开始裁剪。完整服务端历史仍保留。

## 本次验证范围

- 验证覆盖 Rust API/适配器协议、Python 离线适配器、静态演示及模拟 DOM 客户端；以当前测试命令的输出为准，不将历史测试数量视为验收证明。
- Rust fmt/clippy 和浏览器脚本语法检查通过。
- 此环境的 Chromium 因 socket 权限限制无法启动，因此新页面的真实浏览器验收尚未完成。不能把 API 测试当作 UI 验收。
- 未配置提供商密钥、未进行真实模型调用；DSH 实际插件禁用行为仍需按适配器文档验收。

Every task action must include the revision last reviewed by the caller. Each successful transition increments revision in the same transaction. Stale actions return 409 with no task/event mutation, including approval after return/redelivery. The UI binds buttons to the displayed revision. Existing development databases gain revision=0 on upgrade.


## Controlled delegation acceptance (offline test double)

Use two separate browser windows, one with Alice's development token and one with Bob's. In mock mode:

1. Alice sends `请让 bob 帮我检查公开文档` (a space after `帮我` is also accepted). A private proposal appears, with no task in Bob's window even after refresh.
2. Alice reviews the exact recipient and shared summary and clicks **确认共享并委派**. A task is persisted atomically. Only the displayed summary, participant identities and task lifecycle are shared, not Alice's private chat.
3. Bob waits for the visible-tab poll (or refreshes), accepts and submits a **human-written** result. This does not execute code or an agent worker.
4. Alice waits for the visible-tab poll (or refreshes), sees Bob's result and approves or returns it. All actions validate identity, state and reviewed revision on the server.
5. Replaying confirmation does not duplicate the task. Rejected proposals remain rejected. Eve cannot view or confirm Alice's proposal or task.

API additions:
- `GET /api/proposals`: only proposals created by the authenticated sender; recipients see nothing until confirmation.
- `POST /api/proposals`: `{ "owner": "bob", "summary": "review", "request_id": "optional-client-key" }`.
- `POST /api/proposals/{id}/actions`: `{ "action": "confirm", "expected_revision": 0 }` or `reject`. Confirmation also accepts optional `owner` and `summary` edits written by the requester, plus optional `details` shared material. Edits and task creation commit atomically; only another configured participant is allowed, the summary must contain 1–4000 UTF-8 bytes, and details 1–12000 bytes. Reject cannot include edits or details. A replay with different edits or details returns 409; it never changes the created task.
- `POST /api/messages` accepts optional `request_id` for recognized mock proposal fixtures only. Those fixtures persist the proposal, rather than a normal model conversation turn.

DSH mode accepts a strictly validated structured reply with an explicit `proposal` field (null or a bounded proposal). The Hub requires a configured recipient named in the current message and an exact excerpt of that message as the shared summary. This can prepare a sender-private proposal, never authorize sharing or create a task directly. The explicit `/delegate` proposal UI works in either mode. The SDK path is implemented but **live DSH model-backed extraction remains unvalidated**. The client schedules polling every three seconds, with bounded backoff and read timeouts; background tabs remain subject to browser timer throttling. The tab title shows pending work, and system notifications are opt-in; delivery depends on browser permissions, tab scheduling and connectivity. This is not server push. General natural-language task actions, autonomous execution, a running queue consumer, production authorization and multi-tenant security remain absent.

Run `cargo test --manifest-path hub/Cargo.toml`, `cargo clippy --manifest-path hub/Cargo.toml --all-targets -- -D warnings`, and `node hub/web/client.test.cjs`. The UI tests use a mocked DOM, not a real browser.

## Revision-bound explicit role selection

`POST /api/messages` accepts `agent_id` and `expected_profile_revision` together for an explicit per-turn role choice. The revision is the authenticated owner's profile revision, an integer from 0 through 9223372036854775807. Agent IDs contain 1–32 ASCII letters, digits, hyphens or underscores. A revision without an agent ID is invalid. Legacy callers may omit the revision; omitting both fields retains default coordinator routing.

The server checks the owner-scoped profile and freezes its runtime routing under the same mutex used by profile edits, before reserving a durable receipt or dispatching. A changed profile returns HTTP 409 with `code: "selected_agent_stale_before_dispatch"`; an unknown or disabled role returns HTTP 400 with `code: "selected_agent_unavailable_before_dispatch"`. Neither reserves a receipt, creates a message, dispatches, nor falls back to a different role. Clients may recover from these exact codes by refreshing the profile and asking for a new selection. Other errors, including receipt identity conflicts, must not be treated as proof that an existing request was never dispatched.

Durable receipt identity includes the supplied expected profile revision. Reusing a request ID with a different revision, including adding or removing one, conflicts. Identical pending, uncertain and completed requests replay their original receipt before consulting the current profile, so later changes or disabling the role cannot silently reroute a retry. Existing receipt databases migrate with a null expected revision and preserve legacy replay identity. This selection grants no tools, credentials, delegation or additional data access.

## Browser and visual regression checks

```sh
npm ci
npx playwright install chromium
npm run test:browser
npm run test:visual
```

The browser suite uses the real Chromium renderer with a fixture API. It covers send/recovery isolation, explicit delegation, private drafting and reviewed delivery, pending-work titles, and long Markdown overflow on mobile. It does not call a real model or establish remote deployment acceptance.

Visual comparisons cover 1330px, 1440px and 400px layouts in light and dark modes, with chat and task screenshots (12 baselines). Linux baselines use DejaVu Sans and Droid Sans Fallback; the Ubuntu 22.04 CI job installs both plus Chromium's system dependencies. After reviewing an intentional visual change, update with `npm run test:visual:update` and review the changed images before committing. CI retains the screenshot diffs, HTML report and failure traces for 14 days.
