> 历史专题记录：保留设计背景、协议细节和当时的验收结果；其中“当前/尚未”描述可能早于最新实现。现行用法以[入门](../getting-started.md)和[参考](../reference.md)为准。

# GitLab transport (library only)

`hub::gitlab::GitLabClient` implements the publication transport contract using the
system `/usr/bin/curl` HTTPS client. It is not enabled by the Hub, executor, CLI or
staging launcher. No real GitLab call or token is needed by its tests.

## Trusted deployment boundary

The operator supplies one exact HTTPS origin and a nonempty allowlist of numeric
project IDs when constructing the client. Never populate these from model output,
chat links, worker files, or untrusted request parameters. A non-default HTTPS
port is allowed only when explicitly part of that operator origin. Self-hosted
instances are supported through their host's normal trusted CA installation;
there is no insecure TLS option or custom CA input in this client.

The private HTTP implementation disables curl configuration files and inherited
proxy environment, uses HTTPS only and TLS 1.2 or newer with standard certificate
and hostname verification, does not follow redirects, and sets 5-second connect
and 20-second request deadlines. Responses are capped at 1 MiB, including streamed
responses. The secret is sent through stdin configuration, never argv. Transport
errors exclude server bodies and credentials. Pagination is capped at ten pages
of 100 items and fails closed at the cap. There is no automatic retry, including
for POST. A failed POST remains an uncertain outcome in the publication journal.

## Credentials and permissions

Create credentials yourself through the service's secure administration workflow.
Prefer an expiring GitLab project access token scoped to the specific project,
with `api` permission and the lowest project role that can create draft MRs and
read branches, MR pipelines and jobs on that instance. GitLab's `api` scope is
broader than this adapter's operations: token theft can allow other project
writes, so do not treat this adapter as a restriction on the token itself.
Validate role requirements against your GitLab deployment before provisioning.
Use separate tokens/clients for separate projects, rotate and revoke explicitly,
and keep secrets exclusively in the trusted publisher process. Never place them
in a browser, Hub chat, repository, worker environment, model prompt or logs.
The client does not create tokens, save them, request broader permissions, or
perform any authentication setup.

## Controlled operations

The only implemented operations are branch-head reads, creating a **draft** MR,
reconciling a unique exact intent marker, and checking the latest MR pipeline's
successful jobs on the exact candidate SHA. Numeric IDs and URL-encoded path
segments are used; remote URLs are validated but never fetched. Reconciliation
requires an opened draft with matching project, branches and head. CI evidence
is passed to `publication::verify`, which enforces the required job-name set and
rejects failures, duplicates and SHA mismatches. No merge, MR approval, branch
push, source upload or project/settings endpoint exists.

Only invoke through the controlled publication layer after independent candidate
evidence and the owner's exact publication grant are established. Calling a raw
transport method is not authorization. In particular, this library must remain
unwired while executor evidence is not independently trustworthy. The source
branch must already exist at the approved candidate SHA via a separately
authorized process. Branches can race after reads; this creates a draft for human
review, never an automatic merge or task completion.

## Validation status

Fake HTTP tests cover origin/project denials, branch encoding, draft-only body,
remote binding rejection, exact/ambiguous reconciliation, no retry after timeout,
and exact-SHA pipeline/job reads. Additional subprocess tests run a controlled
fake curl executable through the real piping/serialization path. They verify
exact TLS/redirect/proxy/timeout arguments, stdin-only secrets, escaped request
configuration, sanitized certificate/timeout/HTTP failures, malformed output,
and termination of an unbounded streamed response. They do not establish a live
deployment works or perform a real TLS handshake.
No real credential, API call, branch publication, live TLS handshake or GitLab CI
run has been performed. System curl with HTTPS support and a trusted CA store is
an explicit deployment prerequisite. This is an integration building block, not
a claim of end-to-end production readiness.
