# Discuss a selected shared task

The Rust chat client can attach a read-only snapshot of **one explicitly selected
shared task** to the next ordinary chat turn. Select “讨论此任务” on a task card,
review the selection, and then send your question. Selecting or clearing a task
makes no model call or task mutation. No task is included by default.

This connects the existing manual collaboration workflow to chat context; it does
not execute work, send another person's private conversation, confirm a proposal,
change task status, or authorize publication. Existing accept/deliver/approve/return
and cancellation controls keep their separate identity and revision checks.

## Server contract

`POST /api/messages` accepts optional `selected_task` with exactly `task_id` and
`expected_revision`. It accepts no client-authored task summary/result or owner.
The authenticated caller must be the task's requester or owner. Unknown/inaccessible
tasks return 404; stale revisions return 409 before invoking the adapter. Negative
revisions or invalid IDs fail validation, and extra fields fail schema validation.
Known stale/unavailable selection rejections carry the exact machine-readable codes
`selected_task_stale_before_dispatch` (409) or
`selected_task_unavailable_before_dispatch` (404). Only those checks certify that
no receipt was reserved and no provider call started. Generic 409/404 errors do not
certify that; receipt recovery must remain conservative for them.

Under the same task-mutation mutex as receipt reservation, the Hub copies:

- Task ID, revision, requester, owner and status.
- A bounded excerpt of the shared summary and, when present, the requester's shared
  material (`details`, see below) and the delivered result.
- Explicit `summary_truncated`, `result_truncated` and `details_truncated` flags.

Only this snapshot is sent to the configured adapter. No event history, other task,
private proposal, credential, execution grant or another participant's private chat
is included. The caller's own existing private chat history remains part of the
turn as before. Snapshot strings are data, not instructions or authorization.

Summary excerpts are capped at 2,000 JSON-encoded bytes, details at 12,000 and result
excerpts at 5,000, preserving Unicode characters. The full snapshot is capped at 20,480 compact
UTF-8 JSON bytes. Escaped control characters count toward these limits. Empty or
partial excerpts are possible for unusual input; the truncation flags must not be
interpreted as a complete delivery. Existing total adapter payload bounds remain.

The successful reply includes `task_context` containing the exact snapshot used,
or null for a turn without a selection. This is historical context, not a live task
view. A concurrent change after capture does not rewrite an in-flight prompt.
Current status and action controls must still come from fresh task reads.

## Receipt and UI behavior

The selected task ID and expected revision are bound into owner-scoped durable
turn receipts alongside message and agent selection. Changing or omitting a prior
selection under the same key returns 409. Identical completed replay returns the
original response and snapshot even if the task has since changed. Older receipts
migrate with no task selection; their previous deduplication binding is preserved.
API calls without a request ID remain legacy/non-deduplicated.

The UI marks a changed unsent selection stale and requires reselecting or clearing
it. It does not silently adopt a new task revision or silently send without the
selected context. A known pre-dispatch rejection preserves the draft and restores
an invalidated selection for explicit reselection or clearing, without leaving an
unresolvable pending receipt. Logout/account changes clear selection. In-flight selection is
frozen, and existing uncertain-turn recovery never resends automatically. A new
attempt still requires an explicit send. Selected task contents are not added to
browser recovery storage.

## Adapter and verification boundaries

The DSH adapter strictly validates the optional snapshot schema, enums, identifiers,
types and serialized size. It places the snapshot in quoted JSON conversation data
and instructs the model to treat task text as untrusted, read-only, possibly stale
information. Delegation proposals must still quote the **current user message**;
context/history cannot supply a recipient or shared summary. The Hub independently
checks that rule, so a model response derived only from task text cannot create a
proposal. No tools, persistent sessions, executor or publication path is enabled.

Offline fake-adapter/SDK tests cover participant isolation, forged and stale input,
snapshot/replay stability, prior-receipt migration, Unicode/escaped-text bounds,
malicious task text and unchanged proposal boundaries. Simulated-DOM tests cover
selection and stale/identity/recovery flows. These tests do not establish live
model accuracy, prompt-injection resistance in general, real DSH tool isolation,
real-browser acceptance or production deployment. A model answer is not verified
execution evidence and never changes the authoritative task record.

## Shared material (`details`)

When confirming a proposal, the requester may attach up to 12,000 bytes of material
the recipient needs (for example the document under review). It is sent as
`details` in `POST /api/proposals/{id}/actions` with `"action":"confirm"` only, is
stored once and immutably in `task_details`, and is visible only to the two
participants (`GET /api/tasks`, `GET /api/tasks/{id}`). It is never derived from
model output or private chat. A confirm replay must restate identical details, or
the Hub returns 409. Task discussion and recipient drafting receive it inside the
same bounded snapshot; it remains untrusted quoted data, not instructions.
