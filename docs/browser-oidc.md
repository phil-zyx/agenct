# Opt-in browser OIDC sessions

This is a locally tested browser-login foundation, not a production deployment.
It uses `openidconnect`/`oauth2`, Rustls-backed Reqwest, and `cookie` rather than
implementing OAuth cryptography. No real identity provider, credentials, client
registration, TLS ingress, or external service has been configured.

## Explicit configuration

Set `AGENCT_BROWSER_OIDC_CONFIG_FILE` to an operator-controlled JSON file
(maximum 128 KiB). It is mutually exclusive with `AGENCT_OIDC_CONFIG_FILE`.
If neither is set, the existing development bearer login is unchanged.
The browser mode disables all development and API bearer-token authentication.
Invalid configuration fails startup; it does not fall back.

Fields:

- `issuer`: exact HTTPS issuer.
- `authorization_endpoint` and `token_endpoint`: explicit HTTPS endpoints on
  the same origin as the issuer. No URL credentials, query, or fragment.
- `client_id`: a registered **public client** supporting PKCE S256 and token
  endpoint authentication method `none`. This implementation has no client
  secret configuration and does not request refresh tokens.
- `public_origin`: canonical HTTPS origin, with no trailing slash/path, for
  example `https://agenct.example.org`.
- `subjects`: exact provider `sub` to existing local user ID mapping. Email and
  names are never authorization identities; no automatic user provisioning.
- `jwks`: operator-pinned RSA public-key set using the narrow schema described
  in [oidc-auth.md](oidc-auth.md). No private keys or dynamic discovery.

Register exactly `<public_origin>/auth/callback`. There is no configurable
return URL or redirect from user input. The Hub still only listens on loopback
HTTP. An independently configured HTTPS reverse proxy must preserve the exact
public Host header and serve the whole origin from this Hub. Forwarded host,
proto and identity headers are ignored. Do not expose the loopback backend or
weaken Secure cookies to make plain HTTP work. HTTP-only private staging is not
a browser-login acceptance environment.

## Security boundary

- Authorization code + PKCE S256, cryptographic state and nonce.
- State is bound to a short-lived, `__Host-` browser cookie. Callbacks are
  single-use, expire after five minutes, and cannot select their issuer or
  token endpoint. New login/logout invalidates the old login binding. An
  authenticated browser must log out before starting another login.
- ID-token signature, exact issuer, client audience, nonce, authorized party,
  expiry and future issuance are checked. Only RS256 is accepted. Optional
  access-token hash is verified. Only explicitly mapped subjects can log in.
- Token requests require TLS validation, prohibit redirects/proxies, have a
  15-second deadline and a 128-KiB streamed response cap. Tokens never go to
  browser JavaScript, URLs, logs or storage. Returned access/refresh/ID tokens
  are discarded after establishing the local session.
- Sessions are server-side memory only, expire at the earlier of ID-token expiry
  or 15 minutes, and disappear on restart. Cookies have `Secure`, `HttpOnly`,
  `SameSite=Lax`, `Path=/`, and no Domain. No token is stored in localStorage or
  sessionStorage. Logout deletes the server session and clears its cookie.
- Authenticated API reads require a page-bound CSRF value too, so an old tab
  cannot silently use a changed browser identity. Mutations additionally require
  the exact Origin. Duplicate CSRF/Origin headers and ambiguous cookies fail.
- Pending logins and sessions are bounded to 1024 entries each. No automatic
  retry of code exchange or writes. Responses use no-store/no-referrer; page
  scripts/styles are same-origin and frames are blocked by CSP.

## Offline acceptance

`cargo test --manifest-path hub/Cargo.toml browser_auth::tests --offline`

Tests generate ephemeral keys/certificates at runtime using local OpenSSL and
use an HTTPS mock provider with an explicitly trusted test certificate. The
normal production client rejects that certificate. The mock enforces PKCE,
registered callback/client ID and single-use codes. No live IdP calls and no
checked-in private-key fixtures are needed. Run the browser client regressions
with `node hub/web/client.test.cjs` as well.

## Remaining work before production

The implementation deliberately omits refresh tokens, federated logout,
back-channel revocation, automatic JWKS rotation, distributed/persistent
sessions and automated identity lifecycle management. Logout is local, not an
IdP logout. Operator key/subject changes require restart. Production still
requires approved IdP registration, HTTPS ingress, live browser/provider
acceptance, infrastructure rate limiting, monitoring and independent security
review. Offline tests cannot prove a real provider or deployment is configured
correctly.
