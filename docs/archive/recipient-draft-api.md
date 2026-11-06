> 历史专题记录：保留设计背景、协议细节和当时的验收结果；其中“当前/尚未”描述可能早于最新实现。现行用法以[入门](../getting-started.md)和[参考](../reference.md)为准。

# Owner-private recipient draft API (#15)

This implements text drafting, not execution, automatic delivery or acceptance.
It builds on the private drafting design (#14), immutable delivery ledger (#11),
runtime boundary (#7), and isolated adapter protocol (#16). Schema version 7 adds
`draft_turns`. It does not extend the shared task audit or expose private drafts.
The UI is a separate task; no drafting controls are enabled by this API change.

## Endpoints

- POST `/api/tasks/{task_id}/drafts`: `{request_id, expected_revision}` and optional
  paired `agent_id`, `expected_profile_revision`. Unknown fields are rejected.
  Only the authenticated task owner can reserve a draft, while the task is
  `working` at the supplied revision. Role selection is from that owner's profile.
- GET `/api/tasks/{task_id}/drafts/turns/{request_id}`: owner-only recovery.
- GET `/api/tasks/{task_id}/drafts?limit=20&after=...`: owner-only discovery, maximum
  50 receipts per page. `next_cursor` is the last returned request key when more
  rows exist. Ordering is lexicographic request ID, not generation time. Neither
  this cursor nor any draft metadata appears in participant-visible task APIs.

Responses contain `kind: "draft_delivery"`, task/key binding, `status` and nullable
`response`. A completed response includes generated draft ID/content/time, frozen
task context, runtime profile and backend. The saved `draft.stale` reflects task
state at completion. Live **top-level `current_stale`** is recalculated on POST
replay, GET recovery and list; clients must consult it before presenting any next
step. The nested saved response is immutable and is never silently rebased.

Only exact matches replay. Owner/key uniqueness is independent of chat/proposal
keys. Another task/revision/role binding conflicts without a dispatch or an
unstarted-operation code. Reads and exact replay are allowed after task/profile
changes, including when another turn holds the owner's runtime gate.

Known pre-reservation errors use HTTP 409 and these codes:
`draft_task_stale_before_reservation`, `draft_task_state_before_reservation`,
`draft_role_unavailable_before_reservation`, `draft_busy_before_reservation`.
Generic errors, binding conflicts and lost responses do not prove non-dispatch.
Requester/stranger discovery, recovery and creation return generic 404, as for a
missing task, without exposing counts, private content or timing metadata.

## Persistence and isolation

The durable reservation binds exact input, task snapshot, runtime profile and a
fresh UUID before a model call. It uses the same per-owner runtime gate as chat.
Only fixed drafting instructions, empty history, frozen profile and the bounded
shared task snapshot reach the adapter. No ordinary chat session or prior private
messages, proposals, other drafts or events enter the payload. The adapter's
strict no-tools draft mode rejects proposal-bearing output; Hub independently
checks the backend, explicit null proposal and 1..8000 UTF-8 byte reply.

Successful finalization inserts a new immutable `draft` delivery and completes
its receipt in one transaction. Task state changes during generation retain the
valid output privately as stale. No shared events, task revisions, chat messages
or submitted deliveries are created. Owner drafts and submitted deliveries use
independent sequences. Manual `deliver` remains a distinct explicit operation
that stores the exact reviewed text; there is no source-draft submission shortcut.

Timeouts, malformed output and failed completion writes leave uncertain outcomes.
A cancellation guard durably marks its reserved key uncertain without inferring
status from another call. If this cleanup write fails, the receipt stays pending
and blocked until recovery. Startup durably converts pending
receipts to uncertain. No reserved key can redispatch. GET never changes storage.
A new intentional key can duplicate remote work after an unknown result; this is
at-most-once Hub dispatch, not provider exactly-once execution or guaranteed
cancellation. Supported single-process DB locking and durable database retention
remain prerequisites. Backup rollback/manual deletion can remove guarantees.

Receipt triggers prevent replacing/deleting a reservation, changing bindings, or
rewriting terminal outcomes. Migration validation checks the exact receipt schema
and trigger definitions before recovery. Private generation does not change the
public timeline, submitted numbering, publication state or worker authority.

## Verification

Offline API tests cover owner/state/role/input checks; scoped context and fresh
sessions; same-key concurrency, binding conflicts and chat namespace independence;
private stale completion and live stale reads; malformed replies; cancellation and
restart; reserve/finalize failures; immutable receipt protection; pagination;
manual submission isolation; and completed replay during another running turn.
A unit fixture shortens the HTTP timeout and verifies actual timeout uncertainty.
These tests do not establish live model quality, deployed provider handling, or
browser acceptance. No provider traffic is required by the test suite.
