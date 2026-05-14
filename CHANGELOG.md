### What's changed in v0.24.0

* feat(auth): hops auth bootstrap — seed durable AuthStack secrets in AWS SM (by @patrickleet)

  Adds a top-level `hops auth bootstrap <cluster>` command that generates
  random masterkey + admin-password values and writes them to AWS Secrets
  Manager at `<cluster>/zitadel/{masterkey,admin-password}` (override via
  --prefix). Each value is wrapped as `{ <key>: <random> }` to match what
  the AuthStack composition's ExternalSecrets read via their
  `remoteRef.property` selectors.

  By design the command stops there — it never applies a Kubernetes
  manifest. Installing the AuthStack stays a separate `kubectl apply -f
  local/` (or GitOps controller) step so the declarative boundary isn't
  crossed by CLI automation.

  Idempotent: existing AWS SM values are left alone unless `--force` is
  set. Per-secret tags identify the cluster + that it's an AuthStack
  bootstrap value (for future `hops auth status` / cleanup tooling).

  Implements [[specs/authstack-reconciler]] Phase 3.


See full diff: [v0.23.0...v0.24.0](https://github.com/hops-ops/hops-cli/compare/v0.23.0...v0.24.0)
