# Local Control Plane Setup

## Quick Start

```bash
# 1. Install Colima
hops local install

# 2. Start local k8s + Crossplane + providers + registry
hops local start

# 3. Configure AWS provider auth from your AWS profile
hops local aws --profile hops

# 4. Configure GitHub provider auth from gh CLI
hops local github --owner hops-ops

# 5. Install a configuration package
hops config install --repo hops-ops/aws-auto-eks-cluster --version v0.11.0
```

## Commands

### `hops local install`
Installs Colima via Homebrew.

### `hops local start`
- Starts Colima with `--kubernetes --cpu 8 --memory 16 --disk 60`
- Installs Crossplane from `crossplane-stable/crossplane`
- Applies bootstrap manifests: runtime config, providers, provider configs, registry
- Configures Docker for insecure pulls from the in-cluster registry
- Adds host mapping for `registry.crossplane-system.svc.cluster.local`

### `hops local stop` / `hops local destroy` / `hops local uninstall`
Stop, delete, or uninstall Colima respectively.

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
     localhost:30500 → registry:5000
```

## Logging

```bash
LOG_LEVEL=debug hops local start
```
