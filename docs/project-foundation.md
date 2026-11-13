# Project and membership foundation

This is an additive project foundation with a three-person coordination UI.
It is not autonomous agent negotiation or a production access-control claim. Existing one-requester/one-owner tasks keep their
existing lifecycle and authorization.

## Scope and safe defaults

- A project has a creator, explicit name and goal, `temporary` or `ongoing` kind,
  active/archived status, and a revision. The kind records intent; neither kind
  silently expires or schedules background work.
- Only the creator can invite or remove members and archive the project. There is
  no ownership transfer, administrator inheritance or arbitrary role system.
- Invitations target an existing active identity in the same configured workspace
  and tenant. They do not provision accounts or send external messages.
- Invitees see a goal/name/creator preview in their invitation list. They cannot
  read the project roster or task links before accepting. Joining means access to
  the project's deliberately shared coordination data, not the member's personal
  agent, private history or execution authority.
- Accepted members can leave. The creator cannot leave and orphan the project;
  they may archive it. Removal and voluntary departure take effect immediately.
- Up to 50 active or invited members are allowed. This is a safety bound, not a
  claim that 50-person collaboration has been load-tested.

## Task association and visibility

Either original task participant can propose a link only when both original
participants are active project members. They supply a new, explicit shared
coordination label. The other original participant must approve it before anyone
else in the project can see it. A pending link is visible only to those two people.

Approval shares the label, task ID, two participants, and live task status/revision
with accepted project members, including people who join later. It does **not**
share the original task summary, materials, event history, submitted results,
private draft contents or IDs, conversations, profiles, worker artifacts, or
model context. Project members who are not original task participants still
cannot open or operate the underlying task. `can_open_task` is presentation
metadata, not a substitute for the underlying task API's checks.

Either task participant can revoke a link. The project creator can revoke an
active link. When an original participant leaves or is removed, their links are
revoked in the same transaction. Rejoining does not restore prior sharing:
propose and approve again. A disabled or no-longer-configured participant's links
are hidden while the account is inactive; account reactivation restores that
account's existing project membership unless explicitly removed. Project access
also fails closed while its creator is disabled or unavailable. Active members
can still read their own membership revision, leave, or revoke their task links
without reading project content; restoring the creator never restores withdrawn
consent.

Archiving stops invites, joins and new/approved links. Existing members retain
read access; departure, removal and link revocation remain available. It does not
cancel underlying tasks, stop other workflows, or freeze their live status.

## API

All routes require the existing authenticated active identity. Unknown and
unauthorized projects/members/tasks return 404. Stale project revisions return
409; callers must read current state and re-review, never silently retry a write.
JSON requests reject unknown fields.

- `POST /api/projects`: `{name, goal, kind, request_id}`. Returns the project.
  Replaying the same creator/request ID and content returns the same project;
  different content with that key returns 409.
- `GET /api/projects`: accepted projects only, as an array of project records.
- `GET /api/project-invitations`: pending invitation previews only, as project
  records. Archived/unavailable projects are omitted.
- `GET /api/projects/{id}`: `{project, members, tasks}`. Non-creators see active
  members; the creator also sees invited/left/removed/declined members that are
  still valid active workspace accounts.
- `POST /api/projects/{id}/members`: `{owner, expected_revision}` invites a user.
- `GET /api/projects/{id}/membership`: the active caller's own `{status, revision}`
  only, including while the creator is unavailable, to support withdrawal.
- `POST /api/projects/{id}/membership`:
  `{action: "accept" | "decline" | "leave", expected_revision}`.
- `POST /api/projects/{id}/members/{member}/remove`: `{expected_revision}`.
- `POST /api/projects/{id}/archive`: `{expected_revision}`.
- `POST /api/projects/{id}/tasks`: `{task_id, label, expected_revision}` proposes
  association and explicitly approves sharing from the proposing participant.
- `POST /api/projects/{id}/tasks/{task}/actions`:
  `{action: "approve" | "revoke", expected_revision}`.

Writes other than creation return `{revision}`. Project revision starts at zero
and advances atomically with each successful mutation. Concurrent/stale requests
cannot apply twice. An uncertain response must be reconciled through reads. No
background model call, task assignment, notification, or worker job is triggered
by any project route.

Project fields: `id`, `creator`, `name`, `goal`, `kind`, `status`, `revision`,
`created_at`. Member fields: `owner`, `status`. Shared link fields: `task_id`,
`label`, `proposed_by`, `status`, `requester`, `owner`, `task_status`,
`task_revision`, `can_open_task`.

## Migration and remaining work

Schema v18 adds `projects`, `project_members`, and `project_tasks`. Existing
projects are not inferred from private messages, existing tasks, contacts or
organization membership. No old rows are copied or their ACLs widened. The Hub's
transactional migration/rollback and schema validation cover the new tables;
tenant initialization audits historical project membership before committing a
partition assignment. Keep normal database backups before upgrades; old binaries
reject newer database versions instead of silently serving them.

The project panel now supports invitations, shared state with source/version
confirmation, explicit private-chat candidates, read-only alignment checks, and
bilateral task association. Choose **关联我的任务**, select a task whose two original
participants are active project members, and write a new **共享协调标签**. No task
summary is copied into that label. Preview and confirm the proposal; the other
participant must separately preview and approve public coordination visibility.
Either participant can withdraw, and the creator can withdraw an active link.
The third member sees only the approved projection, with no task-opening control.

**退出项目**, creator-only **移除成员**, and **归档项目** each explain their effects
before confirmation. Archived projects retain reads and withdrawal/removal, not
new sharing. A stale revision requires fresh review. A lost response to these
non-idempotent writes disables submission: cancel and read current state before
considering a new action; never automatically retry. If a currently open project
becomes unavailable because its creator is disabled, the panel clears project
contents and offers withdrawal using only the caller’s membership revision.

This deliberately leaves project edits, owner succession, immutable project event
history, material publication, notifications, reminders and agent execution to
separately reviewed work. Bilateral link approval and creator-only membership
management remain conservative choices, not consent to autonomous sharing.

## Verification

`projects_tests.rs` adds HTTP boundary tests for creation/replay, invitation
acceptance, stale revisions, three members plus an outsider, task-link approval,
private-data projection, departure/rejoin, archive/removal, account disable, and
tenant isolation. `schema.rs` covers additive migration without history sharing.
Run the full repository checks (including Rust test/clippy/fmt and the separate
three-person HTTP acceptance harness) before merge. JavaScript checks alone do
not verify this backend implementation.

`hub/web/project-links.browser.test.cjs` exercises the real Hub with three isolated
browser identities and Mock runtime: private link proposal, bilateral approval,
third-member projection/denied task access, stale confirmation, uncertain writes,
withdrawal and rejoining, member removal, archive, mobile layout and identity
changes. These synthetic-browser checks are not real-user device or paid-model
acceptance. Run `npm run test:projects` after building the Hub.
