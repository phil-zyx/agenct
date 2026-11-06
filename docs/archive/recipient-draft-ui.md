> 历史专题记录：保留设计背景、协议细节和当时的验收结果；其中“当前/尚未”描述可能早于最新实现。现行用法以[入门](../getting-started.md)和[参考](../reference.md)为准。

# Private recipient drafting in chat (#17)

The task owner's “我的私密交付草稿” button opens a card inside the existing chat.
It reads owner-only receipts, including paginated older requests. A working task
allows an explicit draft request, optionally using an enabled personal role. The
server resolves the role, task snapshot and policy. No ordinary private chat is
sent. This is text generation, not autonomous work or verified execution.

Generated text is editable private UI state. The user must explicitly choose a
fresh draft for the manual delivery form, then separately click the existing
manual delivery button and confirm the reviewed text. Generation never calls a
task action. Shared state changes only through the existing revision-checked
manual delivery action. Live top-level `current_stale` and current task revision
are checked; historical `draft.stale` is not treated as current authority.

Only the four documented HTTP 409 before-reservation codes release an unstarted
request. All other errors retain the owner-scoped request key and show recovery,
not automatic retry. The browser ends its POST wait after 90 seconds without
claiming provider cancellation. An explicit new-attempt confirmation warns about
duplicate remote work/costs and still requires another explicit draft click.
Tab storage contains owner/auth mode/task/key only, no credentials or draft text.
Temporary authentication expiry preserves recovery; explicit logout or another
verified owner clears local recovery. Server owner-only discovery remains available.

Simulated-DOM tests cover privacy, manual review, stale state, exact coded errors,
generic uncertainty, role binding and late identity callbacks. They do not prove
real-browser, WSL, live provider, or real DSH acceptance. Production execution stays
disabled. The backend API contract is in [recipient-draft-api.md](recipient-draft-api.md).
