# Pinned external identity foundation (opt-in)

The Hub can validate an external identity provider's **RFC 9068 JWT access
 tokens** for its API. This is a tested authentication foundation, **not a
production-ready login system** or an OIDC authorization-code client. It does
not accept ID tokens. Existing loopback-only binding is unchanged.

## Policy

Set `AGENCT_OIDC_CONFIG_FILE` to an operator-controlled JSON file. If absent,
existing development bearer-token authentication remains unchanged. If present,
invalid configuration fails startup, and all development tokens are disabled.
There is no fallback when a JWT is invalid. The file is bounded to 128 KiB.

The JSON schema is illustrated by `hub/tests/fixtures/oidc-test-only.json`.
The modulus is a placeholder replaced by the offline test provider at runtime.
**Never deploy the test issuer or placeholders.** Obtain public signing keys through
your provider's independently verified administrative channel and configure:

- `issuer`: exact HTTPS issuer string, including any path/trailing slash;
- `audience`: the distinct resource identifier for the Agenct API, not a browser
  client ID;
- `required_scope`: one explicit API grant, for example `agenct:access`;
- `subjects`: explicit provider subject (`sub`) to existing local user ID mapping;
- `jwks.keys`: at most 16 keys, each with exactly `kid`, `kty: RSA`,
  `alg: RS256`, `use: sig`, `n`, and `e: AQAB`. Strip other metadata before
  configuring this narrow schema. Never include private key components.

`AGENCT_USERS_JSON` still defines the existing local authorization roster.
Its development token hashes are ignored for authentication while OIDC is on.
Do not use email, display name, or a caller-supplied user ID as subject mapping.
A subject not explicitly provisioned is denied. Changes require restart.

The verifier enforces RS256 signatures using jsonwebtoken/ring, exact issuer,
API audience, expiration with zero leeway, future `nbf`/`iat` rejection, and a
maximum token lifetime of one hour. `sub`, `iat`, `exp`, `client_id`, and `jti`
are required, along with the configured grant in `scope`. This is an
application-wide grant, not a per-route OAuth scope matrix. The JOSE header must contain only `alg`, `kid`, and `typ`, with
`typ` equal to `at+jwt` or `application/at+jwt`. Unsupported extensions,
embedded keys, key URLs, missing/unknown key IDs, and algorithm confusion fail
closed. JWT input is limited to 16 KiB. Errors do not include token material.

No discovery, JWKS URL fetch, redirects, proxy headers, auto-provisioning,
network access, or trust in arbitrary token-provided issuers is involved.
This deliberately trades convenience for a small, auditable trust boundary.
Operators can overlap old/new public keys for rotation; removing a key and
restarting invalidates its tokens. The application does not refresh keys.

## Local verification

`cargo test --manifest-path hub/Cargo.toml --test oidc --offline`

Tests require OpenSSL on PATH and generate a fresh ephemeral 2048-bit mock
issuer signing key in memory for each test process. No private key fixture is
checked in, written to disk, or printed. Tokens never leave the test process.
They cover valid identity, wrong issuer/audience, expiration, future dates,
unmapped/missing subject, missing required claims, excessive lifetime, bad
signature, wrong key, algorithm/type confusion, token-provided key URL,
configuration failure, and disabled dev-token fallback. Tests contact no IdP.

## Remaining deployment requirements

There is no browser login redirect, PKCE/state/nonce flow, refresh-token storage,
logout/revocation/introspection, automatic key rotation, IdP tenant setup, or
real-provider integration test. No credentials were created or configured.
Current UI manual bearer entry is a local acceptance aid, not a production
session solution. A real deployment still needs an approved provider, TLS,
secure browser session design, permission lifecycle, rate limits, observability,
security review, and explicit deployment authorization. Do not expose this
loopback development server directly to the Internet.
