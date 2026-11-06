# Shared task timeline in chat (#13)

Task cards expose “查看共享时间线” to their requester and owner. The control lazily
reads the existing participant-authorized `GET /api/tasks/{id}` DTO. Its additive `timeline: {events, deliveries, audit}` field reuses exactly the
same filtered projection as the preserved top-level compatibility fields. It makes no
mutation, draft request, model call, or worker operation. The Hub is the authority
for participant filtering; frontend validation does not replace access control.

The card shows public lifecycle events with actors, recorded timestamps, task
revisions and linked submitted delivery versions. Returned versions remain visible.
Only the explicit public delivery projection is accepted; private draft IDs,
provenance links and unknown delivery fields cause the view to fail closed. Text
uses `textContent`, never HTML. No model reasoning or private chat is reconstructed.

Legacy incomplete history carries a prominent warning. A surviving historical
result without a verified event link is displayed separately, never inserted into
an invented transition. Timestamps are shown as recorded without guessing a zone.
Task or account changes invalidate in-flight reads and hide old timeline content.
Read timeout releases the view for an explicit retry. Refreshing reads never changes
task state. Private draft cards and manual delivery remain independent controls.

Simulated-DOM and presentation tests cover retained deliveries/returns, legacy gaps,
private-field rejection, requester/owner/stranger visibility, late callbacks and
read timeout. Existing Rust API tests verify the actual shared DTO and its privacy
boundary; #18 exercises the mock cross-person drafting and delivery journey. These
are not real-browser, WSL, live-provider, or production-execution acceptance.
