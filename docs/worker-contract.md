# Output-only code worker contract

Status: tested Rust library foundation, **not a deployed code-execution service**.
`hub/src/worker.rs` implements a persistent SQLite queue and candidate verification
boundary. It performs no shell execution, network access, Git operation, GitLab or
GitHub write, publication, merge, or task completion. There are currently no HTTP
routes or running queue consumer wired to the Hub. `worker_consumer.rs` provides an
opt-in consumer scaffolding; built-in Docker execution and evidence generation
are now disabled because repository tests could tamper with their verifier. See [executor integration](executor-integration.md). The existing human delivery
flow remains unchanged.

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
