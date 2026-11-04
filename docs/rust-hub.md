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

The UI supports ordinary chat and `/delegate bob <summary>` (or `/delegate alice <summary>`). `/delegate` now creates a **private proposal**, not a task. In mock mode, the bounded natural-language fixtures `请让 bob 帮我 <summary>` and `ask bob to <summary>` also create a private proposal. These are deterministic test-double patterns, **not general language understanding or live DSH model tool use**. Review the exact recipient and summary, then click confirm to share and create the task. Rejecting shares nothing. The proposal does not copy prior conversation into the shared summary. All configured participants are allowed to send one-hop tasks to each other. This limited policy is intended only for a consenting two-person trial. Adding a configured user gives them a token identity and the ability to receive/send such tasks; do not treat it as a production multi-tenant policy.

Recipients accept and submit **human-authored** results. Requesters accept/return results. This is a real task lifecycle but not independent artifact/CI verification. Tasks do not read private chat history. Model replies cannot create tasks, approve results, write a repository or execute commands through the Hub.

## API

All `/api/*` routes require `Authorization: Bearer <token>`; no query-string credentials or CORS are supported.

- `GET /api/me`: current server-resolved identity and runtime.
- `GET /api/messages`: most recent 40 personal messages, including failure status.
- `POST /api/messages` with `{ "message": "hello" }`: one turn at a time per identity; 75-second Hub HTTP deadline; the adapter enforces a shorter 45-second process-group deadline, leaving 30 seconds for cleanup and the error response.
- `GET /api/tasks`: only tasks involving the current identity.
- `POST /api/tasks` with `{ "owner": "bob", "summary": "shared task" }`.
- `GET /api/tasks/{id}`: permitted task and its event log; nonparticipants receive 404.
- `POST /api/tasks/{id}/actions` with `{ "action": "accept", "expected_revision": 0 }`, `deliver` plus a `result`, `approve`, `return`, or `cancel`.

The caller cannot supply the requester, model session ID, task state or authorization. Unknown request fields are rejected. SQL values are parameterized. All state-changing task actions and events share a transaction. Repeated acceptance and terminal-state actions are rejected. Proposal creation supports a sender-scoped `request_id` (1–128 ASCII letters/digits/hyphens/underscores). Same key and same content return the persisted proposal; changed content gives 409. Confirm/reject are atomic and replay-safe at the originally reviewed revision, including after restart. Confirm creates exactly one task/event. Rejection cannot later be confirmed. Direct low-level `POST /api/tasks` remains an explicit authenticated task-creation compatibility endpoint, without idempotency; models never call it and the UI no longer uses it. Ordinary model chat is not idempotent; do not automatically retry a chat timeout.

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

1. Alice sends `请让 bob 帮我 检查公开文档`. A private proposal appears, with no task in Bob's window even after refresh.
2. Alice reviews the exact recipient and shared summary and clicks **确认共享并委派**. A task is persisted atomically. Only the displayed summary, participant identities and task lifecycle are shared, not Alice's private chat.
3. Bob waits for the visible-tab poll (or refreshes), accepts and submits a **human-written** result. This does not execute code or an agent worker.
4. Alice waits for the visible-tab poll (or refreshes), sees Bob's result and approves or returns it. All actions validate identity, state and reviewed revision on the server.
5. Replaying confirmation does not duplicate the task. Rejected proposals remain rejected. Eve cannot view or confirm Alice's proposal or task.

API additions:
- `GET /api/proposals`: only proposals created by the authenticated sender; recipients see nothing until confirmation.
- `POST /api/proposals`: `{ "owner": "bob", "summary": "review", "request_id": "optional-client-key" }`.
- `POST /api/proposals/{id}/actions`: `{ "action": "confirm", "expected_revision": 0 }` or `reject`. Content is immutable and cannot be supplied to confirmation.
- `POST /api/messages` accepts optional `request_id` for recognized mock proposal fixtures only. Those fixtures persist the proposal, rather than a normal model conversation turn.

DSH mode accepts a strictly validated structured reply with an explicit `proposal` field (null or a bounded proposal). The Hub requires a configured recipient named in the current message and an exact excerpt of that message as the shared summary. This can prepare a sender-private proposal, never authorize sharing or create a task directly. The explicit `/delegate` proposal UI works in either mode. The SDK path is implemented but **live DSH model-backed extraction remains unvalidated**. The client polls while visible with bounded backoff and read timeouts; this is not push or browser notification delivery. General natural-language task actions, autonomous execution, a running queue consumer, production authorization and multi-tenant security remain absent.

Run `cargo test --manifest-path hub/Cargo.toml`, `cargo clippy --manifest-path hub/Cargo.toml --all-targets -- -D warnings`, and `node hub/web/client.test.cjs`. The UI tests use a mocked DOM, not a real browser.
