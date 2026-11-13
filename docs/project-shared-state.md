# Explicit shared project state

Project membership and the chat-side project panel record explicitly published
coordination items: goals, assignments, blockers, and pending decisions. A member
can prepare a private candidate from one chosen chat message, then review and
publish it. There is no automatic chat extraction, group chat, agent-to-agent
negotiation, task execution, or automatic fact checking.

## Publication and confirmation

A member manually supplies the text, source type, and source note to share with
all accepted project members, including future joiners. Publishing never reads a
private conversation, task, draft, attachment, or model result on the server.
The browser can copy only a message the user explicitly selects into a local
editor; this is not publication. Source notes are
plain, unverified statements supplied by the publisher, not resolved links or
proof that another person said something. The server records the authenticated
publisher, each actor, timestamp, and immutable version history. Unknown fields
such as private message IDs, `confirmed_by`, or client-provided status are rejected.

Every new or revised item starts `proposed`, including human statements and model
suggestions. Nothing is inferred from the existing project's goal text. An
explicit confirmation against both the current item version and current project
revision records one named person's assertion:

- Goal and decision: only the project creator can confirm.
- Assignment: only the named, currently active assignee can confirm. Creating an
  assignment does not assign an executable task or authorize an agent to act.
- Blocker: only the original publisher can confirm their report.

This is not a vote, team consensus, or verified truth. Confirming a model
suggestion preserves its source type as `model_suggestion`; it never relabels it
as human evidence. A consumer must show the source, status, and confirmer together.
For decisions, `proposed` means pending, not an adopted decision. The status does
not describe task progress, blocker resolution, or completion.

The publisher or project creator may revise an item, retaining its kind. Every
revision clears the confirmation and requires a fresh, authorized confirmation.
The publisher, creator, or authorized confirmer may withdraw the item. Withdrawal
is terminal: a new item is needed to propose the work again. All actions append
history rather than overwriting it. Historical confirmations are retained as
history only, and have `confirmation_valid: false`.

## Membership and privacy

Every read and write uses the foundation's accepted-member, active-account,
creator-availability, and tenant checks. Invitations alone grant no state access.
Items cannot be read or acted on through another project's URL. Unauthorized and
unknown objects both return 404. Archived projects stay readable but cannot be
changed. Existing membership departure/removal remains available after archive.

Leaving or being removed invalidates that member's current confirmations in the
same transaction, appending `membership_revoked` versions. Rejoining does not
restore those confirmations. The explicit publication and its history remain
project material after the publisher leaves. A disabled account's confirmations
are flagged `confirmation_valid: false` while disabled; as with foundation
membership, reactivation restores eligibility unless that membership was removed.
Creator unavailability closes project content access; leaving still clears the
member's confirmations without exposing content.

No private task permission is widened. The schema holds only explicit project
text, identity references, and its own provenance. No messages, model prompts,
worker jobs, contacts, draft contents/IDs, or task artifacts are copied into it.

## API

All routes use the existing authenticated identity. Request JSON rejects unknown
fields. Content is nested so strict field validation also applies to provenance.

`GET /api/projects/{id}/state`

Returns `{project_revision, items}` with each item's latest version, including
withdrawn items. It does not return history implicitly.

`POST /api/projects/{id}/state`

Body: `{request_id, expected_revision, content: {kind, content, assignee,
source_kind, source_note}}`.

- `kind`: `goal`, `assignment`, `blocker`, or `decision`.
- `content`: nonblank, at most 4,000 characters.
- `assignee`: active project member for assignments; null/omitted otherwise.
- `source_kind`: `human_statement` or `model_suggestion`.
- `source_note`: nonblank, at most 1,000 characters. Explicitly shared plain text.
- `request_id`: 1–128 ASCII letters/digits, underscores, or hyphens. Scoped to the
  authenticated publisher and project. Exact creation replay returns the latest
  item without resetting it; reuse with different original content returns 409.
  An authorized publisher can also reconcile the original request after archive
  or assignee departure: this replay is read-only and never restores consent.

`GET /api/projects/{id}/state/{item}`

Returns `{project_revision, item, versions}`. Versions are ordered oldest first.

`POST /api/projects/{id}/state/{item}/actions`

Body: `{action, expected_revision, expected_version, content?}`. `action` is
`revise`, `confirm`, or `withdraw`. Only `revise` takes a complete content object.
Confirming/withdrawing cannot change the content as a side effect. All writes
return `{project_revision, item}`. Versions start at one and every successful
action advances both the item version and project revision atomically.

Item fields: `id`, `project_id`, `created_by`, `version`, `kind`, `content`,
`assignee`, `source_kind`, `source_note`, `status`, `changed_by`, `confirmed_by`,
`change`, `created_at`, `confirmation_valid`. `change` records `create`, `revise`,
`confirm`, `withdraw`, or `membership_revoked`. `confirmation_valid` reflects
current membership/account eligibility, not independent verification.

Stale project/item versions return 409 without writing. Callers must read the
latest version and have the person review it, never silently retry a confirmation
against a newer version. After an uncertain response, reconcile through reads;
a repeated action cannot apply to the old version twice.

## Migration and verification

Schema v19 adds empty `project_state_items` and `project_state_versions` tables
and immutable identity/history triggers. Nothing is backfilled from project
fields or private history. Existing project data and private task ACLs are
unchanged. Standard migration rollback, version rejection, and schema checks
include both tables and every trigger. Keep a normal database backup before an
upgrade. Older binaries reject newer schemas.

`project_state_tests.rs` exercises real HTTP routes with three members plus an
outsider: explicit source/status, no private copies or jobs, role-specific
confirmation, revision invalidation, replay/concurrency, cross-project URLs,
withdrawal, removal/rejoin, invitations, account/tenant controls, archive, forged
fields, and immutable history. `schema.rs` tests additive/idempotent v18 upgrade
and trigger verification. The complete Rust/test/clippy/fmt and existing browser,
private-staging, and Docker CI matrix remains the merge gate.
`project.browser.test.cjs` covers the panel with three real Hub clients.
`project-chat.browser.test.cjs` covers explicit message candidates and interrupted
publication; it uses one labeled assistant display fixture because Mock notices
are not eligible candidates. No production deployment is implied.


## From private chat to a project candidate

On a completed private user message or assistant reply, choose **提为项目候选**.
Failed/pending messages and system/Mock notices have no candidate action. This
copies only that message into a temporary browser editor. It does not send a
model request, store a new suggestion on the server, or share anything yet.

1. Explicitly select **发布到项目**, even if another project was just viewed.
   The picker has no default selection. It loads a fresh accepted-member view
   and current project revision before preparing the candidate.
2. Choose the goal/assignment/blocker/decision type and edit **内容** down to
   exactly what should be public. Assignments require an active named member.
   The original private message is unchanged. Nothing else from the conversation
   or task/draft context is copied.
3. The source type stays `human_statement` for the user's message or
   `model_suggestion` for an assistant reply. The source note records the
   selecting owner, role, and timestamp when available. These are publisher
   annotations, not verified evidence; no private message IDs or resolvable links
   are sent. The candidate source fields cannot be silently relabeled.
4. **预览发布** shows the selected project name and ID, its reviewed revision,
   new item version 1, publisher, exact shared text/source, assignee, and audience
   (all accepted members, including future joiners). Check the review box, then
   **确认发布给项目成员**. Only this step calls the existing state creation API.
   It creates `proposed`, never a confirmation or task execution.
5. The named person can separately review and confirm the published version.
   The publisher can use **撤回** after a mistaken publication. Withdrawal keeps
   history; it does not erase material already read by members.

Back invalidates the old preview and requires a new review. Closing retains the
local candidate but clears its review checkbox. A hidden or detached confirmation
cannot publish. Cancel discards the local candidate. Refresh/logout clears it;
it is not saved in local/session storage. A pending write cannot be cancelled,
and a result-unknown cancellation warns that it cannot undo an existing write.

Project/member changes invalidate the preview, and the API checks membership
and the reviewed revision again at write time. No newer revision is substituted
automatically. An uncertain creation response retains the exact `request_id` and
body for an explicit replay; repeated clicks cannot create multiple items.
Changing accounts destroys candidates and ignores old in-flight reads/callbacks.

## Read-only alignment suggestions

In an active project, **检查对齐** reads the project and current shared-state list
again through the existing accepted-member APIs. Both responses must have the
same project revision. It then runs a deterministic browser-side rule check,
not an AI/model call, and shows **对齐建议** for that snapshot only:

- No non-withdrawn goal or assignment: consider whether the project needs an
  explicitly published goal or named assignment. An absent record is not
  evidence that the actual work has no goal or owner.
- Proposed items: review their individual sources and let the named person
  decide whether to confirm. Multiple proposals do not establish disagreement,
  agreement, truth, or team consensus.
- Confirmed items whose confirmation is no longer valid: check current members
  and versions rather than relying on the old assertion.
- Published blockers: check with their reporters whether further work is needed.
  Publication/confirmation status does not indicate whether a blocker is resolved.

The rules never parse item text, source notes, private chat, task bodies or drafts.
They do not find semantic contradictions or invent missing facts. A source list
identifies each supporting shared item's ID, exact version, type, publication
source, publisher and confirmation status. **查看共享条目** focuses that same item
and its full provenance in the existing shared-state panel. A missing-record
suggestion cites the complete current shared-state snapshot and its revision.
Source notes remain publisher annotations, not independently verified evidence.

Checking and viewing sources issue no writes, confirmations, model turns,
notifications or external messages. There is no one-click acceptance of a
suggestion. Any change still uses the separate explicit publication/confirmation
flow. A check with no findings does not establish consensus or completion.

Suggestions are held only in browser memory. Closing the panel, navigating,
refreshing, starting an editor or switching accounts clears them. Any changed
snapshot, including changed membership or confirmation validity without a
revision change, clears them and asks for a new check. Failed background reads
also clear suggestions; authorization failures clear the project material.
Late checks cannot restore suggestions after close, navigation or identity
change. Archived projects remain readable but have no active alignment check.
This is read-time authorization with existing polling, not instantaneous push
revocation or a guarantee that already displayed material can be erased.

`project-state-model.test.cjs` covers deterministic sources, versions, withdrawal,
malformed/mixed snapshots and private-context isolation.
`project-alignment.browser.test.cjs` covers the real-Hub, Mock-runtime member
views, provenance, read-only behavior, stale snapshots, access removal, identity
switching and desktop/mobile screenshots. No live-model trial or deployment is
part of this slice.

## Discuss shared state privately with your own Agent

On an active project's panel, choose **与我的 Agent 讨论**. This performs a fresh
membership-checked read and selects that project for **one** private question;
it does not send the question, publish a suggestion, or notify other members.
The composer shows the selected project revision and the scope of the context.
Review it, enter your question, then send normally. Clear the project selection
before choosing a task. Task and project context cannot be attached to the same
turn. Slash commands are blocked while a project is selected.

The Hub supplies only explicitly published project state with its item IDs,
versions, sources, confirmation validity and provenance, plus the explicitly
published shared questions and replies. Each feedback message retains its ID,
author, source item/version and reply-to reference. `current_reference: false`
marks an old or withdrawn source; `parent_withdrawn: true` marks a reply to a
withdrawn question. Withdrawn feedback bodies are empty, never supplied to the
model. Feedback is not confirmation, consent or consensus. No private conversation,
draft, task body, task result, or linked-task material is included. This isolated
question does not use prior private conversation or memory. The bounded snapshot
reports `omitted_items` and `omitted_feedback` rather than claiming to represent
everything. The total serialized snapshot is capped at 20,480 bytes; whole state
items take priority, and only feedback whose source item fits is included. A reply
may refer to an omitted question, whose content must not be inferred. The digest
covers all authorized state and redacted feedback, including omitted entries.
Withdrawn
items are included only as explicitly labeled historical records, not current
commitments. Published text and source notes are untrusted quotations, not instructions. A proposed
model suggestion remains a suggestion; neither the question nor the answer can
establish another person's commitment, consensus, or completion.

The reply is private to the asking member. The route cannot publish or confirm
state, create proposals, change tasks, or update profiles. To share a useful
answer, use the existing explicit **提为项目候选** preview-and-publication flow.
Successfully received project questions and replies remain in the asking
person’s private message history. Leaving a project or losing access is not a
remote erase of material already received. These messages are excluded from
ordinary chat context and summaries, so a later ordinary turn does not implicitly
inherit this selected project context. Fetching a new project snapshot or reading
a project-turn receipt still checks current project eligibility.

The server binds the request ID to the exact selection and checks current
membership and project version again around dispatch. A stale selection fails
instead of silently switching to a newer snapshot. Leaving or removal prevents
new discussion requests. While still authorized, retrying the same completed
request returns its private receipt without another model dispatch. Revoked
members cannot retrieve that receipt or its stored snapshot through replay. An uncertain result must be reconciled through its receipt, not
silently dispatched again. Successful sends consume the selection; account
changes clear it and fence late callbacks.

`project-discussion.browser.test.cjs` uses a real temporary Hub and Mock runtime
to check explicit selection, one-turn scope, private-only writes, exact replay,
stale versions, removal, outsider denial and account switching. Mock success is
not evidence of live-model answer quality or a production deployment.


## Explicit shared questions and replies

Each state card has **共享问答**. An active member can manually enter a question
about that exact item version, or reply to a question on the current version.
The preview names the project, item/version, author and audience, and requires
an explicit acknowledgement before publication. Private chats and Agent replies
are never automatically copied. All current and future accepted project members
can read published feedback; outsiders and departed members cannot fetch it.

Feedback is historical member-published text, not a confirmed state item, an
accepted assignment, an agreement, or team consensus. Even “yes” and “可以” do
not change state. No model is called, no external communication is sent, and
no task is executed. Replies never infer resolution; use the separate explicit
blocker-resolution action described below.
A revised source makes its old question threads read-only; create a new question
for the new version. Archived projects and withdrawn items cannot receive new
questions or replies. Existing feedback remains explicitly version-labelled.

Authors may explicitly withdraw their own feedback, including after the source
changes or the project is archived while they still have access. Ordinary reads
and replay receipts hide withdrawn text, but append-only database records retain
it for history. Withdrawal is not erasure of copies members already saw. A
withdrawn root question stops new replies, without withdrawing other authors'
existing replies. Leaving a project does not retract previously published text.

The Hub authorizes reads and writes against current membership under its database
lock. Creates bind a request ID to the exact item, source version, reply target
and content. The UI can retry the same original submission after response loss;
it does not invent a new key or silently overwrite a stale source. Logout and
navigation fence delayed callbacks. Privacy revocation is checked on reads and
through existing polling, not guaranteed instantaneous removal of displayed text.

`npm run test:questions` runs `project-questions.browser.test.cjs` against a real
temporary Hub with Mock runtime. It covers three separate browser identities,
preview/acknowledgement/cancellation, duplicate clicks, real response loss and
replay, no private-history or state-confirmation side effects, stale sources,
withdrawal, revoked access, late logout responses, and desktop/mobile screenshots
when `AGENCT_TEST_SCREENSHOT_DIR` is set. It is also in `test:hub-browser`, whose
CI job uploads those screenshots. These tests do not validate live paid models,
real users on WSL, deployment, or production delivery.


## Explicit blocker resolution and project final acceptance

A blocker confirmation means the named reporter confirms that the blocker exists,
not that it is solved. Only its original reporter can select **解除阻塞**, write
a shared resolution note, review the exact item version and explicitly confirm.
**重新打开阻塞** records that it exists again. Both are append-only statements;
withdrawal remains retirement of a report, not resolution. Revised items and
revoked authority invalidate old resolution evidence.

**项目最终验收** is separate from an individual task’s acceptance and from archive.
The creator reviews exact current goal versions and mutually shared task progress,
writes a public acceptance note, and acknowledges the preview. The server checks
current membership, effective confirmations, unresolved blockers, linked-task
acceptance and the exact snapshot under its database lock. Record readiness does
not establish deliverable quality: the creator must inspect the actual result.
No model or another participant can accept on the creator’s behalf. The wording
identifies the creator’s assertion, never unanimous team agreement.

A later project revision, relevant account-authority change or linked-task change
makes the old acceptance historical; returning to similar content cannot revive
it. A new explicit acceptance is required. The creator can withdraw an acceptance
without changing any task, other member’s statement or archive status. Shared
notes are manually entered, never copied automatically from private chats,
task bodies or drafts. Internal task snapshots are used for validation, not
continued access after a task’s public association is withdrawn.

The API rejects stale previews rather than silently updating their targets.
Idempotent retry reconciles the original request and does not recreate a claim
or restore its validity. The UI requires acknowledgement again after closing
and reopening a preview. All project APIs still enforce current access; already
received shared information cannot be remotely erased.

Explicitly selected private project discussion includes the shared resolution
status and the creator’s acceptance statement with its current validity. It
never includes linked-task snapshots or private task bodies. Oversized shared
state/feedback is still bounded and labelled incomplete. The server rechecks
completion validity and its snapshot around model dispatch, so a concurrent
linked-task change cannot return an apparently current old acceptance.
