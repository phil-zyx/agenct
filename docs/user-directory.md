# Configured membership and turn serialization

`Directory` owns the fixed startup roster from `AGENCT_USERS_JSON`. It validates
participant identifiers and duplicate IDs, answers membership checks, and permits
delegation only between two distinct configured participants. Authentication still
owns tokens, browser sessions and OIDC validation; the directory is the roster
passed to those authentication mechanisms, not an identity proof.

Manual proposals, proposal confirmation, model proposal validation and worker
scope configuration consult directory policy. No membership decision consults the
runtime gate map. Gates retain their existing per-user `try_lock` behavior and
shared `Arc` ownership: one user's active turn blocks another turn for that user,
not another participant. Receipt replay and reconciliation ordering are unchanged.

Membership remains immutable for the running process. Future tenant/OIDC member
policy belongs in the directory, but this extraction adds no dynamic enrollment,
new delegation rights, authentication fallback or execution permission. Any future
dynamic roster must coordinate gate lifecycle before admitting a new identity.

Offline tests cover directory policy/invalid IDs, gate independence and existing
Hub authentication, delegation and concurrency behavior.
