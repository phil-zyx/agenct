> 历史专题记录：保留设计背景、协议细节和当时的验收结果；其中“当前/尚未”描述可能早于最新实现。现行用法以[入门](../getting-started.md)和[参考](../reference.md)为准。

# One-chat, one-turn role selection

The composer has an optional “选择本次角色” button. Ordinary chat remains usable
without opening a configuration screen: no selection uses the current enabled
coordinator. Selection reads only the authenticated owner's profile, offers enabled
roles, and sends no message. “恢复默认管家” explicitly clears an unsent selection.
Closing the picker preserves the previous selection; a new read invalidates older
picker buttons and late responses. Names render as text, never HTML or instructions.

A selection applies to the next ordinary chat only. `/agents` configuration and
`/delegate` proposal commands do not consume or attach it. Role selection can be
combined with one explicitly selected task. Each retains its own revision boundary.
There is still one conversation and one server-configured model call, not an
independent agent session, background team, new tool permission, or execution grant.
Mock mode does not call a model.

## Server-authoritative binding

The UI sends `agent_id` and `expected_profile_revision`, never a caller-chosen role,
model, owner, endpoint, or credential. The Hub reads the authenticated user's stored
profile, validates the revision and enabled agent, freezes its runtime profile,
and reserves the request receipt under the existing database mutex. A concurrent
profile edit cannot alter that snapshot between validation and reservation.

The optional expected revision must be a nonnegative signed 64-bit integer paired
with a valid explicit agent ID. Legacy API clients can still omit the expected
revision; those calls use the profile current at dispatch and do not claim reviewed
revision binding. New UI selections always send both fields. API clients that
need the same binding should do likewise.

Receipt identity includes the expected revision. Changed message, selected ID,
expected revision or selected task under the same request ID conflicts. An identical
pending replay never redispatches. Completed replay returns the original frozen
profile even after profile edits, agent removal, or restart.

Two exact server codes identify rejection before receipt reservation/dispatch:

- 409 `selected_agent_stale_before_dispatch`: selected profile revision changed.
- 400 `selected_agent_unavailable_before_dispatch`: selected agent absent/disabled.

Only those exact status/code pairs let the client release that unstarted receipt.
It preserves the draft and task selection and marks the role invalid; the user
must reselect or explicitly clear it. Generic 400/409 responses, transport failures,
timeouts and mismatched receipts remain uncertain. Nothing automatically resends.
A same-page confirmed profile save invalidates an unsent selection immediately;
changes elsewhere are caught when rereading the picker or by the server at send.

## Interrupted sends

During send or uncertainty, role changes are locked. Tab session storage retains
only the request ID, owner/auth mode and selected agent ID/revision, not role names,
message text, task contents or credentials. Reload and same-owner reauthentication
keep this binding. A completed receipt with a different selected ID or revision
cannot clear recovery; while the original in-memory snapshot exists, role/model
metadata must also match. Profile edits do not replace the pending role.

Explicit logout or a different verified owner clears local recovery. Starting a
new attempt requires the existing warning confirmation and drops old role/task
selection. A subsequent explicit Send uses a new request ID and can duplicate
upstream work if the old outcome was unknown. Clearing local recovery does not
cancel provider work or erase the server receipt.

## Verification scope

DOM regressions cover one-shot selection, default/clear behavior, enabled-only
choices, malformed profiles, close/reopen/late reads, profile edits, identity
changes, refresh and receipt recovery, mismatched receipts, pre-dispatch codes,
role/task interaction, and explicit new attempts. Backend regressions cover revision
validation, owner isolation, disabled IDs, migration, concurrent edits and frozen
replay. Actual Rust test results must be recorded separately from authored tests.
Real-browser and live-model acceptance are not established by these fixtures.
Production execution, provider routing and persistent DSH sessions remain unchanged.
