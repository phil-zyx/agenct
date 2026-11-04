# Output-only code worker contract

Status: tested Rust library foundation, **not a deployed code-execution service**.
`hub/src/worker.rs` implements a persistent SQLite queue and candidate verification
boundary. It performs no shell execution, network access, Git operation, GitLab or
GitHub write, publication, merge, or task completion. The Hub now exposes opt-in,
owner-confirmed queue admission/status/cancellation routes. There is still no
running queue consumer wired to the Hub. `worker_consumer.rs` provides an
opt-in consumer scaffolding; built-in Docker execution and evidence generation
are now disabled because repository tests could tamper with their verifier. See [executor integration](executor-integration.md). The existing human delivery
flow remains unchanged.

## Hub queue-only admission (experimental)

This is an admission receipt, **not working code execution**. No provider is
called, no code is run, no PR/MR is created, and no task is delivered automatically.
The built-in executor remains hard-disabled. No HTTP claim/finish/publish API exists.
The chat UI still uses manual task delivery. Each owner task has a “查看待办队列
（不执行）” button that opens a card in the same conversation, not a new dashboard.
It reads scope and receipts before displaying a separate “确认仅入队，不执行”
button. The exact task revision, repository and immutable base are shown before
consent. No scope means no enqueue button. Queue cancellation and rereading are
also inline. Closing a card, identity change or a refreshed task revision
invalidates its controls. Uncertain submissions are not retried automatically;
a deliberate retry after rereading keeps the original request ID until a matching
receipt is found. A terminal receipt is shown as ended, never as newly queued;
a separate explicit new-request confirmation generates a new ID after reconciliation.

Set `AGENCT_WORKER_SCOPES_FILE` to an operator-managed JSON array (maximum 128 KiB,
256 entries). Without it, no task can enqueue work. Each entry fixes exactly:

```json
[{"task_id":"EXISTING-TASK-ID","owner":"bob","task_revision":1,"repository":"agenct","base_commit":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}]
```

Use a real immutable commit for intended future work, not the illustrative hash.
Scopes are operator configuration, never model output or an endpoint accepting
browser-written policy. One entry per task; revision must match the accepted task.
Changing task state increments its revision and atomically cancels all queued,
running or review-ready jobs. A new revision needs new operator scope and new
explicit owner consent. Removing a scope prevents new admission, not deletion of
historical receipts; no consumer is running to act on those receipts.

- `GET /api/tasks/{id}/worker`: owner-only current scope and job receipts. Requester
  and unrelated users cannot read this queue. Always reports `execution_enabled:
  false` and `publication_enabled: false`.
- `POST /api/tasks/{id}/worker`: owner explicitly submits `expected_revision`,
  exact `repository`, exact `base_commit`, a stable `request_id`, and
  `confirm_queue_only: true`. It requires accepted/working task state and matching
  operator scope; accepting a task alone never enqueues it. Unknown fields fail.
- `POST /api/tasks/{id}/worker/{job}/cancel`: authenticated owner cancellation,
  bound to this task. Cancellation is idempotent and immediately revokes leases.

Example queue-only request body:

```json
{"expected_revision":1,"repository":"agenct","base_commit":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","request_id":"owner-generated-stable-uuid","confirm_queue_only":true}
```

Retain the same request ID after an uncertain response. An identical replay
returns the original receipt, including its cancelled status, and never requeues.
Changing fields under that ID conflicts; a second active job also conflicts.
A new intentional request after cancellation requires a new ID. Historical
receipts survive restart; attempts stay zero. Queue-only receipts persist an
execution-denied flag: `claim` cannot select them even if a consumer is later
connected. A replay through the execution-authorized library API cannot upgrade
the original receipt. Legacy unclassified rows migrate to execution-denied.
The reserved `queue-only-disabled` policy identity is only a library admission
marker, **not an executable identity or an isolation attestation**. Do not attach
an executor to it or infer execution permission from these receipts. Enabling
real execution requires a separate reviewed implementation, immutable evidence
verification, and explicit execution authorization, not changing this string.

## Integration

Use the Hub database (which must already have its `tasks` table), call
`worker::migrate`, and keep the connection behind the same transaction/mutex
boundary as other Hub writes. For multiple connections configure SQLite busy
handling; an `SQLITE_BUSY` error is safe to retry for enqueue with the same request
ID. Never ignore database errors.

1. A server-authenticated task **owner** explicitly requests execution after
   accepting the task. Pass the resolved identity, task revision, exact immutable
   base commit (40/64 lowercase hexadecimal characters), a server-defined
   repository key, and an owner-scoped request ID to `enqueue`.
2. `Policy` comes from trusted operator configuration. It contains a pinned
   executor identity, repository allowlist, and required check names. Absence of
   an executor fails closed. Model text and browser payloads must not set policy,
   executor identity, wall-clock time, or check results.
3. An independently authenticated internal worker calls `claim`. Each lease has
   an unpredictable capability (only its hash is persisted), a maximum lifetime
   of 300 seconds, and an attempt counter. A replacement lease fences out the
   previous capability. At three expired attempts the job fails. Explicit worker
   failure is terminal; a new request ID is required for an intentional retry.
4. Execution is currently disabled. A future implementation requires independently
   isolated trusted verification in addition to a reviewed sandbox. This library does not
   create or attest a sandbox. A worktree, process, Python virtualenv, or an
   executor name alone is **not isolation**. Do not wire host shell execution to
   this API as a substitute.
5. The trusted executor reads the actual repository and base/head objects, checks
   the candidate diff against them, runs configured checks on the exact candidate
   head, and calls `finish`. It must not forward an LLM's asserted test results.
   The crate-private `finish` validates repository/base binding, a distinct valid head commit,
   unique passing checks bound to that exact head, and all required checks. It
   stores at most a 1 MiB patch with a digest of the serialized candidate.
6. Successful finish means `review_ready`, **not approved or delivered**. Only
   the enqueuing owner can inspect its candidate. The caller may explicitly
   deliver a reviewed summary through the existing task flow; publishing a patch
   or creating a remote MR/PR needs a separate authorized integration. The separate `publication.rs` module supplies an explicit draft-MR grant and
   transport interface, not a configured live service; see executor integration.

All task authorization checks use database state, not the lease's copied request.
A changed task revision, owner, or non-working status invalidates result submission.
A claim pass cancels stale jobs; it also cancels jobs whose repository permission
was removed. Explicit cancellation fences an existing lease immediately. A
read-only inspection can show a pre-reconciliation status, so it must never be
used as authorization to publish or execute.

## Isolation and secret contract for a future executor

- Fresh nonprivileged container or VM per attempt; deny host filesystem mounts,
  Docker socket, ambient SSH agents, cloud metadata, and inherited credentials.
- Immutable input snapshot, ephemeral writable workspace, read-only toolchain,
  bounded CPU/memory/process count/disk/time, no privileged operation.
- Network disabled unless a narrowly defined operation was separately authorized.
  Untrusted repository tests and package scripts are code execution, not safe
  just because the repository is allowlisted.
- Cleanup on success, failure, timeout, cancellation, and lease loss. Poll job
  validity during long work. Lease fencing stops result acceptance, not an already
  running process, so process termination is the executor's responsibility.
- No external side effects from retryable jobs. Never place publication, payment,
  credential generation, or message sends inside the lease retry loop.
- Collect check evidence directly from tools; redact secrets from artifacts.
  The current candidate digest identifies saved evidence but is not a signature
  and does not independently prove that tests ran or that patch matches a commit.
- A separate publish integration must bind human approval to repository,
  base/head, artifact digest and action, recheck authorization immediately before
  acting, and reconcile uncertain remote outcomes before retrying.

## Storage and idempotence

The queue survives process restarts and uses immediate SQLite transactions for
claim/enqueue/finalization. `(owner, request_id)` is unique; changed content under
the same key is rejected. A replay returns the original job even after it failed
or was cancelled, and never restarts execution. Only one queued, running, or
review-ready job per task is allowed. Lease capabilities should remain internal
and must not appear in logs or user interfaces. All time arguments must come from
one trusted server clock; this contract does not tolerate arbitrary client clocks.
Candidate artifacts remain local to the owner, even after cancellation.

## Tests

Run `cargo test --manifest-path hub/Cargo.toml --lib worker::tests`.
Tests cover fail-closed configuration, owner/revision/status authorization,
repository restrictions, durable reopen, exclusive claims across connections,
idempotency and conflicting replays, stale lease fencing and attempt limits,
revocation/cancellation, exact-commit checks, cross-owner reads, tampered lease
payloads, terminal failure and no automatic task completion. They do not establish
real sandbox isolation, live provider success, or remote publication behavior.
