# Offline recipient drafting acceptance (#18)

`hub/tests/drafting_acceptance.rs` exercises the real Hub router, SQLite migrations
and MockRuntime in-process. It opens no HTTP listener and requires no provider,
SDK, credentials, GitLab, deployment or code execution. Generated text is explicitly
labeled `[MOCK · draft only]`; the test is not evidence of model quality or work
being performed.

The scenario crosses every human handoff:

1. Alice's chat creates a sender-private proposal; Bob sees no proposal or task.
2. Only Alice's explicit confirmation creates the task. Drafting before Bob's
   acceptance is rejected without reserving a draft receipt.
3. Bob accepts, then explicitly requests a draft. The completed private receipt
   creates no shared events, submitted deliveries, chat messages or task changes.
4. Alice and Eve cannot create/list/read Bob's draft or recovery receipt. Exact
   replay keeps one draft. Runtime mock labeling is asserted, not hidden.
5. Bob edits locally; server state stays unchanged. Alice cannot approve before
   delivery. Only Bob's explicit deliver request stores the reviewed text in a
   new submitted row; the immutable generated draft remains private.
6. Bob cannot approve his own delivery. Alice separately approves. After reopening
   the database, Bob can recover the original receipt, now stale, with no extra
   draft/submission. Alice still cannot read it.

The test also checks no worker jobs or publication storage appear, no private draft
content/ID leaks through shared task detail, and public submission numbering starts
at one independent of drafts.

Run from the repository root:

```sh
cargo test --locked --manifest-path hub/Cargo.toml --test drafting_acceptance
```

Run this together with the full Rust suite after integration of audit schema v8
and the #17 UI. The UI's separate simulated-DOM tests cover editing and confirmation
controls. This in-process test does not exercise a browser, UI event wiring, adapter
transport, real DSH tool isolation or remote execution. Live provider/browser
acceptance remains separate. No production execution gate is changed.
