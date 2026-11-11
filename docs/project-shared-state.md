# Explicit shared project state

This API-only slice builds on project membership. It records explicitly published
coordination items: goals, assignments, blockers, and pending decisions. It does
not yet provide a project UI, chat extraction, agent-to-agent negotiation,
notifications, task execution, or automatic fact checking.

## Publication and confirmation

A member manually supplies the text, source type, and source note to share with
all accepted project members, including future joiners. Publishing never reads a
private conversation, task, draft, attachment, or model result. Source notes are
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
private-staging, and Docker CI matrix remains the merge gate. No UI acceptance or
production deployment is claimed by this API-only slice.
