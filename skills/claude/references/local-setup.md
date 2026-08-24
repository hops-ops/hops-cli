# Local Control Plane Setup

## Quick Start

```bash
# 1. Start/resume the declared cluster and watch shared GitOps manifests
hops local gitops cluster ./.gitops/local/cluster.yaml

# 2. Add or update platform packages in .gitops/local/cluster when needed
hops config install --repo hops-ops/psql-stack --version v0.9.1 \
  --gitops ./.gitops/local/cluster --local
hops config install --repo hops-ops/auth-stack --version v1.6.0 \
  --gitops ./.gitops/local/cluster --local

# 3. Register this checkout as an Environment
hops local gitops environment ./.gitops/local/environment.yaml --name main

# 4. Optional cloud provider auth (writes live Secrets; use --gitops for non-secret YAML)
hops local aws --profile hops
hops local github --owner hops-ops
```

**Local CP note:** `hops local start` creates Helm/Kubernetes ProviderConfigs named
`default`. Stack XRs must pin `helmProviderConfigRef` / `kubernetesProviderConfigRef`
to `default` (scaffolded by `config install --gitops --local`). See
[config-install.md](./config-install.md).

## Commands

### `hops local install`
Installs Colima via Homebrew.

### `hops local gitops cluster <cluster.yaml>`

This is the normal lifecycle command. It validates the Kubernetes-shaped
Cluster definition, invokes the local start/bootstrap pipeline, and applies +
watches the definition's `spec.manifests.path`.

The underlying `hops local start` command:
- Starts the chosen backend (colima / kind / dory)
- Installs **pinned** Crossplane Helm chart (`CROSSPLANE_CHART_VERSION` in `start.rs`)
- Applies bootstrap Providers (pinned tags in `bootstrap/providers/`):
  - `provider-helm` (needs ≥ v1.3.0 for Zitadel chart JSON-schema $ref fix)
  - `provider-kubernetes`
- Applies ProviderConfigs named `default`, local registry, DRCs
- Configures node trust for the in-cluster registry

```bash
hops local gitops cluster ./.gitops/local/cluster.yaml
hops local gitops cluster ./.gitops/local/cluster.yaml --down
```

**Version bumps:** Renovate owns these pins (`cli/renovate.json` customManagers →
github-releases). Prefer merging Renovate PRs (`local-start-crossplane-bootstrap`
group) over hand-editing tags.

**Replace a bootstrap provider with a local build** (keep SemVer deps working):

```bash
hops provider install --path /path/to/provider-helm --gitops ./gitops/cluster
# writes providers/ + runtime/ + imageconfigs/ with vMAJOR.999.N package pin
```

See [local-source-packages.md](./local-source-packages.md).

### Cluster teardown

`hops local gitops cluster <cluster.yaml> --down` stops the declared Cluster
while preserving its data. `hops local destroy` remains the explicit,
destructive cluster deletion command; `hops local uninstall` removes tooling.

### `hops local aws --profile <PROFILE>`

Installs AWS provider family and bootstraps auth.

- Resolves profile: `--profile` → `AWS_PROFILE` → `AWS_DEFAULT_PROFILE` → prompt
- Exports credentials via `aws configure export-credentials --format process`
- Auto-triggers `aws sso login` if needed
- Applies Provider package, Secret (`aws-creds`), and ProviderConfig (`default`)
- `--refresh` updates credentials only (skips Provider/ProviderConfig)

### `hops local github --owner <ORG>`

Installs GitHub provider and bootstraps auth.

- Resolves owner: `--owner` → `GH_OWNER` → `GITHUB_OWNER` → prompt
- Uses `gh auth token` for credentials
- Auto-triggers `gh auth login` if needed
- Applies Provider package, Secret (`github-creds`), and ProviderConfig (`default`)
- `--refresh` updates credentials only

## Architecture

```
┌─────────────────────────────────────────────┐
│  Colima VM                                  │
│  ┌────────────────────────────────────────┐ │
│  │  Kubernetes (k3s)                      │ │
│  │  ┌──────────────────────────────────┐  │ │
│  │  │  crossplane-system namespace     │  │ │
│  │  │  - Crossplane                    │  │ │
│  │  │  - Provider Helm                 │  │ │
│  │  │  - Provider Kubernetes           │  │ │
│  │  │  - OCI Registry (:5000/:30500)   │  │ │
│  │  └──────────────────────────────────┘  │ │
│  │  ┌──────────────────────────────────┐  │ │
│  │  │  default namespace               │  │ │
│  │  │  - AWS ProviderConfig + Secret   │  │ │
│  │  │  - GitHub ProviderConfig + Secret│  │ │
│  │  │  - Helm ProviderConfig           │  │ │
│  │  │  - K8s ProviderConfig            │  │ │
│  │  └──────────────────────────────────┘  │ │
│  └────────────────────────────────────────┘ │
└─────────────────────────────────────────────┘
     127.0.0.1:30500 → registry:5000 (host push; use IPv4 not localhost)
```

## Logging

```bash
LOG_LEVEL=debug hops local start
```
