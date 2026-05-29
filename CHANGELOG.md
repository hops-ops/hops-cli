### What's changed in v0.29.0

* feat(local): per-provider DRCs, `hops local doctor`, and global --context (#53) (by @patrickleet)

  Split the shared `local-dev` DeploymentRuntimeConfig into per-provider DRCs
  (local-dev-kubernetes, local-dev-helm), each with its own cluster-admin
  ServiceAccount + ClusterRoleBinding, and point each provider's runtimeConfigRef
  at its own DRC. A shared DRC let one provider's runtime image/SA silently
  clobber the other's pod.

  Add `hops local doctor`: verifies what `hops local start` set up — crossplane,
  both providers (installed / healthy / runtimeConfigRef pinned to its own DRC /
  DRC present / cluster-admin binding / ProviderConfig) and the registry — and
  reports drift with a non-zero exit + remediation. Catches a provider whose
  runtimeConfigRef reverted to `default`, dropping its cluster-admin SA (which
  breaks observing XRs through the in-cluster ProviderConfig).

  Add a global `--context` flag to `hops local` so every subcommand can target a
  context (e.g. `hops local aws --refresh --profile hops --context colima`), given
  before or after the subcommand. Plumbs through HOPS_KUBE_CONTEXT_ENV like
  config/provider install.

  Co-authored-by: Claude Opus 4.8 (1M context) <noreply@anthropic.com>


See full diff: [v0.28.0...v0.29.0](https://github.com/hops-ops/hops-cli/compare/v0.28.0...v0.29.0)
