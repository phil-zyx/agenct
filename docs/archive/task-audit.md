> 历史专题记录：保留设计背景、协议细节和当时的验收结果；其中“当前/尚未”描述可能早于最新实现。现行用法以[入门](../getting-started.md)和[参考](../reference.md)为准。

# Participant-visible task audit

`GET /api/tasks/{id}` keeps `task`, `events`, and `deliveries`. The existing
participant authorization applies to the entire response. Only the requester and
recipient can read a task; private proposals and private delivery drafts are not
part of this audit. No model, provider, worker execution, or publication is enabled.

## Additive event contract

Existing `actor`, `action`, and `at` fields remain. Each event also exposes:

- `seq`: a dense, one-based sequence within this task's visible lifecycle events.
  It is not the database event ID or a global cursor.
- `from_status`, `to_status`, `revision`: recorded transition and resulting task
  revision. Creation starts at revision zero with a null source status.
- `delivery_seq`: the submitted delivery sequence stored when a `deliver` event
  is written, otherwise null. Reads use this stored link, not a revision-based join. Resolve this against the response's public `deliveries`.
  It never refers to a private draft ID or private draft sequence.
- `provenance`: `recorded` for newly captured revisions, `legacy` for earlier rows.
- `reason`: present only for a cancellation with an explicitly shared reason.
  Migration v9 leaves historical reasons unknown; no guessed text is backfilled.
  New cancellations require a trimmed reason of 1–500 Unicode characters.
  Other event types never expose this field.

`audit.schema_version` is 1. `audit.legacy_history_incomplete` is true when the
visible events do not cover revisions from zero in order. It is false only for a
continuous newly recorded lifecycle. `audit.legacy_notice` explains the migration
boundary when history is incomplete, and is otherwise null.

The server filters event actions to `created`, `accept`, `deliver`, `approve`,
`return`, and `cancel`, and actors to the task's participants **before** assigning
public sequence numbers. Private drafts, unsupported actions, unrelated tasks and
unrelated actors therefore introduce neither visible rows nor sequence gaps.
The response never serializes arbitrary event metadata, raw IDs, proposal text,
private draft content or draft references.

## Atomicity and replay limits

Task changes and their event record commit in the same transaction. A rejected
revision, unauthorized action or transaction failure records no transition. A
repeated proposal confirmation returns its existing task rather than another
creation event. Reads hold the Hub database lock for a coherent snapshot. There is
no stream, cursor API or live push subscription in this change.

Schema migration v6 introduced nullable transition columns and append-only guards.
Migration v8 rebuilds the event table transactionally with an actual `task_id`
foreign key to `tasks(id)`, canonical `revision`, and stored `delivery_seq`. The
older `task_revision` column remains for schema compatibility and new writes set
both revision columns identically. Original IDs, timestamps, indexes and triggers
are preserved. Views and triggers attached to other tables remain untouched;
static table recreation avoids a rename while their target is absent. Their
bodies are not executed during the upgrade. Orphan events cause upgrade failure,
never silent removal.

Custom tables with incoming foreign keys to `events` are not automatically
migrated. The upgrade detects these across the whole main schema and fails before
any table rebuild, whether FK enforcement is enabled or disabled. This prevents
`DROP TABLE` from cascading into unrelated rows (or setting their references to
null). These databases need an explicit coordinated dependent-table migration;
do not disable foreign keys to bypass this guard.

For pre-v8 recorded deliver events, migration reconciles a link only when exactly
one immutable submitted row matches task, revision and author; missing/ambiguous
matches fail the upgrade. Legacy null revisions remain null and unlinked. This
reconciliation is not a claim that v6 originally stored a link. No migration
guesses historical revisions or previous statuses. Old
rows retain their actor/action/time, with null transition fields and `legacy`
provenance. Delivery migration v5 preserves only the last result still present;
previous overwritten results cannot be recovered. Even a known final status is
not evidence that the earlier event stream or prior delivery texts can be rebuilt.

For newly recorded tasks, consumers can replay the lifecycle transitions and link
submitted delivery versions. This is not reconstruction of model reasoning,
private conversations, worker activity, external side effects or deleted data.
SQLite append-only triggers protect normal application writes, not an operator
with filesystem/database administrator access. Production backups, retention,
cryptographic attestation and multi-process operation remain separate work.

## Verification

Offline tests cover filtered dense sequences, cross-task/actor isolation, private
versus submitted delivery references, legacy null provenance, transaction
rollback, migration/reopen behavior, lifecycle transitions, stale retries and
participant authorization. These are API/storage tests, not a live browser,
provider, sandbox or publication acceptance test.
