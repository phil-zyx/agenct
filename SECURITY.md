# Security Policy

## Architecture Security Boundaries (v2)

- **Authentication**: Browser sessions use GitLab OAuth 2.0 with PKCE (`S256`), Host header pinning, `HttpOnly`/`SameSite=Lax` cookies (`__Host-` + `Secure` under HTTPS), Origin + `X-CSRF-Token` verification on all state-changing browser requests, a sliding-window failure rate limiter on `/auth/local`, full-site defense-in-depth headers (`X-Frame-Options: DENY`, `Permissions-Policy`, HSTS under HTTPS), and a compile-time `AuthUser` extractor enforcing active account status.
- **Encryption at Rest**: Setting `AGENCT_MASTER_KEY` (or `AGENCT_MASTER_KEY_FILE`, minimum 32 chars) encrypts persisted per-user model API keys, GitLab PATs, and IM webhook configs in SQLite using AES-256-GCM (`enc:v1:`) with per-user/scope AAD. Use `agenct-hub reencrypt-secrets` offline to encrypt existing plaintext rows or rotate keys (`AGENCT_MASTER_KEY_OLD`).
- **Automation Permissions**: Cross-user auto-accept, auto-delivery, GitLab auto-execution, and GitLab auto-reply are all **disabled by default** and can be revoked live (revocation immediately invalidates queued/running node leases).
- **Execution Isolation**: The Hub never executes untrusted shell commands directly. Code/CLI execution runs on registered `pc` or isolated `cloud` nodes using Protocol 2 exclusive leases (with HTTP 3xx redirects disabled on node requests). Nodes default to `workspace-write` (never full host access unless `--unsafe-full-access` is explicitly passed), scrub Hub/node secret env vars (`AGENCT_NODE_TOKEN`, `AGENCT_MASTER_KEY`, etc.) from child processes, block symlink escapes when collecting evidence files, and enforce per-user sliding-window brute-force rate limiting (5 failures / 10 min) + failure audit logging on `/approve` one-shot elevation codes.
- **SSRF Protection**: Outbound IM webhooks default to HTTPS official provider domains (`open.feishu.cn`, `open.larksuite.com`, `qyapi.weixin.qq.com`, `oapi.dingtalk.com`) with DNS resolution checks blocking loopback, private RFC1918, CGNAT, and link-local/metadata IPs. User-configured model endpoints and IM webhooks are additionally guarded at runtime and connection time via `PublicDnsResolver` (eliminating DNS Rebinding TOCTOU). GitLab outbound requests are restricted to server-allowlisted origins.
- **Attachments**: Stored as raw binary files with magic-byte validation, zero unbounded in-memory caching, explicit relational ACL grants (`attachment_grants`), `Content-Disposition: attachment` for non-image files, and `Content-Security-Policy: default-src 'none'; sandbox` on downloads.
- **Auditability**: `audit_logs` is append-only (enforced by SQLite triggers).

## Operational Notes

- Deploy behind HTTPS (e.g., Caddy / Nginx TLS) in non-local environments; `AGENCT_ALLOW_HTTP_ORIGIN=1` is intended only for local development or trusted private networks.
- Database files and backup archives contain user messages and integration tokens; restrict filesystem permissions to `0600`/`0700`.
- Do not post tokens, private chats, or vulnerability exploit details in public issues; report security issues privately to the maintainer.
