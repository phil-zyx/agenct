> 历史专题记录：保留设计背景、协议细节和当时的验收结果；其中“当前/尚未”描述可能早于最新实现。现行用法以[入门](../getting-started.md)和[参考](../reference.md)为准。

# Runtime turn boundary

The Hub's internal `runtime::Runtime` trait separates one turn of runtime output
from authenticated message/proposal persistence. `MockRuntime` and `DshRuntime`
both return the same strict `AdapterReply` type. The Hub applies the same
`AdapterReply::valid` checks before persisting either backend's proposal:

- The backend matches the configured runtime.
- Replies are nonempty and at most 32,000 UTF-8 bytes.
- A proposal names a different configured participant present in the current message.
- Its summary is nonempty, at most 4,000 UTF-8 bytes, and an exact substring of the
  current message. History and selected task context cannot supply this authority.

Neither runtime receives a Hub, database handle, participant roster, or task API.
DSH receives only the bounded turn payload and existing authenticated HTTP client.
Transport failures never fall back to the mock. Existing timeouts, no-proxy and
no-redirect client settings remain owned by Hub initialization.

The exported `Runtime` enum remains the configuration API, so existing callers
using `Runtime::Mock` or `Runtime::Dsh` need no changes. The internal trait uses
static dispatch and a Send future, without adding a dependency or enabling
arbitrary third-party runtime registration.

The mock's deterministic delegation parser now lives in `MockRuntime` and emits
an unvalidated proposal. The Hub preserves the established mock proposal-only
history behavior, but uses the same validation and transactional persistence path
as other replies. A proposal is still sender-private; only a separate explicit
confirmation creates a task. Receipts remain reserved before runtime invocation
and finalized atomically with results.

This refactor depends on the domain split in issue #5 (`messages.rs` and
`proposals.rs`). It makes no schema changes and enables no worker execution or
publication. Unit tests check shared validation with both backend labels; existing
API acceptance tests remain unchanged, with an additional invalid-mock regression.
