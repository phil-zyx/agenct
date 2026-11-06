> 历史专题记录：保留设计背景、协议细节和当时的验收结果；其中“当前/尚未”描述可能早于最新实现。现行用法以[入门](../getting-started.md)和[参考](../reference.md)为准。

# Optional executor and GitLab integration contracts

These are opt-in backend modules. The Hub does **not** automatically start a
worker or publish any code. They are not enabled merely by accepting a chat task.
The existing demo continues to use human-delivered results.

## Security stop: built-in execution is disabled

The original container prototype ran tests with the same UID and writable Git
metadata as its verifier. A check can modify the index, set `assume-unchanged`,
use replacement refs or otherwise alter Git state so that a later `git diff`
appears clean. Therefore it could not establish that passing checks actually ran
against the claimed candidate commit. A container boundary around the entire
runner does not isolate the verifier from the code being tested.

The built-in path now fails closed:

- `consume_one` checks sealed executor preflight **before claiming a lease or
  calling an input provider**. Disabled execution leaves the job queued with zero
  attempts and makes no source/model request.
- `DockerExecutor::run` launches no process and returns `ExecutorUnavailable`,
  even with a valid pinned image and allowed check configuration.
- `sandbox/runner.py` always exits unsuccessfully without cloning, applying a
  patch, running checks or emitting candidate evidence.
- `Executor` is sealed. External implementations cannot simply assert that their
  unchecked output is trusted evidence through this public consumer interface.
- `worker::finish` is crate-private. The public library offers no direct route
  for an external caller to submit arbitrary successful check assertions.
- Queue/policy/publication contracts remain available for tests and review. Only
  in-crate fake executors exercise the positive candidate lifecycle in tests.
  These fixtures are not proof of real execution or verification.

`DockerExecutor::arguments` retains a proposed no-network/no-host-mount,
nonroot/read-only/resource-limited container plan for review. **It is not run.**
Neither installing Docker nor building the supplied image re-enables execution.
The entrypoint and Dockerfile are disabled protocol scaffolding, not an approved
sandbox implementation. No host execution fallback exists.

## Required before re-enabling

A redesigned implementation must have an independently trusted verifier that
repository code cannot modify, signal, impersonate, or feed forged check output.
It must own an immutable exact-commit snapshot and observe checks in a separate
isolation boundary. Untrusted checks must not control verifier processes, Git
metadata, evidence channels or the identity of the tested source. Extra Git diff
commands or filesystem flags inside the same trust boundary are insufficient.

The design also needs bounded resource/time/output limits, authorization
revocation, cleanup and crash-orphan reconciliation, audited image/toolchain
provenance, and adversarial regression tests for index flags, replacement refs,
metadata rewriting and evidence-channel attacks. A security review must establish
which isolation boundary is appropriate before re-opening the sealed executor API.

The queue still handles durable claims, idempotency, revision checks and lease
fencing. Those controls do not independently prove test execution. Integrating a
bounded source/model provider, HTTP/chat execution controls or GitLab publication
does not remove this security stop. The existing human-delivered demo workflow is
unaffected and remains the only enabled end-user completion path.

## GitLab publication boundary

`GitLabTransport` is an injectable interface, **not a live HTTP client**. A real
implementation must use bounded TLS requests to the configured origin with
server-held, least-privilege credentials, reject cross-origin redirects, use
numeric project IDs and exact branch/SHA matches, and redact errors/secrets.
Nothing in this change creates credentials or contacts GitLab.

`authorize` must be called only after the authenticated owner approves the exact
candidate digest, source branch, target project/branch and MR title. It creates a
persistent immutable intent. This grants **draft MR creation only**. It does not
grant code upload/push, merge, review approval, settings changes or credential
creation. The exact candidate head must already exist on the source branch through
a separately authorized process. Branch creation/upload is not implemented.

The transport's create call must make a draft MR with an unguessable durable
marker derived from `Intent.id`. `find_merge_request` must match that marker plus
project, source and target. The library checks returned project/branches/head,
draft status and same-origin MR URL. Both source head and target base must match
before the initial send. After reserving the attempt, any network error or crash
is treated as uncertain. Subsequent calls reconcile only; even an absent search
result never causes an automatic repeat write. A human/operator must investigate
unresolved attempts. Do not bypass this journal with direct provider calls.

CI verification is read-only and accepts only unique passing required checks on
the approved head SHA while the source branch still points at it. It never merges
or marks the Hub task complete. It is not a polling daemon. Target-branch changes,
permissions and MR state must be revalidated again by any future merge operation;
merging is deliberately absent from this interface.

## Remaining integration work

- Independent trusted verification and isolated check execution, followed by
  adversarial security tests and real Docker/VM validation.
- Authenticated HTTP/chat execution controls and operator configuration loading.
- Bounded live source/model patch provider and artifact retention/redaction policy.
- Least-privilege TLS GitLab transport, separately approved source publication,
  actual MR/CI end-to-end tests and progress polling.
- Production authentication, deployment hardening and independent security review.

## Offline tests

`cargo test --manifest-path hub/Cargo.toml`

`python3 -m unittest discover -s sandbox -p 'test_*.py'`

Regression tests prove the built-in executor cannot launch or produce successful
evidence, including a configured `assume-unchanged`/replacement-ref attack command.
The fake provider tests separately cover lifecycle success/failure, source binding, exact check
allowlisting, owner/digest authorization, changed branches, cancelled tasks,
uncertain/crashed sends without duplicate writes, wrong-origin URLs and wrong-SHA
or missing CI evidence. None of these tests executes untrusted repository code.

## Independent verifier prototype

See [independent verifier](independent-verifier.md) for the implemented host-side
Git-object and source-archive verification, read-only per-check container plan,
offline tests, narrow assurance claims and exact remaining runtime blockers.
This prototype does not re-enable the sealed executor or produce accepted worker
evidence.
