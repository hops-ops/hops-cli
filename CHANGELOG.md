### What's changed in v0.52.0

* chore(deps): update rust crate ureq to v3.4.2 (by @renovate[bot])

  Co-authored-by: renovate[bot] <29139614+renovate[bot]@users.noreply.github.com>

* chore(deps): update rust crate clap to v4.6.7 (by @renovate[bot])

  Co-authored-by: renovate[bot] <29139614+renovate[bot]@users.noreply.github.com>

* chore(deps): update docker/build-push-action digest to c3c9e26 (by @renovate[bot])

  Co-authored-by: renovate[bot] <29139614+renovate[bot]@users.noreply.github.com>

* chore(deps): update docker/setup-buildx-action digest to f87e599 (by @renovate[bot])

  Co-authored-by: renovate[bot] <29139614+renovate[bot]@users.noreply.github.com>

* feat(local): one machine cluster with overlay Environments (#126) (by @patrickleet)

  * feat: add remote package development foundations

  Implements the registry and transport foundations for tasks/hops-remote-workbench-1. Remote CLI activation, restore, local migration, and Kubernetes integration remain pending.

  * style: format native registry protocol test

  * feat(local): one machine cluster up/init/env/tui

  Add hops local up to create or reconnect a single machine Cluster,
  init writers for cluster/platform/environment files, env catalog
  (off by default), and tui as a view over the same enable engine.

  Leaf Cluster.metadata.name no longer creates a second kind cluster.
  --cluster-name remains a warned escape hatch.

  Implements [[tasks/lwb-shared-machine-cluster-epic]]

  * fix(local): unique env catalog ids and \$HOME mountRoot

  Discover walks .worktrees, keys catalog files by source path, and
  enables by runtime name so Harmony/Forge worktrees do not clobber
  each other. Cluster.spec.mountRoot may be \$HOME.

  * feat(local): embed standard cluster template and overlay on up

  Materialize Crossplane helm/k8s providers, PCs, and the Harmony cluster
  tree (minus shared/) from the CLI into $HOME/.gitops/local. Project
  .gitops/local/cluster extras overlay; shared/ is never Cluster-owned.

  * fix(local): keep AuthStack on cluster, drop Harmony identity embed

  Zitadel provider, ProviderConfig, and AuthStack stay in the CLI cluster
  template so a local CP has a working install. Harmony personas, project,
  SMTP, and machine users are product identity, not cluster tooling.

  * fix(local): AuthStack Service zitadel in namespace auth

  The zitadel chart doubles the name when the Helm release already contains
  \"zitadel\". fullnameOverride pins the API Service to zitadel so DNS is
  zitadel.auth.svc.cluster.local; login stays zitadel-login.

  * fix(local): do not crawl \$HOME for Environments on up

  Cluster mountRoot is \$HOME for hostPath. Discovering environment.yaml
  under that tree hits macOS PermissionDenied and would auto-enable every
  checkout. Reconcile only catalog-enabled Environments and never watch
  \$HOME recursively.

  * fix(local): resolve extra Environment yamls from checkout root

  harmony-system.yaml lives next to environment.yaml. Checkout root is
  still the repo, not .gitops/local, so deploys .gitops/local/harmony-system
  do not double the path.

  * fix(local): apply k8s env docs as JSON

  serde_yaml round-trips yes/on/no as YAML 1.1 booleans; kubectl then
  rejects container args. JSON keeps them strings.

  * feat(local): hops local status --urls

  Print HTTPRoute *.localhost URLs only. Default status skips missing kube
  contexts; --all restores the stale-workspace dump.

  * fix(local): status lists only running workspaces

  Skip empty and dead-context records unless --all. Default output is
  name, URLs, and not-ready pods.

  * feat(local): rename tui/dns and compact env + cluster status

  hops local envs lists hostPath-relative worktrees (bold if enabled).
  hops local fwd is the Service port-forward command. status opens with
  cluster node, AuthStack, Configuration, and Provider versions.

  * fix(local): drop tui and dns command aliases

  * feat(local): show cluster hostPath on status

  Drop tests that only assert old command names are absent.

  * feat(local): configure hostPath on init and via hops local configure

  Prompt for the kind extraMount directory (default ~/dev). Persist it in
  ~/.hops/local/cluster.json and the machine Cluster yaml. --set hostPath
  confirms then recreates the kind cluster.

  * fix(local): AuthStack first org is hops, not harmony-local

  The instance bootstrap org is cluster-owned. Product orgs (GitKB) belong
  on cluster-scoped Environments via provider-upjet-zitadel.

  * feat(local): Environment.spec.secretSync before deploys

  Cluster-scoped envs can push ignored secrets/vault into the machine
  Vault. Port-forward uses HOPS_KUBE_CONTEXT so Harmony hops.yaml
  kind-harmony does not steal the sync.

  * fix(local): vault secretSync uses the Environment checkout

  hops.yaml vault.path is relative to cwd. Enable from another repo
  looked for hops/secrets/vault. Chdir to the secret tree's Git root
  so Harmony secrets/vault is the naming root.

  * fix(local): secretSync reads Vault root token from vault-0

  Workbench vault sync does not require VAULT_TOKEN in the shell. It
  execs /vault/data/.hops-init like Harmony's make vault-sync script.

  * fix(local): register auth.gitkb.localhost on cluster ingress

  Default browserIngress to ns auth. Treat completed Job spec mutations
  as non-fatal so env reconcile can finish and Dory can bind the domain.

  * fix(local): ignore immutable Job applies during env reconcile

  * feat(local): Environment.spec.setup runs on env enable

  Checkout-relative scripts run once before secretSync/deploys. Watch
  does not re-run them.

  * fix(local): adopt ureq 3 API in package_dev registry client

  Main moved to ureq 3.4; AgentBuilder/.set/Error::Status no longer compile
  on the merge. Keep loopback-only, no-proxy, no-redirect registry calls.

  * fix(local): keep hops local up as a first-class command

  Main still treats up as a removed interim subcommand. This branch owns
  machine-cluster up; only open and stop stay rejected.

  * fix(local): align status and fwd tests with machine-cluster UX

  Main still asserts hops local dns and the old status card. Status is
  compact now and the command is fwd.


See full diff: [v0.51.1...v0.52.0](https://github.com/hops-ops/hops-cli/compare/v0.51.1...v0.52.0)
