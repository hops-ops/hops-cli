# hops-cli

`hops-cli` is a Rust CLI for running a local Crossplane development environment on Colima.

## Overview

This tool manages a local Kubernetes stack and package workflow for Crossplane:

- Installs and manages Colima and kubefwd
- Starts a local k8s cluster with Crossplane installed via Helm
- Installs the Kubernetes and Helm Crossplane providers
- Deploys an in-cluster OCI registry (`crossplane-system/registry`)
- Builds and publishes Crossplane configuration packages from an XRD project

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
- `kubefwd` (used by `local start` / `local kubefwd start` for service forwarding)
- `aws` CLI v2 (used by `local aws` to export profile credentials)

Note: `hops-cli local install` installs `colima` and `kubefwd` through Homebrew.

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
hops local kubefwd --help
hops config --help
```

From source without installing:

```bash
cargo run -- --help
cargo run -- local --help
cargo run -- config --help
```

## Quick Start

```bash
# 1) Install Colima + kubefwd (via Homebrew)
cargo run -- local install

# 2) Start local k8s + Crossplane + providers + local registry
cargo run -- local start

# 3) Configure AWS provider-family + ProviderConfig from your AWS profile
cargo run -- local aws --profile <aws-profile>

# 4) Configure GitHub provider + ProviderConfig from your gh auth login
cargo run -- local github --owner <org-or-user>

# 5) Build and load a Crossplane configuration package from an XRD project
cargo run -- local config --path /path/to/project

# 6) Build from a GitHub repo (cached clone + build + push to local registry)
cargo run -- local config --repo hops-ops/helm-certmanager

# 7) Force reload from source (deletes existing ConfigurationRevision(s) first)
cargo run -- local config --repo hops-ops/helm-certmanager --reload

# 8) Apply a pinned remote package version directly (no clone/build)
cargo run -- local config --repo hops-ops/helm-certmanager --version v0.1.0

# 9) Remove a configuration and prune orphaned package dependencies
cargo run -- local unconfig --repo hops-ops/helm-certmanager

# 10) Generate apis/*/configuration.yaml from upbound.yaml for validation
cargo run -- config generate --path /path/to/project
```

## Commands

- `local install`
  - Runs `brew install colima kubefwd`.
- `local reset`
  - Stops the background `kubefwd` process started by `local start`
  - Runs `colima kubernetes reset`.
- `local start`
  - Runs `colima start --kubernetes --cpu 8 --memory 16 --disk 60`
  - Installs Crossplane from `crossplane-stable/crossplane`
  - Applies manifests from `bootstrap/` for runtime config, providers, provider configs, and registry (embedded in the binary at build time)
  - Configures Docker in Colima for insecure pulls from `registry.crossplane-system.svc.cluster.local:5000`
  - Adds host mapping in Colima VM for the registry service DNS name
  - Starts `kubefwd services -A --resync-interval 30s` in the background (log: `~/.hops/local/kubefwd.log`)
- `local kubefwd start`
  - Starts background `kubefwd` forwarding for all namespaces with a forced resync every 30s.
- `local kubefwd stop`
  - Stops the background `kubefwd` process managed by this CLI.
- `local kubefwd refresh`
  - Restarts background `kubefwd` immediately (stop + start).
- `local stop`
  - Stops the background `kubefwd` process started by `local start`
  - Runs `colima stop`.
- `local destroy`
  - Stops the background `kubefwd` process started by `local start`
  - Runs `colima delete --force`.
- `local uninstall`
  - Stops the background `kubefwd` process started by `local start`
  - Prompts for confirmation, then runs `brew uninstall colima`.
- `local config [--path <PATH>] [--reload]`
  - Runs `up project build` in `PATH` (defaults to current directory)
  - Loads generated `.uppkg` artifacts from `<PATH>/_output`
  - Pushes package images to local registry (`localhost:30500`)
  - Applies Crossplane `Configuration` resources pointing at `registry.crossplane-system.svc.cluster.local:5000/...`
- `local config --repo <org/repo> [--reload]`
  - Uses local repo cache at `~/.hops/local/repo-cache/<org>/<repo>`
  - Clones on first use, then fetches/pulls on subsequent runs
  - Runs the same build/load/push/apply flow as `--path`
- `--reload`
  - Forces source-based config (`--path` or `--repo` without `--version`) to delete existing `ConfigurationRevision` resources and matching `Function`/`FunctionRevision` package resources from the same sources, then re-apply the `Configuration`
  - Useful when re-running a config and you want Crossplane to re-create the current revision from source
- `local config --repo <org/repo> --version <tag>`
  - Skips clone/build and applies `Configuration` with package `ghcr.io/<org>/<repo>:<tag>`
  - Uses configuration name `<org>-<repo>` (for example `hops-ops-helm-certmanager`)
  - Does not support `--reload`
- `local unconfig --name <configuration-name>`
  - Deletes the target `Configuration`
  - Waits for package lock reconciliation
  - Prunes orphaned `Configuration`/`Function`/`Provider` packages and revisions no longer present in lock
  - Prunes orphaned `ImageConfig` rewrites for removed render functions
- `local unconfig --repo <org/repo>`
  - Targets configuration name `<org>-<repo>`
  - If cached repo exists at `~/.hops/local/repo-cache/<org>/<repo>`, derives source hints from it for additional package pruning
- `local unconfig --path <PATH>`
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
- `config generate [--path <PATH>] [--api-path <APIS_PATH>]`
  - Reads `<PATH>/upbound.yaml` and writes `<APIS_PATH>/configuration.yaml`
  - Auto-detects `--api-path` via `apis/*/definition.yaml` when omitted
  - Ensures `apis/**/configuration.yaml` is present in `<PATH>/.gitignore` (unless `--no-gitignore-update`)

## Logging

Set `LOG_LEVEL` to control output (default: `info`):

```bash
LOG_LEVEL=debug cargo run -- local start
```

## Development

```bash
cargo test
```
