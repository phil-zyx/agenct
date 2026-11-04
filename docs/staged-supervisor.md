# Staged host supervisor: benign fixtures only

This stage adds a real lifecycle integration point to the Docker boundary smoke.
It does **not** enable the Rust worker, general repositories or production execution.
`sandbox/staged_supervisor.py::execute_verified_job` always raises
`ExecutionDisabled`; no flag, image digest or successful staging observation can
bypass that boundary. The Rust preflight remains default-deny and sealed.

## What runs in CI

The existing `docker-boundary.yml` job still uses a disposable GitHub-hosted Linux
runner, a scratch image and the repository's fixed C probe. No image is pulled,
no credentials/provider/GitLab calls occur, and no repository-supplied test command
or arbitrary candidate source enters a container.

The harness now invokes `observe_benign`, which creates its own single-file,
content-verified fixture. Only the fixed `probe` and `hang` checkers are accepted.
The trusted host adapter supplies the image ID built from that fixed probe, never
an image selected by a model or external HTTP request.

The staged supervisor:

1. Checks authorization and deadline before any create operation, then registers
   the unpredictable container name before a potentially uncertain create result.
2. Binds the observed container ID, name, image ID, snapshot labels, checker argv,
   user, working directory and isolation/resource policy through trusted daemon
   inspection. It verifies the container has never started.
3. Independently compares the stopped container's source archive to the exact
   host-derived snapshot. It re-inspects identity/policy/state before starting.
4. Polls daemon state with bounded per-operation deadlines and an authorization
   callback. It never interprets container stdout, stderr or files as test evidence.
5. Requires successful terminal state on the same container/image and compares
   the source archive again. This final comparison is defense in depth, not a
   replacement for the read-only boundary.
6. Forcibly removes the named container and requires confirmed absence, including
   after timeout, revocation, failed verification or uncertain creation.
7. Rechecks authorization/deadline after cleanup. Only then returns a frozen
   `StagingObservation` with `production_eligible=False` and the exact image,
   container, commit, snapshot and policy digests.

The real smoke exercises the successful fixed probe, a started hang fixture that
hits its deadline, and revocation immediately after starting another hang fixture.
Both failure cases must clean up and return no success observation. The outer
harness additionally attempts all container/image cleanup even if inner cleanup
fails. No broad daemon pruning is used.

## Test coverage and validation limits

Offline supervisor tests cover authorization before create, revocation after
start/during cleanup, timeout, uncertain create, cleanup failure, changed container
ID/image/labels/policy, changed final source, nonzero exit, restricted checker inputs
and unconditional production disablement. The policy validator is now separate
from terminal-result validation, so pre-start inspection no longer fabricates a
successful terminal state just to check container configuration.

Run offline tests with:

`python3 -m unittest discover -s sandbox -p 'test*.py'`

Run the fixed real-Docker stage only on an authorized disposable host:

`python3 sandbox/docker_smoke/run.py`

The earlier boundary smoke passed in GitHub-hosted CI before this stage was added.
This new supervisor path has only been tested offline in the development workspace;
its real-Docker CI result must be checked separately. A passed benign run would
establish only the exercised fixed-fixture behaviors, not safe execution of
adversarial repositories or production readiness.

## Gates still required for general jobs

- Independently reviewed trusted image/checker provenance and candidate-image
  construction, including metadata/environment/volume/symlink-alias checks.
- A bounded production daemon transport, trusted host execution identity and
  authenticated durable lease/revocation integration. The current CLI adapter is
  restricted to fixed benign fixtures, not a hostile-output transport.
- Hostile-input/security tests for resource exhaustion, output floods, image and
  object parsing, process attacks, evidence-channel forgery and namespace isolation.
- Crash/restart orphan reconciliation, durable cleanup accounting, data retention,
  multi-tenant threat-model review and an appropriate container or VM boundary.
- An explicit reviewed change to the sealed Rust executor only after all required
  real safety checks pass. There is intentionally no environment flag to enable it.

The exact immutable fixture and daemon-observed process exit are the assurance
here. Arbitrary repository tests may lie or intentionally return zero. Neither a
staging observation nor a container exit code proves a user's task is correct.

## Optional durable benign-fixture cleanup

The smoke accepts `--journal-directory /private/host/path` (the CI job uses a
runner-local directory). This activates a host-owned SQLite cleanup journal;
without this option the existing smoke remains in-memory only. The directory
must be owned by the current user, mode 0700, on a trusted local durable filesystem.
Do not put it inside a candidate checkout, shared mount, or container. SQLite uses
FULL synchronous commits. This stage verifies process-crash recovery only, not
power-loss durability. Initialization fsyncs the directory, but DELETE-mode FULL
is not a demonstrated power-loss guarantee for journal deletion on every filesystem.
An exclusive advisory lock is held for the entire
smoke/recovery lifetime. Concurrent use of the same journal fails closed.

Before creating a fixture, the supervisor durably records its unpredictable name,
image ID and snapshot digest. It records successful create return and subsequently
binds the inspected container ID. Startup reconciles pending entries before new
fixtures run. Recovery checks name, image and both ownership labels, removes only
that exact inspected ID, and checks absence again. Identity mismatches, daemon
errors and unconfirmed removals preserve the record and stop new work. Recovery
never starts a container, returns test evidence, or enables production execution.

If the process crashed or timed out during creation and no container is visible,
that intent deliberately remains pending: absence does not prove a delayed daemon
create cannot finish later. The operator must investigate the daemon and resolve
that uncertainty; there is no automatic forget, force-clear, or broad prune path.
Re-running with the same journal can clean it once the matching container appears.
A confirmed create or recorded ID may be reconciled by definitive absence.

This is narrowly durable **container** accounting, not a general production
recovery service. Image build/tag cleanup remains the outer smoke's in-memory
accounting. A process crash may leave its scratch fixture image behind. Host loss,
unreliable filesystems, malicious host owners and destruction of the journal are
outside this stage. A new disposable CI runner cannot recover another runner's
resources. The journal-enabled real Docker path still requires its own CI result;
offline reopen/failure tests do not establish real daemon crash behavior.

Authorization and deadline are rechecked after journal registration immediately
before create. If that check fails, the supervisor removes only its known-unissued
intent, while still holding the lock; it never clears an uncertain issued create.
Tests include a child process killed with SIGKILL after a committed ID binding,
lock reacquisition and cleanup after reopening, and injected registration failure.
These remain fake-engine tests and do not prove actual daemon restart behavior.
