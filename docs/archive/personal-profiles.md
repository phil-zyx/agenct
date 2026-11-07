> 历史专题记录：保留设计背景、协议细节和当时的验收结果；其中“当前/尚未”描述可能早于最新实现。现行用法以[入门](../getting-started.md)和[参考](../reference.md)为准。

# Personal agent configuration foundation

The authenticated Hub exposes a private, revisioned profile for each configured user.
There is no public write endpoint, new dashboard, or separate agent conversation UI.
In the existing chat, send `/agents` (or `配置个人 Agent`) to open an inline card.
Edit names, fixed roles, server model references, and enabled flags, then inspect
the complete replacement before explicitly confirming save. Cancel makes no write.
Refresh preserves the editing card; logout invalidates it. A stale revision or
uncertain save requires rereading server state, never an automatic retry.
This supports **one fixed role per turn, not an autonomous agent army or a model switcher**.
The existing single chat still executes through the operator-selected runtime.

- `GET /api/profile/catalog`: server-owned model references, supported roles and limits.
- `GET /api/profile`: your profile and revision, defaulting to one enabled coordinator.
- `PUT /api/profile`: replace your own configuration with `expected_revision` and `agents`.

Each agent has `id`, `name`, `role`, `model_ref`, and `enabled`. Roles are coordinator,
researcher, writer, and reviewer. There must be exactly one enabled coordinator,
1–8 agents, unique ASCII IDs (1–32 bytes), and names of 1–96 bytes without controls.
All requests use the existing bearer authentication; callers cannot supply an owner.
Stale writes return 409. Invalid values return 400; unrecognized fields return 422.
Profiles persist in SQLite and remain isolated by authenticated identity.

Example PUT body:

```json
{"expected_revision":0,"agents":[
  {"id":"butler","name":"个人管家","role":"coordinator","model_ref":"server-default","enabled":true},
  {"id":"research","name":"研究员","role":"researcher","model_ref":"server-default","enabled":true}
]}
```

`server-default` is the only current model reference. It describes the operator's
existing runtime configuration, not a credential, endpoint, or permission grant.
The API rejects raw credential/endpoint fields and unknown model references. Names
are display labels, not system instructions. Do not put secrets in display names or
chat messages: this API is not a secret vault. Profiles are not included in shared
tasks, cannot expand tool access, and do not affect delegation authorization.

## Deliberate limits

Chat snapshots the authenticated user’s profile at turn start and selects its enabled
coordinator. An optional `agent_id` in `POST /api/messages` selects another enabled
role from that same profile. Unknown/disabled IDs or invalid stored profiles fail
before invoking the adapter. Updates take effect on the next turn; they do not
change an in-flight snapshot. The reply reports `runtime_profile` with agent ID,
profile revision, role, and model reference. The entire single-user chat history
remains shared between that user’s roles; these are not isolated subagent sessions.

The DSH adapter accepts only the four fixed role values and `server-default`, then
adds server-owned writing/research/review guidance. Display names and free-form
role descriptions never become runtime instructions. Every role keeps the same
no-tools patch, provider, operator-selected model, proposal validation, and human
confirmation boundary. Role guidance is a prompt preference, not an authority
boundary or a guarantee of output quality. There is no fan-out or background team.
The mock runtime remains deterministic and does not exercise a model.

Example single-role turn: `{"message":"Review this draft…","agent_id":"research"}`.
The chat composer now offers an optional “选择本次角色” control. It reads this
account's profile and lets the user select one enabled role for the next ordinary
chat, sending only its ID and expected profile revision. “恢复默认管家” explicitly
clears the selection. A selection never sends a message by itself. See
[per-turn role selection](role-selection.md) for revision and recovery behavior. Deploy the Hub and
adapter together: an older adapter rejects the new profile payload rather than
silently dropping routing. Legacy direct adapter requests without a profile retain
the coordinator default for compatibility.

Fake-adapter and fake-SDK tests cover owner isolation, disabled/unknown selections,
fixed-role prompting, metadata bounds, and unchanged runtime authority. These are
not live model or autonomous team acceptance tests. There is no personal API-key entry,
provider URL entry, dynamic model catalog, or per-user provider routing. These need
operator-managed model bindings, secure credential provisioning, runtime dispatch,
quotas, tool policy enforcement, and end-to-end live-provider tests before claiming
an executable personal agent team. Development tokens remain development-only.
