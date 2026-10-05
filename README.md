# cove-tools

Small web apps written in [Cove](https://github.com/myuon/cove), and the host
that runs them on one machine.

- **`crates/cove-host`** — a self-hosted function host: loads `apps/<name>/`,
  checks, prepares and compiles each app once, and runs every request in its
  own Cove isolate on a shared worker pool (issue #1).
- **Apps** (planned): a webhook lab (#2), a benchmark ledger (#3), and an
  algorithm playground (#4).

The host is Rust; the apps are Cove. Cove itself is a git dependency pinned to
a commit, and changes the compiler or runtime needs go back to
[myuon/cove](https://github.com/myuon/cove).

Same-process isolation between apps is a fault and resource boundary for code
you run yourself. It is not a security guarantee against untrusted code.

## Status

Skeleton only. See the issues for the plan.
