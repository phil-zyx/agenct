# Experimental DSH adapter

The Rust Hub is the identity, authorization and conversation authority. This adapter is an **internal, loopback-only** HTTP bridge to the official DeepSeek Harness Python SDK. It is not a public API, sandbox, production multi-tenant service, or standalone assistant product.

## What exists and what was verified

- A real SDK integration path: `DeepSeekHarness(..., profile="sdk-minimal", patches=(...), dsh_home=...)`, followed by `run(..., session_id=...)` and context-managed shutdown.
- Official SDK and bundled runtime pinned to `0.2.1rc2`. Constructor arguments were verified against that published SDK wheel. Disabled profile row IDs were verified against the matching upstream `dsh-v0.2.1-rc.2` tag, not guessed from a different release.
- Seven offline boundary/lifecycle tests use a fake SDK or process. **No provider credentials were used, no model request was sent, and the actual runtime plus patch has not yet passed a live integration test.** This is a foundation, not a claim that real DSH chat is already operating.
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

Every request gets a separate temporary home/workspace and runtime process. History is reconstructed from Hub data; DSH resume/persistent memory is intentionally unused. Inputs are wrapped as JSON conversation text so a slash-prefixed message is not routed as a native SDK command. The wrapper is not a prompt-injection defense; disabling capabilities is the boundary.

`no-tools.patch.yml` disables the sdk-minimal profile's file, working-directory and persistent shell tool producers, plus session-log and package-inventory telemetry plugins. No MCP servers, external tools or cross-user sessions are configured. Per-request directories are removed after completion; the SDK is closed via its context manager. The adapter kills the worker's process group on both normal return and a hard 45-second timeout. The SDK's own request timeout is not treated as an end-to-end deadline.

Only an allowlist of environment variables reaches the worker. This does not provide OS-level confinement: the upstream runtime still runs with the adapter OS account's filesystem/network privileges. A runtime vulnerability or changed plugin behavior could escape application-level restrictions. **Before public or multi-tenant deployment, use an isolated unprivileged container/VM with resource limits, controlled egress, no host mounts/secrets, and verify the effective runtime tool inventory.** The HTTP server is a bounded-active-work development server, not an Internet-facing production gateway.

This profile patch must be re-audited on dependency upgrades: upstream missing-row patches can warn rather than fail. The version guard refuses any SDK/runtime version other than the pinned one. Next acceptance gate: install the exact runtime, inspect its resolved profile/tool inventory without a provider call, then run an explicitly authorized live smoke test for two independent Hub users and failure/cancellation behavior. Do not enable tools merely to make this chat smoke test pass.

## Offline tests

```sh
python3 -m unittest discover -s adapters/dsh -v
```

Tests cover authentication, malformed and cross-boundary input rejection, slash-command wrapping, SDK profile configuration, result failure handling, context cleanup, process-group timeout cleanup, environment minimization, distinct request homes, and removal of temporary state. They do not prove actual SDK plugin disabling or operating-system sandbox isolation.

## Primary sources

- [Official SDK source at the pinned tag](https://github.com/deepseek-ai/deepseek-harness/tree/dsh-v0.2.1-rc.2/python/sdk)
- [Pinned sdk-minimal profile](https://github.com/deepseek-ai/deepseek-harness/blob/dsh-v0.2.1-rc.2/packages/bundle/sdk-minimal/cordis.patch.yml)
- [Official Python SDK guide](https://github.com/deepseek-ai/deepseek-harness/blob/master/docs/user/guide/python-sdk.md)

Timeout ordering: the adapter kills its process group at 45 seconds; the Hub HTTP deadline is 75 seconds, leaving 30 seconds for cleanup and the error response before its per-user gate releases. Network disconnection or adapter crash is still an uncertain outcome, not a durable cancellation protocol. No automatic retries or side-effecting tools are enabled.
