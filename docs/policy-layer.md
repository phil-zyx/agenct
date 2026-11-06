# Hub authorization policy

`policy::can(actor, action, resource)` is the pure decision boundary for task
lifecycle mutations, proposal confirmation/rejection, and worker admission and
cancellation. It receives trusted resource state loaded by the handler, not
model-supplied identities or permission flags. It cannot read/write storage,
dispatch a runtime, create a task, or grant execution/publication capabilities.

Task transitions are declared in `TASK_TRANSITIONS`; proposal targets and worker
cancellation states are data tables too. Identity and reviewed revisions are
checked before allowing transitions. Handlers retain authentication, scoped
resource loading, body validation, transactions, and persistence. SQL owner/state
predicates in mutation statements remain defense in depth rather than substitutes
for policy decisions. The configured worker executor/repository policy remains a
separate execution safety boundary.

Current policy and preserved boundaries:

- Task requester/owner visibility and 404 concealment remain unchanged.
- The owner accepts and delivers; the requester approves or returns.
- Approved #9 policy: requester cancellation is allowed in awaiting_owner,
  working and ready_for_acceptance; owner cancellation only in awaiting_owner
  and working. Terminal and unknown states fail closed.
- `policy::cancellation_reason` requires explicit shared text of 1–500 Unicode
  characters after trimming. The handler invokes it before mutation; the audit
  writer also validates it. Other task actions reject a supplied reason. See
  [the full cancellation contract](architecture.md#任务取消策略用户明确批准).
- Proposal recipients cannot confirm/reject sender-private proposals. An identical
  terminal replay at the original reviewed revision returns the existing result,
  even if recipient availability subsequently changes.
- Queue admission still needs explicit queue-only confirmation and the exact
  operator scope. A fresh queue job needs the current accepted task revision.
  An existing receipt replay does not requeue, reauthorize, or rerun work.
- Only the task/job owner can cancel a queue job; cancelled-job replay is allowed,
  failed-job cancellation remains denied, and route task/job identities must match.
- Authentication, response status/error wording, worker lease revocation,
  transaction boundaries and production execution disablement remain intact.

Tests exhaustively compare task, proposal and worker decisions across identities,
states, actions, revision combinations and scope combinations. HTTP negative
matrices verify every route in this scope denies inappropriate actors/states and
leaves task, proposal, event and worker state unchanged. Existing integration tests
cover successful transitions, idempotent retries and revocation behavior.

This work depends on issue #5's domain split. It does not depend on #7's runtime
implementation and changes no runtime or schema behavior. Issue #10 supplies the configured `Directory` used to populate
`recipient_available`, independently of per-user runtime gates. Later explicit policy changes should update the table and its
identity/state matrices together.
