# hops-cli

`hops-cli` is a Rust CLI for Crossplane development and XR lifecycle workflows.

## Overview

This tool supports three related workflows:

- Local cluster setup on Colima
- Configuration package install/uninstall against the connected cluster
- XR observe/manage/adopt/orphan workflows for existing infrastructure

For local development, it can also:

- Install and manage Colima
- Start a local k8s cluster with Crossplane installed via Helm
- Install the Kubernetes and Helm Crossplane providers
- Deploy an in-cluster OCI registry (`crossplane-system/registry`)
- Build and publish Crossplane configuration packages from an XRD project

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

Note: `hops-cli local install` installs `colima` through Homebrew.

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
hops local --help
hops config --help
hops validate --help
hops xr --help
```

## Create a Local Control Plane

```bash
# 1) Install Colima (via Homebrew)
hops local install

# 2) Start local k8s + Crossplane + providers + local registry
hops local start

# 3) Configure AWS provider-family + ProviderConfig from your AWS profile
hops local aws --profile <aws-profile>

# 4) Configure GitHub provider + ProviderConfig from your gh auth login
hops local github --owner <org-or-user>

# 5) Install a Crossplane configuration package from an Upbound-format XRD project
hops config install --repo hops-ops/aws-auto-eks-cluster --version v0.11.0
```

### Local provider setup and auth

`hops local aws` and `hops local github` install the provider package and bootstrap auth into a local control plane. The exception is `--refresh`, which updates credentials only.

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

## Quick Start

```bash
# Build and load a Crossplane configuration package from an Upbound-format XRD project
hops config install --path /path/to/project

# Build from a GitHub repo containing an Upbound-format XRD project
hops config install --repo hops-ops/helm-certmanager

# Force reload from source (deletes existing ConfigurationRevision(s) first)
hops config install --repo hops-ops/helm-certmanager --reload

# Apply a pinned remote package version directly (no clone/build)
hops config install --repo hops-ops/helm-certmanager --version v0.1.0

# Remove a configuration and prune orphaned package dependencies
hops config uninstall --repo hops-ops/helm-certmanager

# Generate apis/*/configuration.yaml from upbound.yaml for validation
hops validate generate-configuration --path /path/to/project

# Observe an existing XR into a manifest
hops xr observe --kind AutoEKSCluster --name pat-local --namespace default --aws-region us-east-2

# Render adoption patches for managed resources under an existing XR
hops xr adopt --kind AutoEKSCluster --name pat-local --namespace default

# Convert an observed/adopted XR into a managed manifest
hops xr manage --kind AutoEKSCluster --name pat-local --namespace default

# Render patches that remove Delete from management policies
hops xr orphan --kind AutoEKSCluster --name pat-local --namespace default
```

## Config packages

`config install` and `config uninstall` operate on the currently connected Kubernetes cluster.
`config install` expects an Upbound-format XRD project when building from source via `--path` or `--repo`.

Common install flows:

```bash
# Build from the current directory when it is an Upbound-format XRD project
hops config install

# Build from an explicit local Upbound-format XRD project path
hops config install --path /path/to/project

# Build from a cached GitHub repo checkout containing an Upbound-format XRD project
hops config install --repo hops-ops/helm-certmanager

# Force a source reload before re-applying
hops config install --repo hops-ops/helm-certmanager --reload

# Apply a pinned remote package directly from ghcr.io
hops config install --repo hops-ops/helm-certmanager --version v0.1.0
```

Common uninstall flows:

```bash
# Remove by explicit configuration name
hops config uninstall --name hops-ops-helm-certmanager

# Remove by repo slug
hops config uninstall --repo hops-ops/helm-certmanager

# Remove configurations derived from local build artifacts
hops config uninstall --path /path/to/project
```

Notes:

- `--reload` only applies to source installs: `--path` or `--repo` without `--version`.
- `config install --repo ... --version ...` skips clone/build and applies the remote package directly.
- `config uninstall --repo ...` derives the configuration name as `<org>-<repo>`.

## Commands

- `local install`
  - Runs `brew install colima`.
- `local reset`
  - Runs `colima kubernetes reset`.
- `local start`
  - Runs `colima start --kubernetes --cpu 8 --memory 16 --disk 60`
  - Installs Crossplane from `crossplane-stable/crossplane`
  - Applies manifests from `bootstrap/` for runtime config, providers, provider configs, and registry (embedded in the binary at build time)
  - Configures Docker in Colima for insecure pulls from `registry.crossplane-system.svc.cluster.local:5000`
  - Adds host mapping in Colima VM for the registry service DNS name
- `local stop`
  - Runs `colima stop`.
- `local destroy`
  - Runs `colima delete --force`.
- `local uninstall`
  - Prompts for confirmation, then runs `brew uninstall colima`.
- `config install [--path <PATH>] [--reload]`
  - Targets the currently connected Kubernetes cluster
  - Runs `up project build` in `PATH` (defaults to current directory)
  - Loads generated `.uppkg` artifacts from `<PATH>/_output`
  - Pushes package images to the registry exposed at `localhost:30500`
  - Applies Crossplane `Configuration` resources pointing at `registry.crossplane-system.svc.cluster.local:5000/...`
- `config install --repo <org/repo> [--reload]`
  - Uses local repo cache at `~/.hops/local/repo-cache/<org>/<repo>`
  - Clones on first use, then fetches/pulls on subsequent runs
  - Runs the same build/load/push/apply flow as `--path`
- `--reload`
  - Forces source-based config install (`--path` or `--repo` without `--version`) to delete existing `ConfigurationRevision` resources and matching `Function`/`FunctionRevision` package resources from the same sources, then re-apply the `Configuration`
  - Useful when re-running a config and you want Crossplane to re-create the current revision from source
- `config install --repo <org/repo> --version <tag>`
  - Skips clone/build and applies `Configuration` with package `ghcr.io/<org>/<repo>:<tag>`
  - Uses configuration name `<org>-<repo>` (for example `hops-ops-helm-certmanager`)
  - Does not support `--reload`
- `config uninstall --name <configuration-name>`
  - Deletes the target `Configuration`
  - Waits for package lock reconciliation
  - Prunes orphaned `Configuration`/`Function`/`Provider` packages and revisions no longer present in lock
  - Prunes orphaned `ImageConfig` rewrites for removed render functions
- `config uninstall --repo <org/repo>`
  - Targets configuration name `<org>-<repo>`
  - If cached repo exists at `~/.hops/local/repo-cache/<org>/<repo>`, derives source hints from it for additional package pruning
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

## Logging

Set `LOG_LEVEL` to control output (default: `info`):

```bash
LOG_LEVEL=debug hops local start
```

## Development

```bash
cargo test
```
