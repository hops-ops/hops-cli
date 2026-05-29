### What's changed in v0.30.0

* feat(local): label `hops provider install` resources so doctor knows they're intentional (#54) (by @patrickleet)

  `hops provider install` now stamps `app.kubernetes.io/managed-by: hops-provider-install`
  on the Provider, DeploymentRuntimeConfig, and ClusterRoleBinding it manages. A
  custom install (e.g. a forked provider built from source) uses the
  `<provider>-runtime` DRC convention rather than the bootstrap `local-dev-<provider>`
  DRC, which `hops local doctor` previously reported as drift.

  `hops local doctor` now reads that label: a labeled provider is checked against the
  install convention (`<provider>-runtime` DRC, `<provider>-cluster-admin` binding,
  SA `<provider>`) and reported as a "custom install" with its package, instead of
  false drift — while still verifying cluster-admin and still flagging real drift
  (runtimeConfigRef=default / missing cluster-admin) in both modes.

  Co-authored-by: Claude Opus 4.8 (1M context) <noreply@anthropic.com>


See full diff: [v0.29.0...v0.30.0](https://github.com/hops-ops/hops-cli/compare/v0.29.0...v0.30.0)
