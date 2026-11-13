# Security policy

## Reporting a vulnerability

Please report security problems **privately**, not in a public issue,
pull request or discussion:

- On GitHub, open the repository's **Security** tab and choose
  **Report a vulnerability** (GitHub's private vulnerability reporting).
  Only the maintainers see the report.

Include what you can: the version or commit, how to reproduce it (a
statement, a protocol exchange, a damaged file), what an attacker gains,
and whether it is already known elsewhere.

What happens next:

- The report is acknowledged within **7 days**, with a first assessment
  (whether it is a vulnerability, how severe) within **14 days**.
- The fix is developed in a private security advisory; you are asked
  to check it, and credited in the advisory unless you prefer not to be.
- The advisory is published with the release that fixes it — a CVE is
  requested where one applies — and the release notes name it.  The aim
  is a fix within 90 days of the report; if it takes longer, you are
  told why and when.

Please give us that time before disclosing a problem publicly.

## Supported versions

nucleus has not had a release meant for production yet (it is 0.x, and
PLAN.md calls for an independent security audit before one).  Until
then, fixes go to the `main` branch and the latest release only.  Once
there are production releases, this section will name the versions
that receive security fixes.

## Scope

In scope: the server and its front ends (the nucleus protocol `ncp`,
PostgreSQL's wire protocol in `pg-compat`), authentication (SCRAM,
tokens, certificates, OIDC, LDAP), privileges and row-level security,
the audit log, secrets and anything that could reveal them, the storage
engine (a crafted or damaged file must fail with an error, not a crash
or wrong data), `nucleus-crypto`, the drivers, and the modules in this
repository.

Out of scope: weaknesses that need an administrator's credentials to
exploit and stay within an administrator's powers; denial of service
by a logged-in client who is allowed to run expensive queries (limits
exist for that: budgets, `statement_timeout`); and problems in a
deployment's own configuration — unless nucleus's defaults or
documentation lead to them, in which case please report it.

## How nucleus is checked

PLAN.md §M14.29 lists what is in place and what is planned: this
policy; fuzzing of every input that crosses a trust boundary — in
place: `fuzz/` (SQL, value input, text search, `COPY`, both wire
protocols, SCRAM, OIDC tokens, the files on disk, model files), run in
CI on each change, OSS-Fuzz next; an independent security audit; and
checks of the transaction guarantees with Jepsen's tools.
