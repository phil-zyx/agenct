# Independent verification prototype and security boundary

Status: **offline implementation and architecture, execution still disabled**.
The Rust executor remains sealed and disabled by the security fix. The container
entrypoint still exits unsuccessfully. `independent_verifier.execute` also raises
`ExecutionDisabled` unconditionally. This document does not authorize re-enabling
any of them.

## What is implemented

`sandbox/independent_verifier.py` implements host-side input verification without
running Git, a shell, repository hooks, source code or Docker:

1. Read a server-selected **loose** Git object store using no-follow descriptor
   reads. Bound compressed/decompressed object sizes and reject trailing compressed
   data. Packed objects are deliberately unsupported.
2. Independently verify SHA-1 or SHA-256 object identity from canonical Git object
   type/length/content, even when using a custom object reader. Parse the exact
   approved commit's tree directly. No branch names, index, replacement refs,
   attributes, Git configuration or checkout filters participate.
3. Recursively collect ordinary blob files with bounded depth, total object reads,
   entry count, file count and total bytes. Reject symlinks, submodules, `.git`,
   traversal/absolute/control-character/non-UTF-8 paths, duplicate names and
   unsupported file modes. Executable bits are retained.
4. Construct a sorted manifest of paths, modes, sizes and SHA-256 file contents,
   bound to the exact commit/tree, and hash its canonical serialization.
5. Generate a deterministic source-image context with a **host-generated**
   Dockerfile containing only an audited pinned `FROM`, `COPY`, labels, `USER` and
   `WORKDIR`. A repository's Dockerfile is just source data, never executed.
6. Verify a source archive exported by the trusted daemon from a **never-started**
   container against the exact manifest. Parse it in memory without extraction to
   the host. Reject extra/missing/changed files, writable modes, hardlinks,
   symlinks, special/sparse files and unexpected directories. This check prevents
   unnoticed source files inherited from the base image.
7. Construct a separate read-only, networkless, nonroot, restricted container plan
   for each operator-defined checker. No host source bind mounts or Docker socket
   enter a container. The immutable source is in the image; only `/scratch` and
   `/tmp` may be writable.
8. Validate **offline fixtures** of trusted-daemon observations against the exact
   image, checker, command, working directory, user, isolation/resource settings
   and successful terminal exit. The result is labelled `offline-fixture-only`,
   not a `passed` claim or a Rust worker candidate.

The source archive, snapshot object, image ID and daemon observation must never
come from a model response or an untrusted client payload. They are outputs of
one trusted host workflow. Public Python objects are not cryptographic provenance;
constructing a `Snapshot` yourself does not prove its claimed Git commit. Production
code must retain these records internally from extraction through completion.

## Proposed production flow, not enabled

A future independently reviewed supervisor should:

- Authenticate the task owner, exact revision, repository/base/head and bounded
  execution grant. Resolve repository keys through trusted configuration.
- Produce the snapshot through the independent object verifier. The host must
  retain the manifest and commit binding; never trust a container to assert them.
- Inspect the pinned toolchain image through the trusted daemon. Reject inherited
  `ONBUILD` instructions and declared volumes, and verify provenance/tool versions.
- Build the generated context without network, pulls, secrets, repository build
  instructions or host mounts. Obtain the immutable image ID through the daemon's
  structured API, not free-form build logs. Verify labels, image config and source
  archive before starting any checks. Verify `/candidate`, `/scratch` and `/tmp`
  are actual directories, not symlink aliases, and audit/allowlist inherited
  environment variables so no secret or startup hook reaches the check. Associate that exact ID with the verified
  manifest in host state.
- For each allowlisted checker, create a fresh container from the same verified
  image ID, force the complete policy, and verify daemon inspection matches the
  plan **before** starting it. Keep the trusted verifier outside this PID/mount/
  user namespace. Do not place a writable verifier or success JSON channel next
  to repository tests.
- Start the check and observe state via the authenticated local daemon API. Apply
  wall-clock/output/resource limits, poll authorization and terminate on lease
  loss or cancellation. Test stdout/stderr/files are untrusted diagnostics only.
- Accept only the daemon-observed terminal exit code for the exact planned
  container, image and immutable snapshot. Return a narrow attestation such as
  “configured checker container exited 0 with snapshot digest X mounted read-only.”
- Remove containers and ephemeral images, requiring confirmed cleanup. Reconcile
  orphaned resources after crashes. Recheck job authorization immediately before
  writing a candidate record, and bind required checker names to the host record.

This solves the reproduced index/replace-ref attack class by removing repository
Git metadata from checks and keeping snapshot verification outside the check
container. Extra `git diff` commands inside a shared runner are not a substitute.

## What this can and cannot establish

An immutable snapshot plus trusted daemon observation can establish which input
was supplied and how a configured process terminated. It cannot prove arbitrary
repository tests are honest, comprehensive, or correct. A malicious test can
intentionally return zero or copy code into scratch and test something else.
Checks intended to validate behavior must be independently owned, reviewed
checker profiles, and claims must describe their actual assurance. Do not turn
an exit code into “the model completed the task correctly.”

Containers also share a host kernel. These flags are not proof against kernel or
runtime vulnerabilities. Adversarial multi-tenant code may require a separately
reviewed microVM boundary. No runtime isolation guarantee is claimed here.

## Exact blockers before real execution

1. No Docker daemon/CLI exists in this development environment. No image build,
   stopped-container export, real read-only check, cancellation, timeout, resource
   limit, output flood or cleanup test has been performed.
2. No trusted daemon client or supervisor has been implemented. The plan and
   observation fixtures do not create, start or inspect a real container.
3. Packed repositories need a bounded trusted object export/provisioning stage.
   The prototype does not shell out or fall back to reading untrusted Git config.
4. No audited pinned image/checker profiles or isolated-kernel security review is
   supplied. Crash cleanup and image provenance checks need implementation.
5. No Rust integration accepts the prototype's fixture results. The built-in
   consumer cannot reach `review_ready`, and no configured image bypasses that.

## Offline regression coverage

Run `python3 -m unittest discover -s sandbox -p 'test_*.py'`.

Tests cover both Git hash formats, object tampering, compressed-object limits,
symlink/hardlink/gitlink/path attacks, index/replace-ref independence, traversal
budgets, deterministic source contexts, read-only source archive comparison,
base-image ONBUILD/volume rejection, wrong image/writable source observations,
stdout-forged evidence rejection and unconditional runtime disablement. These are
unit/protocol tests, not a substitute for Docker smoke tests or security review.


## Integration gate verified while runtime is unavailable

The Rust consumer now checks a default-deny, sealed executor preflight before it
claims a queued job or calls an input provider. The only ready implementation is
an in-crate test fixture. The disabled Docker implementation leaves a queued job
untouched and cannot consume model/provider resources. This is a verified safety
integration, not a real execution integration.

A current environment check found neither a Docker CLI nor a local Docker socket.
No daemon client, audited image or external trusted supervisor is available here.
Implementing another fixture-only adapter cannot establish that real immutable
mounts, namespace isolation, daemon observations, revocation and cleanup work.
Consequently no runtime adapter was enabled and no “verified execution” result is
reported. The production blockers listed above remain mandatory.
