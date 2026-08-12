### What's changed in v0.40.0

* feat: local workbench gitops and dory registry host access (#89) (by @patrickleet)

  * feat: local workbench up/down/status/open and Application reconcile

  Add hops local up/down/status/open/gitops with workspace registry,
  helm template apply + labels, env/chart FS watch, host access
  URL planning, and auto source-delivery selection. Unit tests
  cover parse/merge/labels/registry/watch/delivery/net.

  Implements [[tasks/lwb-application-reconcile]] [[tasks/lwb-gitops-watch]]
  [[tasks/lwb-up-down-workspace]] [[tasks/lwb-status-net]]
  [[tasks/lwb-source-delivery]] [[tasks/lwb-docs-happy-path]]

  * fix: wire real delivery sync, host access, and node probe

  - Probe node path visibility via docker/kubectl hostPath (not host is_dir)
  - Sync fallback: tar|kubectl with default_sync_ignores; mutagen when present
  - Host access starts kubefwd or falls back to kubectl port-forward; down stops PIDs
  - Stamp pod-template labels for workspace discovery
  - e2e-ui cluster-dev waits for .hops-synced when sourceDelivery.mode=sync

  Implements residual [[tasks/lwb-source-delivery]] [[tasks/lwb-status-net]]

  * fix: per-app deliveryPath and multi-app tar resync

  Resolve source host roots per Application (deliveryPath / service
  root) so UI mounts ui/ (vite package.json) while API mounts the
  Cargo workspace — not a single monorepo root for every pod.

  Fix continuous tar watcher to sync_all apps with distinct host paths.
  Keep infinite wait-for-sync in cluster-dev when mode=sync.

  Tests cover deliveryPath resolution and multi-app watch script.

  * feat: local workbench gitops, dory registry host access, source packages

  Add hops local gitops cluster/worktree flows, map-mode host access for
  package registry (127.0.0.1:30500), TLS-aware registry probe, and dory
  registries inject without host bind mounts. Supports config install --path
  dogfood of multi-backend SecretStack on stock dory.

  * feat(local): DNS-only host access and dory kubeconfig fix

  Collapse host access to one path: workspace Services and related
  in-cluster FQDNs via loopback IPs, /etc/hosts, macOS stub DNS, and a
  port-forward supervisor. Prefer dory-config credentials first, and
  plumb HOPS_KUBE_CONTEXT into long-lived kubectl children.

  * feat(local): default source delivery to git worktree root

  Share one delivery host path across apps in a workspace so monorepo
  codegen and local packages stay coherent; each worktree keeps its own
  changes. Explicit deliveryPath remains an override.

  * feat(local): use --name as Kubernetes namespace

  Drop the hops-wt- prefix so workspace namespace and FQDNs match the
  user-provided name (e.g. --name dogfood → dogfood, not hops-wt-dogfood).

  * fix(local): point start bootstrap includes at renamed provider files

* fix(local): isolate workspace --name from Dory --dory-name (#90) (by @patrickleet)

  Preserve chart namespaces for shared identity MRs while stamping only
  app workloads; rename the global Dory flag so dual workspaces cannot
  rewrite the desktop kube/docker context.

* feat(local): kind extraMounts + Dory docker for hostPath spike (#92) (by @patrickleet)

  * feat(local): kind extraMounts + Dory docker for hostPath spike

  Mount $HOME into the kind node so Mac worktrees are node-visible for
  hostPath delivery (LWB proposal kind-dory-hostpath-named-clusters).

  - build_kind_config with extraMounts; auto DOCKER_HOST from ~/.dory/dory.sock
  - shift registry hostPort to 30501 when product dory-k8s holds 30500
  - verify mount after create; unit tests for config

  Live: hops local reset --backend kind on Dory → node sees $HOME.

  * feat(local): doctor hostPath mount report + kind-on-Dory docs

  Productize Wave A residual (LWB-REQ-250/251/261):
  - pure pick_registry_host_port + NodeMountReport for create/doctor
  - doctor Source delivery section: kind projects-root mount visibility
  - happy-path docs: preferred Mac path kind+Dory, reset for mounts

  Live: kind node sees $HOME; doctor reports hostPath capable.

* feat(local): dual providers, named clusters, workspace→cluster bind (#93) (by @patrickleet)

  * feat(local): dual providers, named kind clusters, workspace cluster bind

  Wave B–D residual for local-workbench epic:

  - --cluster-provider/--cp and --docker-provider/--dp with --backend alias
  - providers.json persist; apply_docker_provider_env for dory sock
  - named kind clusters via --cluster-name (context kind-<name>)
  - workspace registry clusterName/kubeContext + sticky rebind policy
  - status/up surface cluster binding; unit tests for pure logic

  Stacked on feat/kind-hostpath-extramounts (PR #92).

  * fix(local): sticky workspace cluster bind and activate on status/down/open

  When --cluster-name is omitted, keep the workspace's bound cluster instead of
  comparing against the process default. Activate bound kube context before
  kubectl in status/down/open. Unit test covers sticky None request.

  * fix(local): activate bound cluster on gitops worktree

  * fix(local): narrower kind extraMount default + raise inotify limits

  Mount $HOME/dev (or HOPS_KIND_EXTRA_MOUNT) instead of entire $HOME to avoid
  kube-proxy EMFILE on large Mac trees. After create, raise node inotify
  max_user_instances/watches so kind stays healthy with hostPath mounts.

  Live proof: hostPath UI on kind-hops Vite ready; host write visible in pod.

  * fix(local): rebind refreshes kube context; narrower kind mounts

  --rebind-cluster updates kubeContext even when cluster name is unchanged
  (stale dory → kind-hops). Keep $HOME/dev mount default + inotify sysctl.

  * feat(local): add cluster and worktree GitOps commands

* fix: productionize local workbench stack (#94) (by @patrickleet)

  * fix: productionize local workbench stack

  * fix: canonicalize DNS runtime exclusions

  Implements [[fix/lwb-productionize-stack-top]]

  * fix(ci): wait for declared smoke configuration


See full diff: [v0.39.0...v0.40.0](https://github.com/hops-ops/hops-cli/compare/v0.39.0...v0.40.0)
