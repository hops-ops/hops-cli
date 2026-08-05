### What's changed in v0.38.0

* feat: cluster backend abstraction (colima + kind + dory) (#70) (by @patrickleet)

  * refactor: extract local cluster backend seam (colima-only)

  Move all colima-specific operations behind a Backend enum in
  src/commands/local/backend/ and rename ColimaSizeArgs to SizeArgs.
  Registry wiring (hosts sync) now flows through backend::wire_local_registry
  so provider/config installs stay backend-agnostic. No behavior change:
  identical command invocations on the colima path.

  Implements [[tasks/cluster-backend-abstraction]] (phase 1)

  * feat: kind backend for hops local

  Adds --backend <colima|kind> (global on hops local) with resolution order
  flag > persisted (~/.hops/local/backend) > detected cluster (colima wins
  for back-compat) > platform default (macOS colima, else kind).

  kind backend: create with a pinned 127.0.0.1:30500 port mapping for the
  in-cluster registry NodePort, docker start/stop of the node container for
  resume, destroy/reset via kind delete + recreate, and containerd certs.d
  hosts.toml aliases (both registry names -> registry ClusterIP over HTTP)
  written idempotently after the registry deploys. Sizing flags error on
  kind. HOPS_KUBE_CONTEXT is auto-set from the backend when --context is
  absent. Preflight enforces kind >= 0.27 (certs.d config_path default).

  Implements [[tasks/cluster-backend-abstraction]] (phase 2)

  * ci+docs: kind smoke workflow and backend docs

  GH Actions workflow exercises the CI acceptance path on ubuntu-latest:
  hops local start --backend kind, doctor, a registry round-trip through
  both pull names (Service name and localhost:30500), the stop/start
  resume path via the persisted backend, and destroy. README documents
  backend selection, persistence/resolution order, and the dory-via-kind
  recipe.

  Implements [[tasks/cluster-backend-abstraction]] (phases 3-4)

  * feat: native dory backend for hops local (#73)

  * feat: native dory backend for hops local

  Backend::Dory drives dory's built-in k3s headlessly via the dory CLI
  (k8s enable/disable/status) and the engine docker socket:

  - start: writes ~/.dory/k8s/registries.yaml (k3s-native trust aliasing both
    registry pull names to the Service hostname over HTTP; read at boot via
    dory's bind mount), then 'dory k8s enable --publish 30500:30500'. The Dory
    app's port forwarder makes the published NodePort host-reachable.
  - wire_registry: syncs hostname -> ClusterIP in the node's /etc/hosts per
    start (in-place rewrite — /etc/hosts is a bind mount, sed -i fails).
  - stop/resume via docker stop/start of dory-k8s; destroy/reset via
    dory k8s disable (+ enable); sizing flags error (VM sized in the app).
  - kube_context 'dory' (dory names its kubeconfig context that); hops
    prepends ~/.kube/dory-config to KUBECONFIG for its kubectl/helm children.
  - detection order: colima > kind > dory.

  Also: 'helm repo update crossplane-stable' instead of bare update — a stale
  unrelated repo in the user's helm config no longer breaks start.

  Verified end-to-end locally: start (Crossplane 2.3.3 + providers + registry),
  doctor all green, registry round-trip through BOTH pull names to pod Ready,
  stop/start resume, doctor green again.

  Implements [[tasks/dory-native-backend]]

  * fix: harden dory integration contract (#74)

  * fix: harden dory integration contract

  [[tasks/rr-2-dory-contract]]

  * fix: skip KUBECONFIG plumbing when dory context is already merged

  Current dory merges the dory context into ~/.kube/config at enable time,
  so hops no longer needs to mutate KUBECONFIG for child processes; the
  side-file prepend remains as a fallback for pre-merge dory versions.

  Implements [[tasks/dory-kubeconfig-merge]]

  * fix: hold dory cluster through the app's engine provisioning window

  Launching Dory.app re-provisions its engine for ~90s and restarts dockerd
  in the VM at the end, SIGTERMing every container — a k3s node enabled
  during that window reports Ready and then dies mid-bootstrap. The engine
  socket's mtime marks the session start, so when it is younger than 180s,
  start/reset now watch the node until the window passes and re-enable it
  (up to 3x) if the engine restart takes it down. Steady-state starts pay
  one container inspect.

  Implements [[tasks/rr-2-dory-contract]]

  * fix: unify kube context/backend targeting (#75)

  [[tasks/rr-1-context-targeting]]

  * ci: run kind smoke and quality for all PR base branches

  Match main's on-pr-quality trigger so stacked PRs into this
  branch also get kind-smoke and quality.

  * ci: macOS colima backend smoke workflow (#76)

  * ci: add macOS colima backend smoke workflow

  Mirror kind smoke on macos-15-intel with nested-virt pin,
  --backend colima, and kubectl --context colima.

  Implements [[tasks/hops-cli-colima-macos-smoke]] under
  [[tasks/hops-cli-macos-backend-smoke]].

  * test: structural checks for colima macOS smoke workflow

  Assert the shipped on-pr-colima-smoke.yaml pins macos-15-intel and
  mirrors kind smoke without a full GHA run.

  * ci: run colima smoke for all PR base branches

  Drop pull_request.branches: [main] so this job runs on stacked
  PRs into feat/cluster-backend-abstraction.

  * ci: size colima smoke VM for macos-15-intel runners

  Hops defaults (8 CPU / 16 GiB / 60 GiB) exceed standard GHA intel
  runners (~4 / ~14), so VZ hostagent exits during start. Pass explicit
  smaller sizes and dump lima ha.stderr on failure.

  * ci: give colima smoke enough memory for Crossplane

  2 CPU / 4 GiB booted Colima but helm --wait for Crossplane hit
  5m with Available: 0/1. Use 3/8 on macos-15-intel (~14 GiB host)
  and raise Crossplane helm --wait timeout to 10m for nested virt.

  * fix: wait longer for Crossplane on nested-virt colima

  Helm --wait hit 10m with Available:0/1 on GHA macos-15-intel. Apply
  the chart without --wait, poll deployments for ~15m with periodic
  pod dumps, wait for nodes Ready after docker restart, and capture
  cluster state before destroy on CI failure.

  * fix: harden kubectl apply under nested-virt API load

  Crossplane came Ready on GHA colima but ProviderConfigs apply failed
  with openapi schema download timeouts. Apply with --validate=false,
  retry transient failures, and re-wait for the API after Crossplane
  becomes leader. Soft-wait rbac-manager when core is healthy.

  * fix: wait for Established CRDs and room for registry PVC

  ProviderConfig apply raced CRD discovery; wait for Established.
  Registry PVC requests 20Gi — give colima smoke a 40Gi VM disk so
  local-path can bind, and wait longer with diagnostics for registry.

  * ci: recover colima stop/start resume after VM container churn

  After colima stop/start, pods Error on missing docker containers.
  Wait longer for Available and rollout-restart crossplane/registry
  if the first wait stalls.

  * fix: survive apiserver overload when applying registry

  Nested-virt colima CI loses the apiserver (TLS timeouts) right after
  Crossplane/provider install. Wait longer for /readyz, retry kubectl
  apply with API re-probes, and pre-pull registry:2 before deploy.

  * fix(ci): give colima smoke more RAM and settle time for registry pulls

  At 3/8/40, dual-name registry smoke pods sat in ContainerCreating without
  IPs while CoreDNS and metrics-server thrashed. Bump VM memory to 10Gi,
  wait for node/CoreDNS before the push, extend Ready timeout, and dump
  smoke pod describe on failure.

  * ci: macOS dory backend smoke spike workflow (#77)

  * ci: add macOS dory backend smoke spike workflow

  Clone+build patrickleet/dory @ feat/hops-local-integration in CI,
  wait for engine.sock, then kind-parity hops smoke with --backend dory.
  workflow_dispatch / non-required until public GHA can boot the engine.

  Implements [[tasks/hops-cli-dory-macos-smoke]] under
  [[tasks/hops-cli-macos-backend-smoke]].

  * test: structural checks for dory macOS smoke workflow

  Assert clone+build install contract and kind-parity steps in the
  shipped on-pr-dory-smoke.yaml.

  * fix(ci): port colima nested-virt lessons into dory smoke

  Bring shared start hardness (CRD Established waits, apply retries, longer
  Crossplane/registry waits) from the green colima path, and harden the
  dory workflow: settle CoreDNS before registry pods, 420s Ready, stop/start
  rollout restart, failure dump, and ECR pull retries. Structural tests lock
  the contract in.

  * ci: run dory smoke on pull_request so PR branches can execute it

  workflow_dispatch only works once the file exists on the default branch;
  enable pull_request (still non-required) so #77 can actually run dory-smoke.

  * fix(ci): colima-backed engine.sock when dory-hv cannot run on GHA

  Public GHA cannot run dory-hv (no nested virt for native engine; app never
  creates ~/.dory). Pin macos-15-intel, still clone+build dory for scripts/,
  try the app briefly, then expose Colima docker as ~/.dory/engine.sock so
  hops local --backend dory can complete kind-parity smoke.

  * fix(local): recreate dory k8s on create-time config drift

  dory enable exits 3 when ports/registry binds drift. hops just wrote the
  desired config; auto-retry once with --recreate so local start applies it.
  CI: skip native engine wait on Intel, set DORY_ENGINE_SOCK, disable k8s first.

  * fix(local): retry helm install when apiserver openapi is still soft

  After dory stop/start, nodes can be Ready while openapi/v2 still times out
  and helm upgrade fails. Retry helm with wait_for_kubernetes between attempts;
  CI pause briefly after stop before start.

  * fix(local): recreate dory k8s when enable fails after stop/start

  docker stop of dory-k8s often leaves k3s stuck past dory's Ready wait on
  resume. On enable exit 1/3, disable and re-enable with --recreate once.

  * ci: gate macOS backend smoke with labels

  * fix(ci): build Dory FFI before xcodebuild (#81)

  Dory's rebased upstream requires a generated Rust UniFFI XCFramework before SwiftPM can resolve the app package. Install rust/protobuf, materialize and verify the ignored artifacts, then build the app.\n\nImplements [[tasks/hops-cli-dory-macos-smoke]]

  * fix(local): wait for provider Healthy and pin dory smoke SHA

  Doctor requires Healthy=True, but start only waited for CRDs, so
  kind-smoke could race on cold CI. Poll both bootstrap providers after
  ProviderConfigs. Pin dory smoke to a known-good commit of the hops
  integration work so rebases do not move CI without an intentional bump.

  * feat(local): stock Dory integration + self-hosted smoke (#84)

  * refactor(local): dory engine package bridge, no NodePort registry

  Rebuild the dory backend around product k3s + an engine-side registry:
  host push localhost:30500, cluster pull host.dory.internal:30500. Drop
  ports/registries create-time files and in-cluster NodePort on dory.
  Crossplane still installs the same; package pull address is backend-aware.

  * refactor(local): stock Dory only — no k8s enable fork

  hops no longer calls dory k8s enable/disable or requires a scriptable
  CLI. Users enable Kubernetes in the Dory app; hops starts a stopped
  dory-k8s node, waits for Ready, and uses the engine package bridge.
  Destroy/reset only remove the container via the engine docker socket.

  * feat(local): stock Dory backend, self-hosted smoke, path config fixture

  Use product Dory only (dory.sock, hops-dory desktop, TLS registry push via
  engine bridge). Add HOPS_DORY_DESKTOP=0 for env-only sessions that do not
  mutate host kube/docker defaults.

  Rewrite dory CI for self-hosted Apple Silicon (label hops-dory / test-dory):
  no fork build, no Colima sock surrogate, soft cleanup only.

  Add tests/fixtures/config-smoke and path-based hops config install in smoke
  (namespaced ConfigMap under hops-ci).

  * fix(config-install): pass --platform when rebuilding arch-tagged render images

  Avoid Docker buildx InvalidBaseImagePlatform when `up project build` emits
  both :amd64 and :arm64 function images and hops rebuilds the non-host arch
  on Apple Silicon (or the reverse on Intel).

  * fix(ci): single-platform busybox for dory registry smoke push

  Pull with --platform for the host arch and re-materialize via docker build
  so push does not warn about incomplete multiplatform content from the
  upstream busybox:stable index.

  * feat(local): fast start when control plane healthy; kind config smoke

  Skip helm repo update/upgrade and bootstrap reapply when Crossplane,
  k8s+helm providers, and the package registry are already Available.
  Pass --bootstrap to force a full reconcile.

  Kind smoke: drop stop/start resume; install up CLI and run the same
  path-based config-smoke fixture as dory (namespaced ConfigMap).

  * fix(local): expect HTTPS in kind hosts.toml registry unit test

  The local package registry serves TLS; hosts.toml uses https:// + skip_verify.
  Update the unit test that still asserted http:// after the TLS migration.

  * ci(dory): drop failure debug dump on personal Mac runner

  Avoid dumping host-wide docker/pods/doctor state that includes unrelated
  workloads on a shared self-hosted machine.

  * fix(local/ci): fast dory doctor probe; drop rust-cache on self-hosted smoke

  Doctor: for non-loopback push hosts (dory bridge IP) skip host curl and probe
  via the engine docker network only — host curl hangs on TCP timeout.

  Dory smoke: remove Swatinem/rust-cache; post-job cache save is slow and can
  block on password with no TTY on a personal Mac runner.

  * fix(local/ci): skip host curl for dory doctor; drop rust-cache hang

  Doctor probes non-loopback push hosts only via engine docker (explicit
  ~/.dory/dory.sock), avoiding ~75s host TCP timeout and host docker password
  prompts. Remove Swatinem/rust-cache from dory smoke so post-job target/
  upload no longer hangs the personal Mac runner.

  * ci(dory): resolve working cargo via rustup toolchains

  Self-hosted Mac runners often have a dangling ~/.cargo/bin/cargo → rustup
  shim. Prefer a real toolchain cargo so preflight and build succeed.

  * ci(dory): quiet env-session step logs on personal Mac runner

  Drop set -x and host dumps (full PATH, docker info, readiness) so Actions
  logs only show short status lines for the Dory preflight.

  * fix(ci): install up CLI with correct binary URL and PATH

  The shell installer only downloads to CWD, the fallback URL was missing
  /bin/, and GITHUB_PATH does not affect the current step.

  * fix(ci): retry ECR busybox pull in kind smoke

  public.ecr.aws rate-limits anonymous pulls; mirror dory smoke's backoff.


See full diff: [v0.37.1...v0.38.0](https://github.com/hops-ops/hops-cli/compare/v0.37.1...v0.38.0)
