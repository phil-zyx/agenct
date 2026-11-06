# Experimental DSH adapter

The Rust Hub is the identity, authorization and conversation authority. This adapter is an **internal, loopback-only** HTTP bridge to the official DeepSeek Harness Python SDK. It is not a public API, sandbox, production multi-tenant service, or standalone assistant product.

## What exists and what was verified

- A real SDK integration path: `DeepSeekHarness(..., profile="sdk-minimal", patches=(...), dsh_home=...)`, followed by `run(..., session_id=...)` and context-managed shutdown. A disabled persistence prototype exercises the pinned SDK’s explicit `resume_session(id).run(...)`; production configuration cannot enable it.
- Official SDK and bundled runtime pinned to `0.2.1rc2`. Constructor arguments were verified against that published SDK wheel. Disabled profile row IDs were verified against the matching upstream `dsh-v0.2.1-rc.2` tag, not guessed from a different release.
- Twenty-four offline boundary/lifecycle tests use a fake SDK, process, or guarded local storage. **No provider credentials were used, no model request was sent, and the actual runtime plus patch has not yet passed a live integration test.** This is a foundation, not a claim that real DSH chat is already operating.
- There is no mock model response inside this service. Missing credentials, version mismatch, timeouts, and runtime failures fail explicitly. Rust's separately selected mock runtime is for UI/Hub development only.

## Local configuration

Requires Python 3.11+ and Linux/macOS (process-group termination). In the repository root:

```sh
python3 -m venv .venv-dsh
.venv-dsh/bin/pip install -r adapters/dsh/requirements.txt
# Set these through your local secret manager/shell, never commit values:
# AGENCT_ADAPTER_TOKEN: a random >=32-character non-whitespace ASCII shared secret
# DEEPSEEK_API_KEY: your own provider key
# Optional AGENCT_DSH_MODEL, default deepseek-v4-flash
# Optional AGENCT_ADAPTER_PORT, default 8788
.venv-dsh/bin/python adapters/dsh/adapter.py
```

Configure the Hub's DSH runtime URL as `http://127.0.0.1:8788/v1/turn` and give the Hub the same internal bearer token. Follow the Hub instructions for its environment variable names. The provider key belongs only to the adapter process, never browser JavaScript or the Hub API payload. This adapter deliberately does not accept user-selected provider URLs, SDK binary paths, profile names, shell commands, arbitrary patches, or workspace paths.

Actual DSH mode sends the supplied conversation history and message to DeepSeek to obtain an answer, with normal provider data handling and charges. The upstream SDK/runtime is a separate dependency with its own license and terms; see the upstream repository. Operators must review these before enabling real provider traffic.

## Wire contract

`POST /v1/turn`, `Content-Type: application/json`, `Authorization: Bearer <internal token>`:

```json
{"session_id":"07a65954-7ee2-4ca3-a63a-09c9828b303b","history":[{"role":"user","content":"Earlier message"},{"role":"assistant","content":"Earlier answer"}],"message":"Current message"}
```

Success requires all three fields: `{"reply":"...","proposal":null,"backend":"dsh"}`. A delegation draft instead contains `"proposal":{"owner":"bob","summary":"review tests"}`. The SDK model must emit exactly `reply` and `proposal`; the adapter validates them before adding `backend`. Missing, duplicate, unexpected or malformed fields, Markdown fences, and unstructured model text fail closed. `proposal` must be explicitly null when no delegation is proposed.

Both adapter and Hub require the proposal recipient to appear in the current message, and the nonempty summary to be an exact contiguous excerpt from that message, at most 4,000 UTF-8 bytes. The Hub additionally checks the recipient is another configured participant. Private history cannot supply shared summary text. The Hub persists only a sender-private pending proposal; the recipient sees nothing until the requester explicitly confirms the immutable recipient and summary. Rejection sends nothing. Model output cannot authorize or execute a task. Ambiguous requests should produce a clarifying reply with `proposal:null`; malformed output is not silently repaired or retried.

Errors: 400 invalid turn, 401 unauthorized, 404 unknown route, 502 runtime failure, 503 capacity exhausted, 504 total runtime timeout. No automatic retry, partial-success response, or mock fallback occurs. A failed/timed-out call may already have incurred provider charges; cancellation cannot undo a remote request.

The Hub must authorize the caller and choose the session UUID/history itself. Possession of the internal token gives access to all adapter operations; the adapter cannot independently establish user ownership. Never expose port 8788, forward browser bearer tokens, or share the internal token with end users.

## Isolation and capability limits

By default every request gets a separate temporary home/workspace and runtime process. History is reconstructed from Hub data. The disabled persistence prototype is described below; no supported configuration resumes SDK state between turns. Inputs are wrapped as JSON conversation text so a slash-prefixed message is not routed as a native SDK command. The wrapper is not a prompt-injection defense; disabling capabilities is the boundary.

`no-tools.patch.yml` disables the sdk-minimal profile's file, working-directory and persistent shell tool producers, plus session-log and package-inventory telemetry plugins. No MCP servers, external tools or cross-user sessions are configured. In default mode, per-request directories are removed after completion; the SDK is closed via its context manager. The adapter kills the worker's process group on both normal return and a hard 45-second timeout. The SDK's own request timeout is not treated as an end-to-end deadline.

Only an allowlist of environment variables reaches the worker. This does not provide OS-level confinement: the upstream runtime still runs with the adapter OS account's filesystem/network privileges. A runtime vulnerability or changed plugin behavior could escape application-level restrictions. **Before public or multi-tenant deployment, use an isolated unprivileged container/VM with resource limits, controlled egress, no host mounts/secrets, and verify the effective runtime tool inventory.** The HTTP server is a bounded-active-work development server, not an Internet-facing production gateway.

This profile patch must be re-audited on dependency upgrades: upstream missing-row patches can warn rather than fail. The version guard refuses any SDK/runtime version other than the pinned one. Next acceptance gate: install the exact runtime, inspect its resolved profile/tool inventory without a provider call, then run an explicitly authorized live smoke test for two independent Hub users and failure/cancellation behavior. Do not enable tools merely to make this chat smoke test pass.

## Persisted SDK sessions are disabled

**Do not set `AGENCT_DSH_STATE_DIR`.** The adapter explicitly fails at startup with `persistent_dsh_sessions_not_verified` whenever this variable is present, before opening a store or invoking a runtime. Default request-local temporary homes remain supported. Persisted conversational context remains the Hub's responsibility and is supplied to each fresh runtime.

The repository retains an internal prototype and offline tests for private per-session homes, explicit `resume_session`, history acknowledgment, uncertain-outcome journals, model/profile fingerprints, and cross-process locks. Tests wire that prototype directly; there is no production configuration override. The pinned Python SDK documents resume failure rather than implicit creation, and `sdk-minimal` includes separate JSONL session persistence, but those facts alone do not establish crash-safe turn durability.

**Why disabled:** a ready adapter journal can acknowledge a response while the SDK JSONL is missing that turn, truncated, or rolled back. Matching Hub-history hashes prove only what the adapter accepted, not what the SDK can restore. Hashing/fsyncing an arbitrary SDK home after shutdown would detect some later file changes but would still not prove buffered events had reached the correct completed-turn checkpoint before the snapshot. The prototype is therefore not a safe restoration boundary and must not be enabled by editing around the gate.

Before enabling persistence, implement and test a version-pinned SDK checkpoint contract that binds the exact completed session/turn to durable JSONL content, verifies it before resume, and refuses stale/corrupt/reordered state. Include actual-runtime crash/restart tests at each write/flush/ack boundary, multi-user isolation, and Hub/SDK reconciliation. Failures must never trigger silent fresh-session fallback or automatic replay of possibly charged requests. Until then, no live persistent DSH memory claim is made.

Regression tests assert that production rejects persistence before runtime invocation for empty stores and for ready journals paired with stale or truncated fake SDK logs. The remaining prototype tests exercise local guard logic only; they do not validate SDK persistence or model behavior.

## Offline tests

```sh
python3 -m unittest discover -s adapters/dsh -v
```

Tests cover authentication, malformed and cross-boundary input rejection, slash-command wrapping, SDK profile configuration, result failure handling, context cleanup, process-group timeout cleanup, environment minimization, distinct request homes, and removal of temporary state. They do not prove actual SDK plugin disabling or operating-system sandbox isolation.

## Credential-free Hub/adapter integration smoke

With the release Hub already built, on a host where loopback socket tests are permitted:

```sh
python3 scripts/smoke-hub-adapter.py
```

The private-staging CI workflow runs this after its release build. It starts the real Rust binary through the staging launcher and a separate Python process using the production `adapter.make_server`. Only the smoke script supplies a deterministic, clearly labeled test runner. Successful fixture replies use the production `parse_model_response`; one deliberate malformed-wire fixture bypasses that parser to test the Hub's own rejection. The production adapter and its configuration have no fake-runner mode.

The smoke uses generated temporary Alice/Bob and internal bearer credentials, owner-only files, an empty private home, and explicit subprocess environments that do not inherit provider keys, proxies, Python hooks, OIDC configuration, or DSH persistence flags. It installs and imports no SDK and makes no provider call. Its HTTP client only addresses loopback, rejects redirects, and never retries writes.

Coverage includes the real internal authentication boundary, distinct and stable owner session UUIDs, exact completed-only private histories, selected personal role/revision routing, rejection before dispatch for stale selection, and frozen completed receipts after profile edits. A proposal remains requester-private until the requester confirms its exact revision; recipient/stale confirmation attempts create nothing. Injected runner failure and malformed output become uncertain receipts with no extra dispatch on status checks or repeated requests, including after gracefully restarting both processes. Fresh turns after restart must exclude uncertain input and other-owner history.

This is protocol and **Hub SQLite persistence** coverage, not live DSH acceptance, real network-timeout/cancellation coverage, a crash-durability test, model quality, SDK tool disabling, an OS sandbox, or persisted SDK sessions. The live-runtime and persistence gates above remain unchanged. The separate offline harness tests (`python3 -m unittest discover -s scripts -p 'test_*.py' -v`) open no sockets and cannot establish that this cross-process smoke passes.

## Primary sources

- [Official SDK source at the pinned tag](https://github.com/deepseek-ai/deepseek-harness/tree/dsh-v0.2.1-rc.2/python/sdk)
- [Pinned sdk-minimal profile](https://github.com/deepseek-ai/deepseek-harness/blob/dsh-v0.2.1-rc.2/packages/bundle/sdk-minimal/cordis.patch.yml)
- [Official Python SDK guide](https://github.com/deepseek-ai/deepseek-harness/blob/master/docs/user/guide/python-sdk.md)

Timeout ordering: the adapter kills its process group at 45 seconds; the Hub HTTP deadline is 75 seconds, leaving 30 seconds for cleanup and the error response before its per-user gate releases. Network disconnection or adapter crash is still an uncertain outcome, not a durable cancellation protocol. No automatic retries or side-effecting tools are enabled.

## Recipient delivery drafting protocol (#16)

The adapter accepts `kind: "draft_delivery"` in `/v1/turn`. Omitted `kind` means
legacy chat; explicit `"chat"` is equivalent. Other kinds are rejected. This is
an adapter protocol capability, **not** an enabled Hub draft endpoint or UI.

A draft turn requires all existing session/history/message fields, plus validated
`runtime_profile` and `task_context`. History must be empty, task status must be
`working`, and message must be exactly:

`Draft a delivery for this task for owner review.`

Use the existing task snapshot schema and 8,192-byte compact JSON bound. The Hub
must supply only the explicitly reviewed task snapshot, not either participant's
private chat, and generate a separate request-local session UUID. The adapter
validates UUID syntax but cannot establish ownership, receipt uniqueness, or that
a UUID differs from a chat session: these are mandatory #15 Hub responsibilities.
Profile routing remains server-default; role guidance grants no permissions.

Draft response JSON remains exactly `reply`, `proposal`, `backend`. Model output
contains only `reply` and `proposal`; the adapter adds backend `dsh`. Reply must be
nonblank and at most 8,000 UTF-8 bytes, and proposal must be explicitly null. Both
SDK output and subprocess output undergo kind-specific validation. Draft resume
is rejected before SDK construction or subprocess launch. Task strings stay quoted
untrusted data; the prompt grants no action authority. Existing no-tools patch,
request-local temporary workspace and process cleanup remain unchanged.

Dependent work: #15 must authorize owner/working/revision, reserve a durable receipt
before dispatch, freeze task/profile/session bindings, reject nonnull proposals
and oversized text independently, atomically persist private draft plus receipt,
and recover uncertainty without redispatch. #11 must preserve immutable drafts
and separate explicitly reviewed submitted records. Neither protocol support nor
model output authorizes delivery, approval, execution or publication.

Offline tests in `adapters/dsh/test_draft_delivery.py` cover input rejection before
SDK/runner, context-only prompts, malicious task text, UTF-8 boundaries, forbidden
proposals, chat compatibility, no resume, and subprocess revalidation/cleanup.
They use fake SDK/processes and a local fixture HTTP listener. They do not verify
live DSH behavior, actual tool inventory, cross-user authorization or Hub receipts.
