> 历史专题记录：保留设计背景、协议细节和当时的验收结果；其中“当前/尚未”描述可能早于最新实现。现行用法以[入门](../getting-started.md)和[参考](../reference.md)为准。

# Hub SQLite schema upgrades

`Hub::open` owns schema upgrades through `hub/src/schema.rs` and SQLite's
`PRAGMA user_version`. Back up an existing database consistently before upgrading;
do not copy a live database without its WAL. There is no automatic downgrade.

Versions are ordered:

1. Adopt/create the baseline Hub, personal-profile, chat-receipt and worker tables.
2. Add task revisions, defaulting existing rows to zero.
3. Add selected-task, task-context and profile-revision receipt bindings. Existing
   bindings remain unchanged; missing bindings become NULL.
4. Add worker execution authority, defaulting unclassified jobs to zero. An upgrade
   does not authorize or start worker execution.
5. Add the append-only delivery ledger. Existing non-NULL `tasks.result` becomes
   submitted sequence 1, authored by the task owner at its current revision.
   `created_at` for backfilled rows is the migration time, not an inferred delivery
   time. Already overwritten legacy versions cannot be recovered.
6. Add nullable lifecycle transition fields and append-only guards to events.
   Legacy transition fields remain unknown; they are not backfilled by inference.
7. Add owner-private durable draft-generation receipts.
8. Add canonical event revision and stored delivery links, rebuilding events with
   a task foreign key. Refuse orphan events or ambiguous recorded delivery links.
9. Add nullable `events.reason`. Existing rows remain NULL, including old
   cancellations; new user-confirmed cancellation writes its shared reason
   atomically with the task transition and worker revocation.
10. Add `task_details`: optional requester-written shared material attached when a
   proposal is confirmed. One immutable row per task (insert/replace, update and
   delete are blocked by triggers). Existing tasks have none; nothing is inferred.
11. Add nullable `created_at` to `messages` and `proposals` (UTC `YYYY-MM-DD HH:MM:SS`,
   written by the Hub on insert; SQLite cannot add a column with a timestamp
   default). Existing rows stay NULL and are shown as "earlier".

All pending steps, including their version markers, run in one immediate
transaction. Any migration or validation error rolls the whole upgrade back, so a
retry starts at the original version. A newer unsupported version is rejected
before changing journal mode, schema, messages, receipts or configured sessions.
Opening a file-backed Hub can still create its normal lock sidecar.

Version zero supports empty databases and subsets of the historical Hub tables,
including historical combinations of the earlier additive columns. Present tables must
have the historical base columns, declared types, nullability, defaults, primary
keys and unique keys. Old receipt tables without the status CHECK are supported
and preserved. The named active-worker index is created if absent on version zero;
an existing index with a different definition is rejected. Missing tables/columns
at an already-recorded version, unknown columns, nonhistorical constraints,
collations and generated columns are rejected rather than silently stamped as
current. This is a structural compatibility check, not a verifier for hostile
SQLite files. Ordinary nonunique indexes and triggers are left intact, and other
subsystems' tables are not adopted or rewritten.

After successful migration, a separate transaction performs recovery on **every**
open, including an already-current database. Pending messages become interrupted;
messages attached to pending chat receipts and those receipts become uncertain.
Recovery failure rolls back both sides together. It never redispatches a turn.

The public `worker::migrate` API remains available for standalone library users
and does not set a Hub schema version. Publication storage retains its independent,
opt-in migration and is not enabled by opening a Hub.

## Verification

Offline Rust storage regressions cover all legacy additive-column combinations,
each migration failure boundary and retry, prior versions, incompatible layouts,
constraints, preserved data, atomic recovery, repeated Hub opens and future-version
refusal without changing database bytes. They do not start listeners or providers.

```sh
cargo test --locked --manifest-path hub/Cargo.toml --lib schema::tests
cargo test --locked --manifest-path hub/Cargo.toml --test schema
```

Aggregate checks below also exercise the existing integration suites, some of
which start local fixture HTTP listeners. Run those only where permitted.

```sh
cargo test --locked --manifest-path hub/Cargo.toml
cargo fmt --manifest-path hub/Cargo.toml -- --check
cargo clippy --locked --manifest-path hub/Cargo.toml --all-targets -- -D warnings
```

## Delivery ledger

A successful explicit owner `deliver` appends one submitted row in the same
transaction as the task transition, audit event and worker revocation. Returning
work still clears the compatibility `tasks.result` field, but never deletes any
ledger version. Stale or unauthorized actions append nothing. Update/delete triggers and a duplicate-identity insert guard enforce immutable
rows even for SQLite `INSERT OR REPLACE`; validation checks those trigger definitions.

`GET /api/tasks/{id}` includes `deliveries`, ordered by submitted sequence, for
participants only. Its allowlist is `seq`, `author`, `content`, `created_at` and
`task_revision`. Draft IDs, source links, contents and counts are not exposed.
Draft and submitted sequences have separate uniqueness scopes, so private drafts
cannot create observable gaps. `status=draft` and nullable `source_draft_id` are
reserved storage for later owner-only drafting. This change introduces no draft
creation endpoint, provider dispatch, execution authorization or publication.
Future explicit draft submission must append a distinct submitted row and leave
the original draft intact; requester-facing projections must stay submitted-only.

### Version 6: participant-visible lifecycle audit

Adds nullable `events.from_status`, `events.to_status`, and
`events.task_revision`. Historical values stay null; old transitions and delivery
links are not guessed. New writes capture each task transition in its transaction.
The task-detail API exposes a dense, filtered, task-local sequence rather than the
raw global event ID. Only submitted delivery versions can be linked.

The migration installs and validates `events_no_update`, `events_no_delete`, and
`events_no_replace`. The insert guard rejects duplicate primary keys even under
SQLite `INSERT OR REPLACE` with recursive triggers disabled. These guards prevent
ordinary writes from rewriting history; they are not protection against a database
administrator. See [audit contract and replay limits](task-audit.md).


### Version 8: stored audit links and task referential integrity

The transactional rebuild preserves event IDs, timestamps, original columns,
indexes and triggers. It adds `revision` and `delivery_seq`, plus an actual
`task_id REFERENCES tasks(id) ON DELETE RESTRICT` foreign key. `task_revision`
remains for compatibility; new writers populate both revision columns equally.
Reads use the stored delivery sequence. The event writer captures only an exact
submitted delivery matching task, resulting revision and author, in the same
transaction as the delivery and task transition.

For old v6/v7 recorded deliveries, migration requires exactly one matching
submitted row before storing its sequence. Old null-revision events stay null and
unlinked. Missing/ambiguous recorded links or orphan task references fail the
entire upgrade without deleting or rewriting old data. An operator must reconcile
the inconsistency before retrying; the server never repairs it by guessing. This
rebuild does not recover earlier overwritten delivery content.


Event-table rebuilds preserve views and cross-table triggers in place without
rewriting or executing their bodies. The final table is recreated directly under
its original name using owned DDL, rather than renamed while references are
unresolved. Incoming foreign keys from any other main-schema table are an explicit
unsupported automatic-upgrade boundary: preflight rejects them before any DROP,
even when foreign-key enforcement is off. This prevents cascaded deletion or
nulling of unrelated records under FK-enabled SQLite builds. Operators must plan
a coordinated migration for those dependent tables instead of bypassing checks.
