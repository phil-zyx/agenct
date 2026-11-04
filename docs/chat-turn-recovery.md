# Durable chat-turn receipts

Chat clients can send an owner-scoped `request_id` with `POST /api/messages`.
The Hub reserves this ID durably before dispatching a model request. It binds the
exact message and requested agent selection, snapshots the current runtime profile,
and records completion in the same SQLite transaction as the assistant reply and
any sender-private delegation proposal. Delegation still requires its separate
explicit sharing confirmation. Receipts never authorize execution or publication.

This is **at-most-once adapter dispatch per authenticated owner and request ID**,
not exactly-once provider execution. A provider may finish even if its response is
lost. The Hub does not retry provider calls, infer success, or fabricate the missing
reply. Losing/restoring the database or deliberately using a new ID is outside this
deduplication guarantee. Existing single-process database locking remains required.

## API

- A successful keyed POST returns the usual reply, backend, proposal and frozen
  runtime profile, plus `turn: {request_id, status: "complete"}`.
- An identical completed replay returns the original saved response without a new
  model call. Later profile edits do not change it. Proposal data in the response
  is a historical snapshot: reread `/api/proposals` before presenting current
  confirmation controls; existing proposal revisions still fence actions.
- Changing the message or explicit agent selection under that ID returns 409.
  Omitting an agent and explicitly naming the coordinator are different bindings.
- A replay of an unresolved receipt returns `reply: null` and a `turn` with status
  `pending` or `uncertain`. It never dispatches again. The initial adapter failure
  still returns the existing error response; read the receipt to reconcile it.
- `GET /api/turns/{request_id}` returns `{request_id, status, response}`. The response
  is the saved complete POST result or null. Other users' IDs return 404. Requests
  use the same bearer/session authentication and browser CSRF boundary as chat.
- IDs are 1–128 ASCII letters, digits, underscores or hyphens. A key already used by
  the separate proposal endpoint conflicts with a new chat request, and vice versa.
- API callers omitting `request_id` retain legacy behavior: there is **no durable
  deduplication or receipt**. New clients should generate a fresh unpredictable ID
  per intentional turn and retain it until the outcome is reconciled.

`pending` means a reserved request may still be active. Failed/invalid adapter
responses become `uncertain`. Startup converts interrupted pending receipts to
uncertain; a receipt read also marks an abandoned handler uncertain when no turn
holds that owner's gate. Receipt and linked user-message status are reconciled
together, so an abandoned message is not left showing an active pending turn. Neither state permits reuse to dispatch. Cancellation of
an HTTP handler does not establish cancellation of work already sent upstream.

## Browser behavior

The chat client remembers unresolved receipt IDs in owner-scoped tab session
storage, without saving credentials or message text there. It exposes a read-only
status check after transport failure or refresh instead of silently resending.
Chat POSTs have a 90-second browser deadline. Expiry stops waiting, retains the
unresolved receipt and enables status checks; it does not cancel or retry provider
work. An explicitly confirmed new attempt enables a subsequent explicit Send,
which generates a new ID and can duplicate upstream work if the prior result is
unknown.

Expired or unknown authentication hides the recovery controls but retains the
receipt until the same owner signs in again. A verified different owner or auth
mode clears it without exposing the previous owner's receipt. Explicit logout
clears local recovery; it does not cancel a provider request or erase the server
receipt. Recovery on another browser/device, after clearing tab storage, or after
logout requires the original ID; the API does not provide a receipt list.

## Verification and limits

Instrumented fake-adapter tests cover one dispatch after replay, concurrent calls,
owner isolation, changed bindings, interrupted handlers, database restart, profile
changes, actual transport timeout, malformed responses, reservation failure,
transaction rollback and private proposal creation.
SQLite trigger failures verify that a completion error cannot leave an assistant
reply committed without its matching completed receipt. Browser tests exercise
recovery controls and session storage with a simulated DOM, including expired
login recovery, stalled sends, cancellation and identity changes. An optional
real-browser suite is available at `hub/web/client.browser.test.cjs`. With
Playwright and Chromium separately available, run `node hub/web/client.browser.test.cjs`;
set `CHROMIUM_EXECUTABLE_PATH` when using an existing Chromium executable.
It uses only a loopback fake API and synthetic data, with no real model or worker.

No live model credentials or real provider calls are needed for these checks. They
do not establish provider-side exactly-once execution, real-browser acceptance,
host-crash/power-loss durability, multi-process dispatch or indefinite retention.
Stored receipts contain private messages and replies; deployment still needs an
explicit retention and backup policy. The production executor remains disabled.
