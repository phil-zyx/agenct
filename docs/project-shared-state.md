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
