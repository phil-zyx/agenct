# Rust Hub technical proof of concept

This is a **loopback-only development server**, not an Internet-ready account system. It does not replace or change the hosted browser demo. It supplies real server-side storage and authorization; mock model output is explicitly labelled. SQLite is used to make the single-process experiment runnable without a PostgreSQL service. PostgreSQL, OIDC, deployment hardening, worker leases and actual code execution remain future work.

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

The only current UI commands are ordinary chat and `/delegate bob <summary>` (or `/delegate alice <summary>`). Delegation is a user command, **not autonomous model tool use**. All configured participants are allowed to send one-hop tasks to each other. This limited policy is intended only for a consenting two-person trial. Adding a configured user gives them a token identity and the ability to receive/send such tasks; do not treat it as a production multi-tenant policy.

Recipients accept and submit **human-authored** results. Requesters accept/return results. This is a real task lifecycle but not independent artifact/CI verification. Tasks do not read private chat history. Model replies cannot create tasks, approve results, write a repository or execute commands through the Hub.

## API

All `/api/*` routes require `Authorization: Bearer <token>`; no query-string credentials or CORS are supported.

- `GET /api/me`: current server-resolved identity and runtime.
- `GET /api/messages`: most recent 40 personal messages, including failure status.
- `POST /api/messages` with `{ "message": "hello" }`: one turn at a time per identity; 60-second adapter deadline.
- `GET /api/tasks`: only tasks involving the current identity.
- `POST /api/tasks` with `{ "owner": "bob", "summary": "shared task" }`.
- `GET /api/tasks/{id}`: permitted task and its event log; nonparticipants receive 404.
- `POST /api/tasks/{id}/actions` with `{ "action": "accept", "expected_revision": 0 }`, `deliver` plus a `result`, `approve`, `return`, or `cancel`.

The caller cannot supply the requester, model session ID, task state or authorization. Unknown request fields are rejected. SQL values are parameterized. All state-changing task actions and events share a transaction. Repeated acceptance and terminal-state actions are rejected. There is not yet request-id idempotency for task creation; repeated create commands create separate tasks.

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

- 10 个 Rust API/适配器协议测试通过；8 个 Python 适配器离线测试通过；原静态演示 18 个测试通过。
- Rust fmt/clippy 和浏览器脚本语法检查通过。
- 此环境的 Chromium 因 socket 权限限制无法启动，因此新页面的真实浏览器验收尚未完成。不能把 API 测试当作 UI 验收。
- 未配置提供商密钥、未进行真实模型调用；DSH 实际插件禁用行为仍需按适配器文档验收。

Every task action must include the revision last reviewed by the caller. Each successful transition increments revision in the same transaction. Stale actions return 409 with no task/event mutation, including approval after return/redelivery. The UI binds buttons to the displayed revision. Existing development databases gain revision=0 on upgrade.
