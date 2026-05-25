### What's changed in v0.28.0

* feat(local): add `hops local listmonk` + make --source-context required (#51) (by @patrickleet)

  Mirrors `hops local zitadel` / `hops local github`. Bootstraps the
  hops-ops/provider-listmonk Crossplane provider + a cluster-scoped
  ProviderConfig pointing at a Listmonk instance via Basic-Auth
  credentials (JSON Secret).

  Credential resolution waterfall:
    1. Explicit --endpoint / --username / --token flags
    2. LISTMONK_{ENDPOINT,USERNAME,TOKEN} env vars
    3. Read from the chart-bootstrapped Secret on a source cluster
       (with keys `username` + `token` — the shape produced by
       listmonk-chart v0.2.0's post-install api-user-bootstrap hook)

  Endpoint is derived from the source Secret name when not explicitly
  set: `<release>-provider-creds` → in-cluster service
  `http://<release>.<source-namespace>.svc.cluster.local:9000`.

  Default upjet provider package: ghcr.io/hops-ops/provider-listmonk:v0.0.3.

  Also drops the `pat-local` default from `--source-context` on BOTH
  this command and `hops local zitadel` — that hardcoded value bakes
  the implementer's personal cluster name into a tool meant for
  multiple users. Required positional flag now; users explicitly pass
  their own source context.

  Verified end-to-end on pat-local 2026-05-25:
  - Provider install + Healthy
  - ProviderConfig applied
  - UserRole MR reconciled (Crossplane → upjet → TF provider →
    Listmonk REST API → users / roles table)
  - User MR reconciled with cross-resource userRoleIdRef → numeric
    userRoleId (typed-reference resolution works end-to-end)
  - AppSettings MR reconciled (no-op write of current values; round-
    trip lossless)


See full diff: [v0.27.0...v0.28.0](https://github.com/hops-ops/hops-cli/compare/v0.27.0...v0.28.0)
