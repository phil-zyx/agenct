# Hub SQLite schema upgrades

`Hub::open` owns schema upgrades through `hub/src/schema.rs` and SQLite's
`PRAGMA user_version`. Back up an existing database consistently before upgrading;
do not copy a live database without its WAL. There is no automatic downgrade.

Versions are ordered:

1. Adopt/create the baseline Hub, personal-profile, chat-receipt and worker tables.
2. Add task revisions, defaulting existing rows to zero.
3. Add selected-task, task-context and profile-revision receipt bindings. Existing
   bindings remain unchanged; missing bindings become NULL.
4. Add worker execution authority, defaulting unclassified jobs to zero. An upgrade
   does not authorize or start worker execution.

All pending steps, including their version markers, run in one immediate
transaction. Any migration or validation error rolls the whole upgrade back, so a
retry starts at the original version. A newer unsupported version is rejected
before changing journal mode, schema, messages, receipts or configured sessions.
Opening a file-backed Hub can still create its normal lock sidecar.

Version zero supports empty databases and subsets of the historical Hub tables,
including any combination of the five additive columns above. Present tables must
have the historical base columns, declared types, nullability, defaults, primary
keys and unique keys. Old receipt tables without the status CHECK are supported
and preserved. The named active-worker index is created if absent on version zero;
an existing index with a different definition is rejected. Missing tables/columns
at an already-recorded version, unknown columns, nonhistorical constraints,
collations and generated columns are rejected rather than silently stamped as
current. This is a structural compatibility check, not a verifier for hostile
SQLite files. Ordinary nonunique indexes and triggers are left intact, and other
subsystems' tables are not adopted or rewritten.

After successful migration, a separate transaction performs recovery on **every**
open, including an already-current database. Pending messages become interrupted;
messages attached to pending chat receipts and those receipts become uncertain.
Recovery failure rolls back both sides together. It never redispatches a turn.

The public `worker::migrate` API remains available for standalone library users
and does not set a Hub schema version. Publication storage retains its independent,
opt-in migration and is not enabled by opening a Hub.

## Verification

Offline Rust storage regressions cover all legacy additive-column combinations,
each migration failure boundary and retry, prior versions, incompatible layouts,
constraints, preserved data, atomic recovery, repeated Hub opens and future-version
refusal without changing database bytes. They do not start listeners or providers.

```sh
cargo test --locked --manifest-path hub/Cargo.toml --lib schema::tests
cargo test --locked --manifest-path hub/Cargo.toml --test schema
```

Aggregate checks below also exercise the existing integration suites, some of
which start local fixture HTTP listeners. Run those only where permitted.

```sh
cargo test --locked --manifest-path hub/Cargo.toml
cargo fmt --manifest-path hub/Cargo.toml -- --check
cargo clippy --locked --manifest-path hub/Cargo.toml --all-targets -- -D warnings
```
