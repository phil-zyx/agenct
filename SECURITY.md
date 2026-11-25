# Security Policy

## Architecture Security Boundaries (v2)

- **Authentication**: Browser sessions use GitLab OAuth 2.0 with PKCE (`S256`), Host header pinning, `HttpOnly`/`SameSite=Lax` cookies (`__Host-` + `Secure` under HTTPS), Origin + `X-CSRF-Token` verification on all state-changing browser requests, and a compile-time `AuthUser` extractor enforcing active account status.
- **Encryption at Rest**: Setting `AGENCT_MASTER_KEY` (or `AGENCT_MASTER_KEY_FILE`, minimum 32 chars) encrypts persisted per-user model API keys, GitLab PATs, and IM webhook configs in SQLite using AES-256-GCM (`enc:v1:`) with per-user/scope AAD. Use `agenct-hub reencrypt-secrets` offline to encrypt existing plaintext rows or rotate keys (`AGENCT_MASTER_KEY_OLD`).
- **Automation Permissions**: Cross-user auto-accept, auto-delivery, GitLab auto-execution, and GitLab auto-reply are all **disabled by default** and can be revoked live (revocation immediately invalidates queued/running node leases).
- **Execution Isolation**: The Hub never executes untrusted shell commands directly. Code/CLI execution runs on registered `pc` or isolated `cloud` nodes using Protocol 2 exclusive leases. Nodes default to `workspace-write` (never full host access unless `--unsafe-full-access` is explicitly passed).
- **SSRF Protection**: Outbound IM webhooks default to HTTPS official provider domains (`open.feishu.cn`, `open.larksuite.com`, `qyapi.weixin.qq.com`, `oapi.dingtalk.com`) with DNS resolution checks blocking loopback, private RFC1918, CGNAT, and link-local/metadata IPs. GitLab outbound requests are restricted to server-allowlisted origins.
- **Attachments**: Stored as raw binary files with magic-byte validation, zero unbounded in-memory caching, and explicit relational ACL grants (`attachment_grants`).
- **Auditability**: `audit_logs` is append-only (enforced by SQLite triggers).

## Operational Notes

- Deploy behind HTTPS (e.g., Caddy / Nginx TLS) in non-local environments; `AGENCT_ALLOW_HTTP_ORIGIN=1` is intended only for local development or trusted private networks.
- Database files and backup archives contain user messages and integration tokens; restrict filesystem permissions to `0600`/`0700`.
- Do not post tokens, private chats, or vulnerability exploit details in public issues; report security issues privately to the maintainer.
