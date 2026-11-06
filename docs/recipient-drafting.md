# Recipient agent drafts a delivery

Status: implementation contract for [#14](https://github.com/phil-zyx/agenct/issues/14).
This document does **not** enable an endpoint, model traffic or execution. It guides
[#11](https://github.com/phil-zyx/agenct/issues/11),
[#15](https://github.com/phil-zyx/agenct/issues/15), and
[#16](https://github.com/phil-zyx/agenct/issues/16). The local implementation and offline acceptance are described in
[the API contract](recipient-draft-api.md), [chat UI](recipient-draft-ui.md), and
[acceptance tests](drafting-acceptance.md). Optional source-draft provenance and
live-provider verification remain outside that implementation. This design alone
does not authorize deployment, provider calls or execution.

## User flow and authority

1. Alice confirms a delegation proposal to Bob. Bob explicitly accepts the task.
2. On the `working` task, Bob chooses “让我的 Agent 起草”, optionally selects one
   of his enabled roles, and explicitly starts a draft.
3. Bob alone sees the generated draft, its task/profile revision and any stale or
   uncertain warning. He can edit a local copy. Starting a draft does not deliver,
   accept, approve, publish, create another task, or change task status/revision.
4. Bob explicitly confirms the reviewed text through the existing `deliver`
   action with a fresh, reviewed task revision. Alice sees only that submitted
   text, and separately approves or returns it through existing controls.

The model is a text generator, never an actor in policy decisions. A role is fixed
prompt guidance, not a tool, shell, provider, credential or permissions selection.
The existing disabled code-worker/executor gates remain disabled. Text drafting
requires no code-worker job, execution grant, repository checkout or external write.
Changing cancellation rights is out of scope; drafting follows the current policy.

## Privacy and context

The Hub constructs one immutable task snapshot in the reservation transaction:
`task_id`, `revision`, `requester`, `owner`, `status`, `summary`, `result`, `details`,
`summary_truncated`, `result_truncated`, `details_truncated`. Reuse the selected-task context validator
and bounds (2,000 JSON-encoded bytes of summary, 12,000 of requester-shared details, 5,000 of result, 20,480 total),
with the wire field names used there (`id` for task ID). Require `working` when
reserving. A prior result is included only if it is already shared task data.
Truncation is explicit in the prompt and review UI; this is not a promise that the
model has read the entire task. Historical deliveries can be added only through a
separately reviewed bounded-context extension.

No private chat history is supplied, including **Bob's own** ordinary chat. No
Alice private messages, private proposals, other tasks, other drafts, events,
credentials or execution authority are supplied. The only additional guidance is
Bob's server-selected frozen runtime profile. Do not copy its private configuration
into shared task responses. Use a fresh request-local session UUID distinct from
Bob's ordinary chat session, empty history, and no SDK resume/persistent sessions.

Task text, role display names and model output are untrusted data, never control
instructions. Wrap task contents as structured JSON data; do not route slash
commands. Structural validation and disabled capabilities enforce authority;
prompt wording alone is not a security boundary. Model text is not evidence that
work or verification actually happened. Render it as text using the existing safe
UI path.

## #15 Hub API and durable receipt contract

Proposed new API (not a change to legacy chat):

- `POST /api/tasks/{task_id}/drafts`: JSON `request_id`, `expected_revision`,
  optional `agent_id` and `expected_profile_revision`. Reject unknown fields;
  no caller-authored owner, task contents, history, session, role text or endpoint.
- `GET /api/tasks/{task_id}/drafts/turns/{request_id}`: owner-only receipt recovery.
- `GET /api/tasks/{task_id}/drafts`: owner-only discovery of drafts and unresolved
  receipts, bounded/paginated, for refresh or same-owner reauthentication.

`request_id` is required and follows the existing chat key grammar (1..128 ASCII
letters/digits/hyphen/underscore). Revisions are nonnegative signed 64-bit integers.
Explicit role selection requires both role ID and expected profile revision;
omitting both selects the current enabled coordinator. Never silently replace an
unavailable selection. Resolve profile only from the authenticated owner.

Use a dedicated draft receipt table with unique `(owner, request_id)`. Its kind is
`draft_delivery`. Chat/proposal and drafting keys occupy **separate namespaces**:
the same textual key can independently identify one chat and one draft request.
Within drafting, reuse for another task, task revision, selected role or expected
profile revision conflicts. Clients generate a new random key for each intentional
new operation and keep kind/task/owner/key together. This avoids adapting chat
receipts by pretending draft text is a chat message or proposal.

Bind the submitted request fields, exact server snapshot, resolved runtime profile,
session UUID and resulting draft ID to the receipt. Store enough frozen metadata
for deterministic recovery, without storing secrets. Existing chat/proposal receipt
semantics and legacy optional request IDs are unchanged.

Order of operations:

1. Authenticate, enforce existing CSRF rules, validate input and owner visibility.
2. Check for an existing receipt **before** the per-owner runtime gate. An exact
   match returns its frozen result/status even after profile/task changes. A key
   conflict never dispatches. Never disclose another owner's existence or content.
3. Acquire the existing per-owner runtime gate, then recheck the receipt. Under a
   database transaction, validate owner, working state and task/profile revisions,
   freeze inputs, and reserve a durable `pending` receipt **before** any adapter call.
   Reservation failure makes zero calls. Do not hold the database lock while waiting.
4. Dispatch once. Validate the reply independently in Hub and adapter. In one
   transaction, insert the immutable private draft and finalize the receipt as
   `complete`. Do not add ordinary chat messages or shared task events.
5. Recheck task revision/status in that final transaction. If changed, still retain
   valid generated output privately, marked stale, and complete the receipt. It is
   not deliverable against the captured revision. Also compute current staleness
   when listing drafts; never rewrite the immutable generated content or snapshot.

`pending`, `complete`, `uncertain` are receipt outcomes, not delivery statuses.
Pending replay returns status only, never redispatch. Malformed output, transport
errors/timeouts, cancellation, process death, or failed completion storage after
reservation become `uncertain`, with a bounded nonsecret reason. Atomic completion
means no orphan draft can be exposed after rollback. Best-effort failure cleanup
and startup reconciliation must never resurrect dispatch; pending after a crash is
uncertain even if a call may not have started. If storage cannot record uncertainty,
leave the durable pending row blocked for recovery, not eligible for retry.

Completed replay is historical: return the same stored response/snapshot, not a
new profile or live task view. Expose fresh task state through ordinary task reads.
GET recovery is read-only and requires both matching owner and task. Nonowners see
no receipt, draft count, timing, content or private metadata. Receipt discovery
must not be bundled into the requester-visible task timeline.

Use distinct machine-readable errors only where the server knows **no receipt was
reserved**: stale task revision, unavailable task/state, stale/unavailable role,
and capacity rejection before reservation. Define and test their exact status/code
pairs with the UI. Generic 400/404/409/5xx or transport failures are not proof of
non-dispatch. A binding conflict must not be mistaken for an unstarted request.

This guarantees at-most-once **Hub dispatch per owner/key**, not exactly-once remote
execution, and not zero provider charges after failure. As with
[chat receipt recovery](chat-turn-recovery.md), this assumes the supported single
Hub process owns its durable database/lock; it is not distributed deduplication.
Database loss, backup rollback or manual receipt deletion removes that protection. A deliberate new key can
repeat unknown upstream work. The UI must explain this before a new attempt, never
retry automatically, and retain owner-scoped recovery across temporary auth expiry.
Explicit logout/account changes clear local private state. Browser timeout unlocks
recovery without claiming cancellation of remote work.

## #11 Immutable drafts and submitted deliveries

Use append-only delivery records with `id`, `task_id`, `author`, `content`,
`status` (`draft` or `submitted`), `created_at`, and captured `task_revision`.
`status` is an immutable record category, **not** a field changed from draft to
submitted. Drafts additionally link to their originating receipt and frozen
snapshot/profile. Submitted records may reference `source_draft_id`.

- Generation appends a `draft`; editing is local UI state, not overwriting that row.
- Confirmed `deliver` appends a **new** `submitted` record holding exactly the
  human-reviewed text. It may differ from the generated draft. The original remains
  private. Never promote a draft row by changing its status/content.
- The deliver transaction validates current owner/working/revision, appends the
  submitted row, updates `tasks.result`/status/revision, appends the public delivery
  event and revokes obsolete worker grants atomically. Text remains 1..8000 UTF-8
  bytes, nonblank, matching current deliver validation.
- An optional `source_draft_id` extension must verify owner, task, completed draft
  and matching captured revision in that same transaction. It is provenance, not
  authority; clients cannot label another task/person's draft as their source.
  Existing manual deliver callers without this field remain supported. A stale
  draft requires a new review/generation at current revision, not automatic rebasing.
- Return/approve/cancel never deletes or edits previous delivery records. Migrated
  nonempty `tasks.result` becomes a submitted historical record, never a draft.
  Migration cannot recover results already erased by the old return implementation.

Use a submitted-only sequence per task for public delivery ordering; private draft
IDs/sequence gaps/counts must not reveal draft activity. Public pagination cursors
and page counts derive only from submitted ordering, never a mixed draft/submission
row ID or offset. If one table is used, all
requester-facing queries explicitly filter `status='submitted'`. Private generation
must not appear in public events, timestamps or task revision increments. Public
responses must omit private provenance identifiers even when a delivery came from
a draft. #12 replay and #13 timeline consume submitted records/public transitions
only; owner-private draft receipts have their own audit/recovery path.

## #16 Adapter protocol and Runtime contract

Add the discriminant `kind: "chat" | "draft_delivery"` to the shared validated
turn input. Omitted kind means legacy chat; unknown kinds fail closed. Existing
chat parsing/proposal checks remain unchanged. Draft turns require a valid working
task snapshot, the isolated session UUID, empty history and frozen runtime profile.
Use a fixed server-authored draft instruction as `message`, not freeform client
chat; task contents occur only in the validated task-context object. Reject a draft
payload with nonempty history or missing/invalid task context.

Retain the response envelope `reply`, `proposal`, `backend`. For draft delivery,
`reply` must be nonblank and at most 8,000 UTF-8 bytes; `proposal` must be explicitly
null. Reject missing/extra/duplicate fields, wrong types, invalid backend or any
proposal rather than silently dropping it. Hub independently performs this
kind-specific validation before persistence. MockRuntime uses the same input/output
validation and creates deterministic text without a provider or execution call.

Drafting never registers tools, sends notifications, writes files to a user project,
resumes an SDK session or creates a worker job. Existing runtime limits, internal
adapter authentication, no-tools configuration, payload bounds and timeout cleanup
remain in force. No automatic model fallback or retry is introduced.

## Acceptance gates before claiming implementation

Implement and run these regressions as the dependent issues land:

1. Identity/state matrix: owner working succeeds; requester/stranger, unaccepted,
   returned, ready, completed/canceled, stale revision and stale/disabled role make
   zero adapter calls. Wrong-owner reads disclose no private draft metadata.
2. Context capture: serialized adapter input has no seeded Alice/Bob private chat,
   prior drafts/proposals or other tasks. Verify byte bounds, truncation flags,
   immutable task/profile snapshots, malicious text and a distinct session UUID.
3. Concurrency/replay: duplicate simultaneous keys dispatch once; binding changes
   conflict; cross-task keys conflict; chat/draft namespace independence is explicit.
   Completed replay works while another turn holds the gate and after profile edits.
4. Failure boundaries: reserve-write failure makes zero calls; actual timeout,
   handler cancellation, malformed/proposal-bearing output, atomic completion
   failure, and restart leave no public result and never redispatch the same key.
   Test both tables and receipt state, not just HTTP status.
5. Concurrent task change during generation produces a private stale draft, no
   status mutation. Subsequent task changes also invalidate delivery. A stale source
   draft ID cannot be used to bypass fresh task review.
6. Submission: reviewed edits are stored in a new submitted row; original draft
   remains immutable/private. Three deliveries/two returns preserve all submissions.
   Submission rollback leaves no partial task/result/event/delivery update. Ordinary
   manual deliver and requester approval/return continue to work.
7. Privacy: requester list/detail/events/timeline/receipt APIs and errors reveal no
   drafts or counts, including after return/cancel. Public delivery numbering and
   provenance do not expose private draft activity. Owner recovery is authenticated.
8. UI: request binding survives refresh/same-owner reauthentication, never another
   owner; hung POST releases controls after timeout; known pre-dispatch rejection
   preserves local edits, while generic errors remain uncertain. No automatic
   submit, retry, role substitution or task-revision adoption.
9. Offline end-to-end (#18): Alice proposes/confirms, Bob accepts/drafts/edits and
   explicitly delivers, Alice approves. Assert no delivery exists before confirmation
   and no worker execution/publication call occurs throughout.

Offline mocks establish contract behavior only. Live DSH correctness, effective
tool isolation, provider data handling, deployment and real browser acceptance
remain separate prerequisites; this design does not claim any of them verified.
