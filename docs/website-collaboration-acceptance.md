# Website collaboration scenario acceptance

`npm run test:website-collaboration` runs three isolated browser identities against
one real temporary Rust Hub and SQLite database. Build the current Hub first
(`cargo build --locked --manifest-path hub/Cargo.toml`); install the pinned npm
packages and Playwright Chromium. `AGENCT_TEST_HUB_BINARY` can select the built
binary. This scenario is included in the required Hub browser CI suite.

The scenario follows one temporary website project:

1. Alice explicitly publishes the three-page scope. Bob asks about mobile and
   the contact form; Alice and Carol explicitly publish their clarifications.
2. Bob and Carol each choose the project for one private **Mock** discussion.
   Their Agent context contains the published replies and version/provenance,
   not Carol's private chat. Discussion changes no shared state and does not
   message another member.
3. Alice confirms the scope and no-deployment decision. Bob and Carol publish
   their own proposed assignments, then each confirms their own exact version.
   Alice cannot confirm Bob's assignment for him.
4. Bob explicitly reports a missing contact address. Alice publishes a test
   placeholder in the shared question thread. Bob privately discusses the
   reply, then explicitly withdraws the blocker report. Withdrawal is historical
   retirement, **not** a new automatic “resolved” or team-consensus status.
5. Alice prepares a private task and explicitly delegates it to Bob. Both
   participants consent to exposing only its coordination label/progress to the
   project. Bob explicitly accepts, receives a private Mock draft, edits the
   delivery, and manually delivers. Alice explicitly accepts the delivery.
6. Carol sees the shared task projection but cannot read the original private
   task or drafts. Reload retains project state. A narrow screen has no page
   overflow. Departed membership and an old revision reject subsequent project
   discussions.

Repeated-click checks verify one publication; previews do not publish; question
publication requires its acknowledgement checkbox. Existing focused browser
suites additionally exercise response loss, interrupted previews, withdrawal,
newer navigation, stale question versions, account switches, and replay recovery.

## What passing means

The acceptance proves integration of real authorization, persistence, browser
UI and human-confirmed transitions with a deterministic Mock model. It does not
prove live-model reasoning, real Agent negotiation, human usability on separate
computers, a built website, code execution, WSL startup, email delivery, payment,
or deployment. The delivery in this scenario is explicitly a **Mock website
acceptance checklist**, not working website source code. No provider credentials
or external recipients are used. Screenshots are written outside the repository
(or to the caller's explicit artifact directory); tokens are never printed.
