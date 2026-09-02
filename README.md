# hops-cli

`hops-cli` is a Rust CLI for Crossplane development and XR lifecycle workflows.

## Overview

This tool supports four related workflows:

- Importing existing application repositories into the Hops GitOps delivery contract
- Local cluster setup on colima or kind
- Configuration package install/uninstall against the connected cluster
- XR observe/manage/adopt/orphan and cross-control-plane migration workflows

For local development, it can also:

- Install and manage a local cluster backend (colima or kind)
- Start a local k8s cluster with Crossplane installed via Helm
- Install the Kubernetes and Helm Crossplane providers
- Deploy an in-cluster OCI registry (`crossplane-system/registry`)
- Build and publish Crossplane configuration packages from an XRD project
- Run a Kubernetes-shaped GitOps workbench for a project and its worktrees

## Installation

### Using ubi

1. **Install ubi:**  
   Ensure you have ubi installed by running:
   ```bash
   curl --silent --location \
    https://raw.githubusercontent.com/houseabsolute/ubi/master/bootstrap/bootstrap-ubi.sh |
    sh

   mkdir -p ~/.ubi/bin
   echo 'export PATH="$HOME/.ubi/bin:$PATH"' >> ~/.zshrc  # or your preferred shell profile
   ```
2. **Install vnext with ubi:**  
   ```bash
   ubi --project hops-ops/hops-cli --in /usr/local/bin --rename-exe hops
   ```

Install a specific version:

```bash
ubi --project hops-ops/hops-cli --tag vx.x.x --in /usr/local/bin/ --rename-exe hops
```

See "Releases" for available versions and changenotes.

## Prerequisites

- macOS
- [Rust/Cargo](https://www.rust-lang.org/tools/install)
- [Homebrew](https://brew.sh/)
- `docker` CLI
- `kubectl`
- `helm`
- `up` (Upbound CLI, used by `up project build`)
- `aws` CLI v2 (used by `local aws` to export profile credentials)

Note: `hops-cli local install` installs the selected backend (`colima` or `kind`) through Homebrew.

## Build

```bash
cargo build
```

If you want static OpenSSL vendoring:

```bash
cargo build --features vendored
```

## Usage

```bash
hops --help
hops import --help
hops local --help
hops config --help
hops secrets --help
hops validate --help
hops xr --help
hops ai --help
```

Install the bundled Hops skills into the current repository for either
supported agent client:

```bash
hops ai codex
hops ai claude
```

Both commands install the general `hops` skill and the focused `hops-import`
skill. Existing files are preserved; pass `--force` only when replacing a
locally customized installed copy is intentional.

## Import an existing application

Run `hops import` from an existing GitHub repository to add the application
delivery files without changing its source code:

```bash
hops import
```

Preview the exact generated state before changing an existing repository:

```bash
hops import --dry-run
```

Dry-run classifies importer-owned paths as `CREATE`, `UPDATE`, or `UNCHANGED`
and prints the complete proposed content for creates and updates. It does not
write files, require `gh` or `vnext`, or configure a deploy key.

The command adds two independent Helm charts:

- `.gitops/deploy` for the application workload deployed by Argo CD
- `.gitops/promote` for rendering the Argo CD `Application` committed to an
  environment repository

The deploy chart contains a Kubernetes `Deployment` and `Service` by default.
For a Knative Serving application, select a Knative Service instead:

```bash
hops import --knative-service
```

The Knative deploy chart defaults to `minScale: 0`, configurable in its
generated values file. Import intentionally leaves `.gitops/local` alone until
the application's local development runtime has been selected explicitly.

It also adds workflows that calculate and push vNext tags, publish the
application image, promote `v*.*.*` releases to staging, and promote pull
requests labeled `preview` to the preview environment. Existing `./Dockerfile`
repositories use `workflows-containers`; repositories without one use the
pinned Railpack fallback. Image tags and promotion are ordered so an
environment is never updated before its image has been published.

To pilot an application through pull-request previews before enabling releases,
generate only the deploy and promotion charts, image publisher, and preview
workflow:

```bash
hops import --preview-only
```

Preview-only imports do not add main-branch versioning or staging promotion and
do not require a vNext deploy key. They still require the GitHub App credentials
described below to write the preview environment repository.

By default, `origin` supplies the GitHub `OWNER/REPO`, and the environment
repositories are `OWNER/OWNER-staging-env` and `OWNER/OWNER-preview-envs`.
The importer reads the default branch from `origin/HEAD`, falling back to the
checked-out branch. Override those choices when needed:

```bash
hops import ./service \
  --staging-repository example/platform-staging-env \
  --preview-repository example/platform-preview-envs \
  --branch trunk \
  --project example-nonprod
```

The importer uses `vnext generate-deploy-key` to create the repository's
`DEPLOY_KEY` secret and corresponding write-enabled deploy key. This requires
authenticated `gh` and `vnext` CLIs. Use `--skip-deploy-key` for offline
scaffolding or tests, then run the printed vNext command later. Existing
importer-owned files cause the command to stop before writing anything; use
`--force` to replace only those known paths.

Promotions authenticate with a GitHub App. The application repository must
receive the Actions secrets `GH_APP_ID` and `GH_APP_KEY`, and that App must be
installed with write access to the selected staging and preview repositories.

## Local GitOps workbench

The local workbench has two Kubernetes-shaped resources:

- **Cluster** describes one durable local control plane and its shared
  Crossplane resources. There is normally one Cluster per project/meta root.
- **Environment** describes the applications that should run for one checkout
  or worktree. Environment definitions are independent of the Cluster, so a
  worktree can be added or removed without editing `cluster.yaml`.

Keep the canonical files together under `.gitops/local/`:

```text
project/
  .gitops/local/cluster.yaml
  .gitops/local/environment.yaml
  .gitops/local/cluster/
    registry/        # local package registry
    providers/       # Provider, ProviderConfig, and runtime config
    configurations/  # Crossplane Configuration packages
    functions/       # Crossplane Function packages
    platform/        # namespaces and other platform resources
    shared/          # resources shared by all Environments
    rbac/            # cluster-level access for local packages
  apps/api/.gitops/local/       # editable local workload chart
  apps/api/.gitops/deploy/      # cloud workload chart
  apps/api/.gitops/promote/     # cloud promotion chart (not local input)
  apps/ui/.gitops/test-users/   # optional explicit local deploy
```

### Cluster definition

`cluster.yaml` must contain exactly one `hops.local/v1alpha1` `Cluster`:

```yaml
apiVersion: hops.local/v1alpha1
kind: Cluster
metadata:
  name: project-dev
spec:
  clusterProvider: kind
  dockerProvider: dory
  # Relative to .gitops/local/cluster.yaml. ../.. is the project root.
  mountRoot: ../..
  manifests:
    path: .gitops/local/cluster
  controlPlane:
    crossplane:
      chart: crossplane-stable/crossplane
      version: "2.4.0"
```

`mountRoot` is the smallest project/meta directory that contains the
worktrees. For kind, that exact host path is mounted into the node. An existing
kind Cluster with a different exact mount path fails with explicit
recreate/reset guidance; Hops never silently deletes it.

### Environment definition

An Environment is reusable from a checkout:

```yaml
apiVersion: hops.local/v1alpha1
kind: Environment
metadata:
  name: local
spec:
  clusterRef:
    name: project-dev
  # Resolved inside Cluster.spec.mountRoot.
  root: .
  values:
    local: true
    preview: false
  deploys:
    # Every path is a complete, explicit renderer input.
    - path: apps/api/.gitops/local
      type: helm
    - path: apps/ui/.gitops/local
      type: helm
    # A second deploy can use another chart from the same application.
    - path: apps/ui/.gitops/test-users
      type: helm
    # Raw Kubernetes YAML; recurse only when requested.
    - path: platform/manifests
      type: k8s
      recursive: true
    # A directory containing kustomization.yaml.
    - path: platform/overlays/local
      type: kustomize
```

Environment values are merged with deploy-specific values for Helm deploys. The
local controller injects the immutable runtime values `local: true`, the
Environment name/namespace, and the resolved source path/type. Raw Kubernetes
and Kustomize directories are already rendered inputs; they do not consume Helm
values or get silently Helm-templated, but they do receive the common namespace,
labels, and ownership pipeline. The namespace defaults to the Environment
runtime name; `--name` and `--namespace` can override those values for an
explicitly run Environment.

`type` is required and must be `helm`, `k8s`, or `kustomize`:

- `helm` requires `Chart.yaml` and renders with Helm values.
- `k8s` applies YAML files directly; `recursive` defaults to `false`.
- `kustomize` runs the directory's `kustomization.yaml`.

Each local workload chart is deliberately separate from its cloud chart:

- `.gitops/local` is the editable, source-mounted development workload.
- `.gitops/deploy` is the cloud deployment workload.
- `.gitops/promote` is rendered by cloud promotion tooling and is not selected
  by local reconciliation unless explicitly named as a deploy path.
- `.gitops/test-users` (or another explicit renderer directory) is an optional
  independent deploy, useful for local identities and fixtures.

### Start the Cluster controller

From the project/meta root:

```bash
# Defaults to .gitops/local/cluster.yaml.
hops local gitops cluster

# One reconcile for CI/scripts; do not enter the watcher.
hops local gitops cluster ./.gitops/local/cluster.yaml --once
```

`gitops cluster` is the canonical GitOps entry point. It:

1. Validates the Cluster, providers, paths, and manifest identities before
   touching the backend.
2. Starts or resumes the declared kind, Colima, or Dory backend.
3. Waits for the API and nodes, ensures local registry trust, and installs the
   pinned Crossplane Helm seed. This Helm seed is the one intentional
   prerequisite outside the file-owned tree because it creates the APIs needed
   by the remaining manifests.
4. Applies `.gitops/local/cluster/` and records a last-known-good inventory of
   exact object identities and content revisions.
5. Discovers every `.gitops/local/environment.yaml` below `mountRoot`, renders
   each Environment's explicit deploy paths with their declared renderer, and
   applies them to their namespaces.
6. Keeps one foreground watcher for the Cluster tree, discovered Environments,
   and their explicitly selected renderer directories.

The controller lock is stored under
`~/.hops/local/clusters/<cluster>/controller.lock`. A second process cannot
become a competing watcher. A malformed, conflicting, or stale lock is
rejected rather than implicitly adopted; stop or explicitly hand off the
existing owner first.

### Add a worktree

Put the worktree under the configured `mountRoot`, ensure it contains its
`.gitops/local/environment.yaml`, and give it a distinct Environment name (or
use an explicit `--name` invocation). The running Cluster controller discovers
the file and reconciles it into a separate namespace:

```bash
git worktree add .worktrees/feature-auth feature/auth

# Usually the single Cluster watcher is enough.
hops local gitops cluster
```

For a targeted one-shot workflow, reconcile an Environment
directly. This does not start a second watcher when the Cluster controller
already owns the backend:

```bash
hops local gitops environment ./.gitops/local/environment.yaml --name feature-auth --once
```

### What is watched and what happens on deletion

The watcher uses a short debounce and reacts to:

- Cluster YAML under `.gitops/local/cluster/`
- Environment definitions under `mountRoot`
- referenced explicit deploy directories (Helm, raw Kubernetes, or Kustomize)
- `.gitops/test-users` or other explicitly selected renderer directories
- `.gitops/promote` paths when a promotion chart is explicitly selected

Ordinary application source changes are handled by the development process in
the pod through the mounted source tree; they do not require a Helm reconcile.

Every successful Cluster pass atomically updates its inventory. A removed
Cluster manifest is pruned only by its recorded API version, kind, namespace,
and name. A removed Environment definition is pruned only from its durable
ownership snapshot and then unregistered. Removing a chart while the
Environment still exists is a reconcile error, not permission to delete the
whole Environment. Invalid changes retain the previous last-known-good state.

Environment cleanup is explicit and exact:

```bash
hops local gitops environment --name feature-auth --down
hops local gitops cluster --down
```

Environment `--down` deletes only its recorded namespaced objects and removes
its local registration. Cluster `--down` stops the backend and source-delivery
runtimes while preserving the inventory/snapshots for a later restart.

### Desired state versus local state

Committed YAML is the desired state. Hops keeps only runtime coordination data
under `~/.hops/local/`, including:

```text
clusters/<cluster>/controller.lock
clusters/<cluster>/cluster-inventory.json
clusters/<cluster>/environments/<environment>.json
```

These files contain ownership and identity metadata, not rendered secret
values, worktree inventories, restart counters, or source-generation values.

### GitOps and standalone local setup are separate modes

`hops local start` remains valid for the non-GitOps use case: prepare a local
control plane imperatively, then use plain `hops local aws`, `hops config
install`, or `hops provider install` to bootstrap an AWS/cloud environment.
That mode is intentionally separate from `hops local gitops cluster`; do not
run both owners against the same backend. When the GitOps controller owns a
Cluster, imperative package/provider installers reject the conflicting owner.

Provider credential commands have a deliberate secret boundary. For example:

```bash
hops local aws --profile my-profile --gitops ./.gitops/local/cluster
```

This writes the non-secret AWS Provider, runtime config, and ProviderConfig
files, while applying only the live credential Secret. The Cluster watcher
then owns the non-secret resources. GitHub and Zitadel currently have partial
`--gitops` writers; Cloudflare and Listmonk remain imperative. Local
`secretSync` is parsed as configuration but is not an active sync mechanism
yet.

The controller does not create or infer secrets, namespaces, or shared-resource
ownership from names alone.

## Command Areas

`hops-cli` is organized into a few command groups:

- `local`
  - Manage a local control plane (colima, kind, or dory backend), install providers, and bootstrap AWS or GitHub provider auth.
- `config`
  - Build, install, reload, and uninstall Crossplane configuration packages against the connected cluster.
- `secrets`
  - Initialize secrets config, encrypt and decrypt local secrets, and sync repo-managed secrets to AWS Secrets Manager or GitHub repository secrets.
- `validate`
  - Generate configuration manifests from Upbound-format XRD projects for validation workflows.
- `xr`
  - Observe existing XR-backed infrastructure and render adoption, management, or orphaning manifests.

Microservice scaffolding previously available as `hops service` now lives in the standalone **distributed** CLI (`distributed` / `distributed_cli`).

## Secrets

`hops secrets init` sets up local secrets directories, `.sops.yaml`, and `.hops.yaml` so plaintext secrets can be encrypted locally and synced to AWS Secrets Manager or GitHub repository secrets.

Typical layout:

```text
secrets/
  aws/
  github/
    _shared/
secrets-encrypted/
  aws/
  github/
```

Typical config:

```yaml
secrets:
  plaintext_dir: secrets
  encrypted_dir: secrets-encrypted
  aws:
    path: aws
    region: us-east-2
    tags:
      hops.ops.com.ai/secret: "true"
  github:
    owner: hops-ops
    path: github
    shared_secrets:
      path: _shared
      repos:
        - repo-a
        - repo-b
```

Encrypt and decrypt operate from the configured roots:

```bash
hops secrets encrypt
hops secrets decrypt
```

AWS sync reads from `<plaintext_dir>/<aws.path>`:

```bash
hops secrets sync aws
```

AWS rules:

- A `.json` file becomes one AWS Secrets Manager secret with the JSON object stored as-is.
- A directory containing plain files rolls up into one AWS secret. Each filename becomes a key in the JSON object.
- A `.env` file is parsed into key/value pairs and stored as one JSON secret.
- A directory containing a `.env` file merges those parsed key/value pairs into that directory's rolled-up JSON secret.
- Secret names are derived from the path relative to the AWS root.
- `--cleanup` only works when syncing the full configured AWS root.
- `hops.ops.com.ai/secret=true` is always applied to repo-managed AWS secrets.

Examples:

- `secrets/aws/app.json` -> AWS secret `app`
- `secrets/aws/github/token` and `secrets/aws/github/owner` -> AWS secret `github`
- `secrets/aws/slack/.env` with `WEBHOOK_URL=...` -> AWS secret `slack`

GitHub sync reads from `<plaintext_dir>/<github.path>`:

```bash
hops secrets sync github
```

GitHub rules:

- Each GitHub secret remains a separate GitHub secret. There is no AWS-style roll-up into a single JSON secret.
- A raw file becomes one GitHub secret.
- A `.json` file becomes multiple GitHub secrets, one per top-level key.
- A `.env` file becomes multiple GitHub secrets, one per `KEY=value` entry.
- Repo-specific secrets come from repo-named paths like `secrets/github/repo-a/...` or `secrets/github/repo-a.json`.
- Shared GitHub secrets come from `secrets/github/_shared/...` and fan out to the repos listed in `secrets.github.shared_secrets.repos` or passed with `--repo`.
- If a shared secret and a repo-specific secret have the same final name, the repo-specific value wins for that repo.
- GitHub secret names are normalized by the CLI to a stable format before syncing.

Examples:

- `secrets/github/repo-a/NPM_TOKEN` -> GitHub secret `NPM_TOKEN` in `repo-a`
- `secrets/github/repo-a/actions.json` with `{"SLACK_WEBHOOK":"..."}` -> GitHub secret `SLACK_WEBHOOK` in `repo-a`
- `secrets/github/repo-a/.env` with `NPM_TOKEN=...` -> GitHub secret `NPM_TOKEN` in `repo-a`
- `secrets/github/_shared/ORG_TOKEN` -> synced to every configured shared target repo

## Create a Local Control Plane (standalone mode)

The following is the separate non-GitOps workflow. Use it when the local
control plane is being used imperatively to bootstrap an AWS/cloud environment,
or when no `gitops cluster` controller owns the backend.

```bash
# 1) Install/select the cluster and Docker providers.
hops local install --cluster-provider kind --docker-provider dory

# 2) Start local k8s + Crossplane + providers + local registry
hops local start

# 3) Configure AWS provider-family + ProviderConfig from your AWS profile
hops local aws --profile <aws-profile>

# 4) Configure GitHub provider + ProviderConfig from your gh auth login
hops local github --owner <org-or-user>

# 5) Configure Zitadel provider + ProviderConfig from the AuthStack PAT Secret
hops local zitadel --source-context pat-local --domain auth.ops.com.ai

# 6) Install a Crossplane configuration package from an Upbound-format XRD project
hops config install --repo hops-ops/aws-auto-eks-cluster --version v0.11.0
```

### Cluster and Docker providers

`hops local` separates Kubernetes node provisioning from the Docker engine:

- **colima** — a VM running dockerd + k3s. macOS/Linux; supports `--cpus`,
  `--memory`, `--disk`, and `hops local resize`.
- **kind** — cluster nodes as docker containers on any reachable docker
  daemon: Docker Desktop, colima's dockerd, or CI runners. No VM of its own,
  so sizing flags don't apply (size the docker daemon instead); requires
  kind >= v0.27.
- **dory** — [dory](https://augani.github.io/dory) stock app: shared Apple
  Silicon engine + product k3s. Enable Kubernetes **in the Dory app** (hops
  does not fork Dory or call `dory k8s enable`). Package installs use the same
  **in-cluster** `registry:2` as other backends (Crossplane pulls
  `registry.crossplane-system.svc.cluster.local:5000`). Docker push uses the
  k3s **NodePort** on the engine docker bridge (`{dory-k8s-ip}:30500`) because
  dockerd runs *inside* the engine — Mac `localhost` is the wrong plane. The
  VM is sized in the Dory app, so hops sizing flags don't apply.

Select both dimensions explicitly:

```bash
hops local start --cluster-provider kind --docker-provider dory --cluster-name hops
```

The chosen pair is persisted to `~/.hops/local/providers.json` on a successful
start, so later commands can target the same cluster without repeating flags.

Unless `--context` is given, kubectl commands automatically use the backend's
kubeconfig context (`colima`, `kind-hops`, or `hops-dory`), regardless of your
current-context.

#### Using dory

Stock Dory only (brew cask / [Dory.app](https://augani.github.io/dory)). No hops
fork of Dory required.

```bash
# 1. Open Dory.app — engine healthy (not "needs attention")
# 2. Enable Kubernetes in the app; wait until the cluster is running
#    (product container is usually named dory-k8s)
# 3. Bootstrap Crossplane + local package registry
hops local start --cluster-provider dory --docker-provider dory
```

On start/activate, hops:

- merges stock `~/.kube/dory-config` into `~/.kube/config` as context **`hops-dory`**
  (override with `--dory-name <name>` or `HOPS_DORY_NAME`; persisted in `~/.hops/local/dory-name`)
- runs `kubectl config use-context hops-dory`
- creates/uses a docker context of the same name → `unix://$HOME/.dory/dory.sock`

`--dory-name` is intentionally **not** `--name`. Workspace GitOps uses `--name`
for the Kubernetes namespace.

So you should **not** need:

```bash
export KUBECONFIG=$HOME/.kube/dory-config
export DOCKER_HOST=unix://$HOME/.dory/dory.sock
```

```bash
hops local start --cluster-provider dory --docker-provider dory
hops local start --cluster-provider dory --docker-provider dory --dory-name mine
hops local gitops environment ./.gitops/local/environment.yaml --name alice

kubectl get nodes          # context hops-dory
docker info                # context hops-dory
hops local doctor
hops local github -o hops-ops
hops config install --path … --cluster-provider dory --docker-provider dory
```

Alternatively, use kind on Dory's docker socket (no product k3s):

```bash
docker context use dory   # product context from Dory.app
hops local start --cluster-provider kind --docker-provider dory --cluster-name hops
```

**CI:** `.github/workflows/on-pr-dory-smoke.yaml` runs on a **self-hosted**
Apple Silicon Mac labeled `hops-dory` (opt-in with PR label `test-dory` or
`workflow_dispatch`). Stock Dory only — no fork build, no Colima sock.
Offline runner → job waits. The session is **env-only** so your desktop
defaults never change: `DOCKER_HOST=unix://$HOME/.dory/dory.sock`, a
job-private `KUBECONFIG`, and `HOPS_DORY_DESKTOP=0` (hops skips
`use-context` / docker context switching). No `destroy`/`stop` of
`dory-k8s`. After start/doctor/registry, it path-installs the in-repo
fixture `tests/fixtures/config-smoke` (`hops config install --path …`) and
applies a namespaced ConfigMap XR under `hops-ci`. See the workflow header
for runner setup.

#### Dory architecture (why troubleshooting is different)

Two network planes matter. Mixing them up is the usual failure mode.

| Plane | Who | Where | Role |
|-------|-----|--------|------|
| **Engine Docker** | `docker` CLI via `~/.dory/dory.sock` | Linux VM (dockerd) | build/push images |
| **Cluster** | kubectl / Crossplane pods | k3s inside `dory-k8s` | run control plane + pull packages |

Package registry model (same idea as colima/kind, adapted to Dory):

- **Pull (Crossplane):** in-cluster Service  
  `registry.crossplane-system.svc.cluster.local:5000` over **HTTPS**
- **Push (docker on the engine):** k3s **NodePort** on the docker bridge  
  `{dory-k8s-ip}:30500` (not Mac `localhost:30500`)
- **TLS:** hops generates a self-signed CA/server cert (`Secret hops-local-registry-tls`)
  and patches Crossplane to trust that CA. Plain HTTP fails package unpack
  (`http: server gave HTTP response to HTTPS client`).

Do **not** put `host.dory.internal` in Crossplane package refs. That name exists
on the **node** `/etc/hosts`, not in pod CoreDNS, and is the wrong plane for
Crossplane's package manager.

#### Dory troubleshooting

**Engine won't start / thrash / `HV_BAD_ARGUMENT` (big Macs)**

The failure we hit in practice: on a **large laptop** (e.g. **128 GB** host
RAM), Dory's UI **Recommended** / host-scaled default for guest engine memory
was **~half of host RAM → 64 GB** (`DORYD_MEMORY_MB=65536`). That is not
"you need 64 GB of containers" — it is an aggressive ceiling. On Apple Silicon,
Hypervisor.framework often rejects creating a VM that large
(`HV_BAD_ARGUMENT`), so the engine never stays healthy and the UI looks broken
even though a **4 GB** engine works fine.

```bash
# Pin sane resources (4 GiB / 4 CPUs is enough for hops local)
defaults write com.pythonxi.Dory dory.engineMemoryMB -int 4096
defaults write com.pythonxi.Dory dory.engineCPUCount -int 4

# Confirm the LaunchAgent is not still on 65536:
launchctl print "gui/$(id -u)/dev.dory.doryd" 2>/dev/null | grep DORYD_MEMORY
# Live engine cmdline should show --mem-mb 4096, not 65536:
ps aux | grep 'dory-hv engine' | grep -v grep
```

In the Dory app **Resources** panel: set memory to **~4 GB** (8 GB is usually
fine too). **Do not apply Recommended** if it offers tens of GB on a high-RAM
Mac.

Then fully restart the engine (quit Dory, ensure no thrashing `doryd`/`dory-hv`,
reopen or `dory engine wake`). Once guest RAM is modest, the rest of hops local
is ordinary.

**Docker socket path**

Stock Docker API socket is:

```text
unix://$HOME/.dory/dory.sock
```

There is no `~/.dory/engine.sock` for the host Docker API. hops uses `dory.sock`
only. Prefer the docker context hops creates (`hops-dory`) over manual
`DOCKER_HOST`.

**Kubernetes missing / `no dory-k8s container`**

k3s is product-owned. hops does not run `dory k8s enable`.

1. Install the Kubernetes component if needed: `dory component install kubernetes`
2. Enable Kubernetes in the Dory app UI
3. Wait until a `dory-k8s` container is running: `docker ps` (with context hops-dory)
4. Re-run `hops local start --cluster-provider dory --docker-provider dory`

**`k ctx` has no dory / hops-dory entry**

Stock Dory writes `~/.kube/dory-config` (context often named `default`). hops
merges that into `~/.kube/config` as **`hops-dory`** on activate/start. If the
merge is missing:

```bash
hops local doctor --cluster-provider dory --docker-provider dory
kubectl config get-contexts   # expect hops-dory
kubectl config use-context hops-dory
```

**Configuration Installed=False / HTTPS error**

```text
http: server gave HTTP response to HTTPS client
```

Crossplane always pulls packages with HTTPS. The local registry must be TLS
(hops sets this up). If you have an old HTTP-only registry Deployment:

```bash
kubectl -n crossplane-system delete deploy registry
kubectl -n crossplane-system delete secret hops-local-registry-tls
hops local start --cluster-provider dory --docker-provider dory
```

**docker push to localhost:30500 fails (connection refused / HTTPS to HTTP)**

With the docker context pointed at Dory, **`localhost` is the engine VM**, not
the Mac. hops pushes to `{dory-k8s container IP}:30500` and marks the engine
docker bridge as an insecure registry for that self-signed TLS endpoint.
Check:

```bash
docker context show          # hops-dory
hops local doctor            # "package push registry reachable (…:30500)"
```

Do not rely on Mac `kubectl port-forward` for engine-side `docker push`.

**Engine docker broken after daemon.json / dockerd kill**

Prefer Dory's own recovery:

```bash
dory repair dockerd --apply
dory engine wake
dory readiness
```

Avoid raw `kill` of dockerd inside the guest unless you are prepared to wait for
`dory repair` / LaunchAgent recovery (`live-restore` helps but is not magic).

**Useful diagnostics**

```bash
dory doctor
dory readiness
hops local doctor --cluster-provider dory --docker-provider dory
docker context show
kubectl config current-context
kubectl get configuration,provider -A
kubectl -n crossplane-system get deploy,svc,secret | grep -E 'registry|crossplane|hops-local'
```

### Local provider setup and auth

`hops local aws`, `hops local github`, and `hops local zitadel` install the provider package and bootstrap auth into a local control plane. The exception is `--refresh`, which updates credentials only.

#### AWS auth

`hops local aws` installs the AWS provider package and uses your AWS CLI configuration to generate credentials for it.

```bash
# Use an explicit AWS profile
hops local aws --profile hops

# Refresh only the Secret credentials without re-applying the Provider or ProviderConfig
hops local aws --profile hops --refresh
```

How it works:

- Resolves the profile in this order: `--profile`, `AWS_PROFILE`, `AWS_DEFAULT_PROFILE`, then interactive prompt.
- Runs `aws configure export-credentials --format process`.
- If the selected profile needs AWS SSO login, it runs `aws sso login --profile <profile>` and retries once.
- Applies the AWS provider package unless `--refresh` is used.
- Writes the generated credentials into a Kubernetes Secret, defaulting to `default/aws-creds`.
- Applies an AWS `ProviderConfig` named `default` unless `--refresh` is used.
- Supports overrides for namespace, Secret name, ProviderConfig name, provider name, and provider package.

#### GitHub auth

`hops local github` installs the GitHub provider package and uses your GitHub CLI login to generate credentials for it.

```bash
# Use an explicit owner
hops local github --owner hops-ops

# Refresh only the Secret credentials without re-applying the Provider or ProviderConfig
hops local github --owner hops-ops --refresh
```

How it works:

- Resolves the owner in this order: `--owner`, `GH_OWNER`, `GITHUB_OWNER`, then interactive prompt.
- Uses your current `gh auth token`.
- If `gh` is not authenticated, it runs `gh auth login` and retries once.
- Applies the GitHub provider package unless `--refresh` is used.
- Writes the generated credentials into a Kubernetes Secret, defaulting to `default/github-creds`.
- Applies a GitHub `ProviderConfig` named `default` unless `--refresh` is used.
- Supports overrides for namespace, Secret name, ProviderConfig name, provider name, and provider package.

#### Zitadel auth

`hops local zitadel` installs the Zitadel provider package and creates a Zitadel `ProviderConfig` for consumer stacks that need to author Zitadel resources from the local control plane.

```bash
# Read the AuthStack iam-admin PAT from a target cluster and create default/zitadel-credentials + ProviderConfig/default
hops local zitadel --source-context pat-local --domain auth.ops.com.ai

# Use an explicit token instead of reading the target cluster Secret
ZITADEL_ACCESS_TOKEN=<pat> hops local zitadel --domain auth.ops.com.ai

# Refresh only the Secret credentials without re-applying the Provider or ProviderConfig
hops local zitadel --source-context pat-local --domain auth.ops.com.ai --refresh
```

How it works:

- Resolves the access token in this order: `--access-token`, `ZITADEL_ACCESS_TOKEN`, then the source cluster Secret.
- Defaults the source Secret to `pat-local/zitadel/iam-admin-pat` key `pat`.
- Writes the generated credentials JSON into a Kubernetes Secret, defaulting to `default/zitadel-credentials`.
- Applies a Zitadel `ProviderConfig` named `default` unless `--refresh` is used.
- Supports overrides for namespace, Secret name, ProviderConfig name, provider name, provider package, source context, source namespace, source Secret, source key, domain, port, and `insecure`.

## Config packages

`config install` and `config uninstall` operate on the currently connected Kubernetes cluster.

There are two different `config install` modes:

- Source-build mode via `--path` or `--repo` builds an Upbound-format XRD project locally, pushes the package through the local registry flow, and is intended for a local control plane started with `hops local start`.
- Remote-package mode via `--repo ... --version ...` skips the build and applies a pinned package reference directly, so it can work against non-local connected clusters too.

Common install flows:

```bash
# Build from the current directory when it is an Upbound-format XRD project
hops config install

# Build from an explicit local Upbound-format XRD project path
hops config install --path /path/to/project

# Install from a GitHub repo; interactive TTY runs ask whether to build from source
# or use a published version
hops config install --repo hops-ops/aws-auto-eks-cluster

# Force a source reload before re-applying
hops config install --repo hops-ops/aws-auto-eks-cluster --reload

# Set spec.skipDependencyResolution=true on the generated Configuration
hops config install --path /path/to/project --skip-dependency-resolution

# Apply a pinned remote package directly from ghcr.io
hops config install --repo hops-ops/aws-auto-eks-cluster --version v0.11.0
```

Common uninstall flows:

```bash
# Remove by explicit configuration name
hops config uninstall --name hops-ops-aws-auto-eks-cluster

# Remove by repo slug
hops config uninstall --repo hops-ops/aws-auto-eks-cluster

# Remove configurations derived from local build artifacts
hops config uninstall --path /path/to/project
```

Notes:

- `--reload` only applies to source installs: `--path` or `--repo` without `--version`.
- `--skip-dependency-resolution` sets `spec.skipDependencyResolution=true` on the generated `Configuration`.
- `config install --repo ...` now prompts in interactive terminals to choose between cloning/building from source or applying a published package version. Published-version prompts suggest the latest discovered tag by default and still accept arbitrary tags such as `pr-<gitsha>`.
- Non-interactive `config install --repo ...` keeps the previous default behavior and builds from source.
- `config install --repo ... --version ...` skips clone/build and applies the remote package directly.
- `config uninstall --repo ...` uses the cached `_output/*.uppkg` package identity when available. Without cached artifacts, it assumes the published OCI package is `ghcr.io/<org>/<repo>`.

## Commands

- `local install`
  - Runs `brew install colima`.
- `local reset`
  - Runs `colima kubernetes reset`.
- `local start [--bootstrap]`
  - Brings up the selected backend cluster (colima / kind / dory)
  - If Crossplane, k8s+helm providers, and the package registry are already
    Healthy/Available, skips helm repo update/upgrade and bootstrap reapply
    (fast resume). Pass `--bootstrap` to force a full helm upgrade + reapply.
  - Cold path: installs Crossplane, applies `bootstrap/` DRCs/providers/PCs,
    deploys the local HTTPS package registry, wires node/engine registry trust
- `local stop`
  - Runs `colima stop`.
- `local destroy`
  - Runs `colima delete --force`.
- `local uninstall`
  - Prompts for confirmation, then runs `brew uninstall colima`.
- `config install [--path <PATH>] [--reload]`
  - Targets the currently connected Kubernetes cluster
  - Source-build mode intended for a local control plane because it depends on the local registry flow
  - Runs `up project build` in `PATH` (defaults to current directory)
  - Loads generated `.uppkg` artifacts from `<PATH>/_output`
  - Pushes package images to the backend-specific registry endpoint selected by the CLI: `127.0.0.1:30500` when Docker runs locally, or `{dory-k8s-ip}:30500` for Dory
  - Applies Crossplane `Configuration` resources pointing at `registry.crossplane-system.svc.cluster.local:5000/...`
  - Supports `--skip-dependency-resolution`
- `config install --repo <org/repo> [--reload]`
  - Interactive terminals prompt for install mode: source build or published version
  - Published-version installs suggest the latest discovered tag by default and accept custom tags such as `pr-<gitsha>`
  - Non-interactive runs and `--reload` continue to use the source-build flow
  - Source-build mode is intended for a local control plane because it depends on the local registry flow
  - Source builds use local repo cache at `~/.hops/local/repo-cache/<org>/<repo>`
  - Source builds clone on first use, then fetch/pull on subsequent runs
  - Source builds run the same build/load/push/apply flow as `--path`
- `--reload`
  - Forces source-based config install (`--path` or `--repo` without `--version`) to delete existing `ConfigurationRevision` resources and matching `Function`/`FunctionRevision` package resources from the same sources, then re-apply the `Configuration`
  - Useful when re-running a config and you want Crossplane to re-create the current revision from source
- `config install --repo <org/repo> --version <tag>`
  - Remote-package mode that can target any connected cluster
  - Skips clone/build and applies `Configuration` with package `ghcr.io/<org>/<repo>:<tag>`
  - Uses configuration name `<org>-<package>` (for example `hops-ops-aws-auto-eks-cluster`)
  - Does not support `--reload`
  - Supports `--skip-dependency-resolution`
- `config uninstall --name <configuration-name>`
  - Deletes the target `Configuration`
  - Waits for package lock reconciliation
  - Prunes orphaned `Configuration`/`Function`/`Provider` packages and revisions no longer present in lock
  - Prunes orphaned `ImageConfig` rewrites for removed render functions
- `config uninstall --repo <org/repo>`
  - Uses package identity from cached `_output/*.uppkg` artifacts when available, so the repository and packaged OCI names may differ
  - Without cached artifacts, assumes the published OCI package is `ghcr.io/<org>/<repo>`
  - If cached repo exists at `~/.hops/local/repo-cache/<org>/<repo>`, also derives source hints from it for additional package pruning
- `config uninstall --path <PATH>`
  - Derives target configuration names from `<PATH>/_output/*.uppkg` image tags
  - Also derives package sources from those artifacts and prunes matching package resources (including Functions) if they remain
- `local aws [--profile <AWS_PROFILE>]`
  - Exports temporary AWS credentials with `aws configure export-credentials --format process`
  - Uses profile resolution order: `--profile` -> `AWS_PROFILE` -> `AWS_DEFAULT_PROFILE` -> interactive prompt
  - If AWS SSO token is missing/expired, runs `aws sso login --profile <profile>` and retries once
  - Applies `xpkg.crossplane.io/crossplane-contrib/provider-family-aws:v2.4.0`
  - Waits for `providerconfigs.aws.m.upbound.io` CRD to exist
  - Applies a Secret (`aws-creds`) and AWS `ProviderConfig` (`default`) in namespace `default`
  - `--refresh` updates only the Secret credentials and skips Provider/ProviderConfig apply
  - Supports overrides via `--namespace`, `--secret-name`, `--provider-config-name`, `--provider-name`, and `--provider-package`
- `local github [--owner <ORG_OR_USER>]`
  - Exports your current GitHub CLI token with `gh auth token`
  - Uses owner resolution order: `--owner` -> `GH_OWNER` -> `GITHUB_OWNER` -> interactive prompt with your authenticated `gh` login as the default
  - If GitHub CLI is not authenticated, runs `gh auth login` and retries once
  - Applies `xpkg.crossplane.io/crossplane-contrib/provider-upjet-github:v0.19.0`
  - Waits for `providerconfigs.github.m.upbound.io` CRD to exist
  - Applies a Secret (`github-creds`) and GitHub `ProviderConfig` (`default`) in namespace `default`
  - `--refresh` updates only the Secret credentials and skips Provider/ProviderConfig apply
  - Supports overrides via `--namespace`, `--secret-name`, `--provider-config-name`, `--provider-name`, and `--provider-package`
- `validate generate-configuration [--path <PATH>] [--api-path <APIS_PATH>]`
  - Reads `<PATH>/upbound.yaml` and writes `<APIS_PATH>/configuration.yaml`
  - Auto-detects `--api-path` via `apis/*/definition.yaml` when omitted
  - Ensures `apis/**/configuration.yaml` is present in `<PATH>/.gitignore` (unless `--no-gitignore-update`)
- `xr observe --kind <KIND> --name <NAME> --namespace <NAMESPACE> --aws-region <REGION>`
  - Generates an observe-only XR manifest for an existing resource
  - Loads the live XR from the cluster when present
  - Enriches the manifest with live AWS discovery for supported XR kinds such as `AutoEKSCluster` and `Network`
  - Supports `--output` and `--apply`
- `xr adopt --kind <KIND> --name <NAME> --namespace <NAMESPACE>`
  - Lists managed resources that belong to the XR and renders metadata patches needed for adoption
  - For `AutoEKSCluster`, uses the composite-specific label `hops.ops.com.ai/autoekscluster=<name>`
  - Only emits patches for resources whose external name is missing or blank and can be resolved for that kind
  - Supports `--apply`, `--output`, and `--recursive`
- `xr manage --kind <KIND> --name <NAME> --namespace <NAMESPACE>`
  - Generates the final managed XR manifest from an observed or adopted XR already in the cluster
  - Supports `--output` and `--apply`
- `xr orphan --kind <KIND> --name <NAME> --namespace <NAMESPACE>`
  - Renders managed-resource patches that remove `Delete` from management policies
  - Supports `--apply` and `--output`
- `xr migrate --kind <KIND> --name <NAME> --source-context <CONTEXT> --source-namespace <NAMESPACE> --target-context <CONTEXT> --target-namespace <NAMESPACE>`
  - Recursively compares the source and target XR composition graphs
  - Requires the target XR and its composed resources to be observe-only
  - Copies existing `crossplane.io/external-name` identities to matching target managed resources
  - Supports status-gated graphs by reporting source resources not rendered in the target as deferred
  - Plans without changing either cluster by default; `--apply` patches only the target
  - Never orphans, deletes, or changes management policies on the source XR

## XR workflow

Typical reclaim flow:

```bash
# 1) Observe the existing resource into an XR manifest
hops xr observe --kind AutoEKSCluster --name pat-local --namespace default --aws-region us-east-2 --output observed.yaml

# 2) Apply the observe XR if desired
kubectl apply -f observed.yaml

# 3) Render and apply adoption patches for the next set of managed resources
hops xr adopt --kind AutoEKSCluster --name pat-local --namespace default --apply

# 4) Repeat adopt until no more patches are needed, or use --recursive
hops xr adopt --kind AutoEKSCluster --name pat-local --namespace default --recursive --apply

# 5) Convert the XR into a managed manifest
hops xr manage --kind AutoEKSCluster --name pat-local --namespace default --output managed.yaml
```

Notes:

- `xr adopt` only patches resources it can identify for the selected XR kind.
- A blank `crossplane.io/external-name` is treated as missing.
- `AutoEKSCluster` adoption currently resolves identities for supported managed kinds such as IAM attachments and KMS keys.

### Cross-control-plane migration staging

Create the same XR in the target control plane with
`managementPolicies: [Observe, LateInitialize]`, and wait for its composition
graph to render. Then inspect the migration plan:

```bash
hops xr migrate \
  --kind RegistryCache \
  --name production \
  --source-context kind-gitkb-aws-bootstrap \
  --source-namespace default \
  --target-context arn:aws:eks:us-east-2:065328823520:cluster/production \
  --target-namespace production
```

The command matches recursively composed managed resources by their composition
path, API group, and kind. A status-gated target may initially be a strict
subset of the source; source-only resources are reported as `DEFER` until an
earlier identity patch lets the target render them. Apply the verified identity
patches with `--apply`, wait for the graph to expand, and repeat until
`deferred: 0`. The command still fails if the target has a resource absent from
the source, a source external name is missing, the target allows creation or
mutation, or a target already has a different external name. This command
stages adoption only; source orphaning and target promotion remain separate,
explicit cutover steps.

## Logging

Set `LOG_LEVEL` to control output (default: `info`):

```bash
LOG_LEVEL=debug hops local start
```

## Development

```bash
cargo test
```
