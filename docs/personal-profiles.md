# Personal agent configuration foundation

The authenticated Hub exposes a private, revisioned profile for each configured user.
There is no public write endpoint, new dashboard, or separate agent conversation UI.
In the existing chat, send `/agents` (or `配置个人 Agent`) to open an inline card.
Edit names, fixed roles, server model references, and enabled flags, then inspect
the complete replacement before explicitly confirming save. Cancel makes no write.
Refresh preserves the editing card; logout invalidates it. A stale revision or
uncertain save requires rereading server state, never an automatic retry.
This is configuration storage, **not an autonomous agent army or a model switcher**.
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

Role assignments and enabled flags are stored for future orchestration; they do not
currently spawn agents or change chat behavior. There is no personal API-key entry,
provider URL entry, dynamic model catalog, or per-user provider routing. These need
operator-managed model bindings, secure credential provisioning, runtime dispatch,
quotas, tool policy enforcement, and end-to-end live-provider tests before claiming
an executable personal agent team. Development tokens remain development-only.
