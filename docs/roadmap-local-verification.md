# Local roadmap implementation and verification

This inventory describes the integrated source, not publication, deployment or
issue closure. GitHub issue state and the default branch may still differ. All
model behavior below is verified with offline mocks or protocol fixtures only.

| Issue | Implemented scope | Main evidence |
| --- | --- | --- |
| #4 | Tasks require explicit sender-private proposal confirmation; direct creation removed | `api.rs`, browser auth/client regressions |
| #5 | Message, proposal and task domain modules | `messages.rs`, `proposals.rs`, `tasks.rs` |
| #6 | Atomic versioned SQLite upgrades and conservative schema validation | `schema.rs`, migration/rollback tests |
| #7 | Runtime trait, mock/DSH shared proposal validation | `runtime.rs`, invalid-mock regression |
| #8 | Central data-defined task/proposal/worker policy | `policy.rs`, HTTP negative matrices |
| #9 | Approved per-stage cancellation permissions and required shared reason | `cancellation.rs`, architecture/policy contract |
| #10 | Configured participant Directory separate from runtime gates | `directory.rs`, directory regressions |
| #11 | Immutable private drafts and submitted delivery ledger | ledger/migration tests; three submissions/two returns |
| #12 | Replayable public lifecycle audit, foreign keys and legacy provenance | `task_audit.rs`, schema v8/v9 regression tests |
| #13 | Participant-filtered timeline API and task-card view | timeline model/UI/client and real API tests |
| #14 | Reviewed owner-private drafting contract | `recipient-drafting.md` |
| #15 | Owner-only draft API, immutable receipts, stale recovery | `drafts.rs`, timeout/restart/concurrency tests |
| #16 | Strict isolated draft turn protocol, no proposals | adapter tests and Hub validation |
| #17 | Optional role, private editing, explicit manual delivery, uncertainty recovery | draft UI and integrated client tests |
| #18 | Mock Alice-to-Bob reviewed drafting/acceptance journey | `drafting_acceptance.rs` |

#19 remains an on-demand tracker, not completed work. Database pooling/blocking
isolation, PostgreSQL migration, SSE, history summarization/retention and real
browser acceptance are not established here. UI modules were extracted where
needed, but this does not close the broader frontend/browser item. The previous
Chromium restriction has not been bypassed.

The #14 design's optional `source_draft_id` submission extension is not exposed.
The existing explicit manual-delivery action submits exactly the human-reviewed
text as a new immutable record, without promoting the original private draft.
Generated text is not evidence of executed work. Legacy overwritten deliveries
cannot be recreated; migrated timestamps and incomplete event history are labeled
without inventing past facts. Supported dispatch deduplication still assumes one
Hub process and preserved durable database state.

No live provider credentials/calls, real DSH runtime boot/tool isolation, actual
WSL operation, real-browser acceptance, deployment, autonomous code execution,
external publication or automatic approval is established by these tests.
Production worker execution remains disabled. Safe source integration is separate
from any later authorization to publish, merge, connect a model or deploy.
