# Private cloud staging, without public token exposure

This package makes the **existing Rust Hub** reproducible on a trusted Linux host. It is not a hosted deployment, production identity system, public demo link, or claim that DSH has been tested with a real provider. No infrastructure is provisioned. Default runtime is explicitly `mock`.

## Security boundary

- Hub listens on `127.0.0.1` only, including when an inherited environment requests a public bind address.
- Use an existing, authorized SSH connection to reach it. Do not use a public tunnel, public reverse proxy, port forwarding to `0.0.0.0`, or publicly shared development tokens.
- Each participant has a distinct high-entropy token. Server configuration contains only SHA-256 hashes. Tokens are written to a separate owner-only file, never printed by the launcher, and must be transferred individually over an approved private channel. Do not paste them into tickets, chat, screenshots, URLs, shell arguments, or committed files.
- This is for a small consenting trial on a trusted host. Administrators and processes with the same OS identity can read the database and credentials. Loopback is not isolation from other users on the server; bearer authentication remains required. Data is not encrypted at rest.
- Use a private parent directory owned by the operator, outside the checkout, with no untrusted writable ancestors. The launcher rejects symlinked state directories/configuration/database files, group/world-readable state, malformed identities and implicit credential replacement. It is not a defense against a concurrently malicious process with the same OS identity.

## Build and initialize

Requirements: Linux, Python 3.9+, Rust stable with Cargo, an authorized trusted host, and an existing SSH route. Build runs dependencies from the committed lockfile; review and follow your environment's software installation policy.

From the repository root:

```sh
cargo build --locked --release --manifest-path hub/Cargo.toml
python3 -m unittest discover -s scripts -p 'test_*.py' -v
python3 scripts/smoke-private-staging.py

# Choose an existing private parent directory. The final directory MUST NOT exist.
python3 scripts/private-staging.py init --state-dir "$HOME/agenct-private" --users alice bob
python3 scripts/private-staging.py check --state-dir "$HOME/agenct-private"
python3 scripts/private-staging.py run --state-dir "$HOME/agenct-private"
```

Shortcut: `python3 scripts/private-staging.py start --state-dir "$HOME/agenct-private"` initializes the directory once if it does not exist (choosing a free port from 8080 upward unless `--port` is given), rebuilds the release binary with `cargo build --locked --release`, checks the port and runs. It never reinitializes or rotates an existing directory. `run` and `start` refuse to launch when the configured loopback port is already taken by another program, or when a Hub already holds this directory's lock, and print the access URL plus the credential file location (never the tokens) before starting.

`init` creates a mode-0700 directory and mode-0600 `config.json` and `credentials.json`. It refuses an existing directory, so restarting never rotates identities. `--port` customizes the port at initialization. `--users` is an initialization argument only. Change the port later with `set-port --port <PORT>`, which validates the directory and atomically rewrites only `config.json`; credentials and data are untouched. The launcher always derives the database location inside that directory and starts the release binary from the checkout root, where the UI assets are located. Keep `hub/web` alongside the binary's checkout.

Read the credential file in an authorized private editor and provide each participant **only their own** token. Never serve the state directory over HTTP. For a two-person acceptance run, the operator may instead use two separate private browser profiles without distributing credentials.

## Open the one-window UI

On the operator's local computer, using a host they are already authorized to access:

```sh
ssh -N -o ExitOnForwardFailure=yes -L 127.0.0.1:18080:127.0.0.1:8080 YOUR_EXISTING_SSH_HOST
```

Open `http://127.0.0.1:18080` in the local browser and paste the appropriate token into the login form. The SSH connection encrypts traffic between computers; the two HTTP segments remain on loopback. Verify the SSH host key normally; never bypass a mismatch warning. Close the SSH process to end access. This creates no public URL and must not be advertised as a public cloud deployment. Granting additional SSH accounts or changing firewall/security settings is a separate operator-approved action.

## Acceptance checklist

1. `check` reports mock mode. This validates configuration only, not a live process or provider.
2. An unauthenticated request to `/api/me` is rejected. A valid token identifies the intended user; a fake token is rejected.
3. Alice and Bob see separate private messages. Use the controlled delegation flow from [rust-hub.md](rust-hub.md): a proposal remains private until sender confirmation, then the other participant can accept and deliver a result.
4. Restart the Hub using the same state directory. Private history, tasks and revisions persist; no credentials change. Do not automatically repeat uncertain external actions.
5. Stop the Hub and tunnel. Confirm the local UI is no longer accessible through the tunnel.

Run API tests and browser acceptance independently. Passing launcher tests does not establish UI correctness, provider compatibility or multi-tenant security.

## DSH mode

Provisioning a provider credential and making billable model calls require the appropriate authorization. The launcher never requests, generates, stores or prints provider API keys; it only passes `DEEPSEEK_API_KEY` from the current shell to the adapter process.

Simplest: `start --runtime dsh` for a new directory, or `set-runtime --runtime dsh` for an existing one, then `start` with `DEEPSEEK_API_KEY` exported. The launcher installs the pinned adapter dependencies into `<state-dir>/dsh-venv` and supervises adapter + Hub together; see [launcher-managed mode](dsh-adapter.md#launcher-managed-mode-recommended). `check` reports whether that environment is installed and whether the key is set (never its value).

To run the adapter yourself instead (on the **same trusted host**), follow [dsh-adapter.md](dsh-adapter.md#manual-configuration), set `runtime` to `dsh` and supply both `AGENCT_ADAPTER_URL` and `AGENCT_ADAPTER_TOKEN` to the launcher through your approved secret mechanism. The Hub validates the adapter's loopback URL. Missing configuration fails closed; there is no silent mock fallback. The adapter's provider secret belongs to the adapter process, not the browser. A mock smoke test is not real-model acceptance.

## Optional service operation

`deploy/agenct-private.service.example` is a **template**, not an installed service. An operator must review paths, create a dedicated unprivileged account and owner-only state directory, and explicitly approve installation on their host. Build before starting; do not give the service permission to modify the checkout or run as root. The example is for mock Hub operation, not an adapter supervisor, container deployment or a complete production hardening profile. It stops with SIGINT, matching the Hub's graceful-shutdown handler.

Before upgrades, stop the service, create an owner-only backup of the **whole state directory**, and preserve the repository commit. Do not copy a live SQLite main file without its WAL. Verify restoration into a separate private directory with the same compatible application version; newer schemas may not be downgrade-compatible. Never commit backups. A production retention/backup policy is not supplied.

To revoke access, stop the service, remove the identity or replace its hash with a newly generated high-entropy token's SHA-256 hash in `config.json`, then restart. Existing browser credentials will no longer authenticate after restart. Removing an identity does not erase its history. Do not reuse old user IDs for different people. Coordinate pending tasks before removal.

## Remaining deployment gates

- No remote host, domain, TLS certificate, OIDC/session system, public rate limiting, production tenant policy, abuse controls or production security review is provided.
- No public proxy/container configuration deliberately weakens the Hub's loopback constraint.
- Real provider credentials and live DSH acceptance remain separate gates.
- Only an actual successful remote start plus browser test can establish that a remote instance is usable. Repository readiness is not hosting readiness.
