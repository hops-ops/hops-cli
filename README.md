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
```

From source without installing:

```bash
cargo run -- --help
cargo run -- local --help
```

## Quick Start

```bash
# 1) Install Colima + kubefwd (via Homebrew)
cargo run -- local install

# 2) Start local k8s + Crossplane + providers + local registry
cargo run -- local start

# 3) Configure AWS provider-family + ProviderConfig from your AWS profile
cargo run -- local aws --profile <aws-profile>

# 4) Build and load a Crossplane configuration package from an XRD project
cargo run -- local config /path/to/project
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
  - Applies manifests from `bootstrap/` for runtime config, providers, provider configs, and registry
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
- `local config [PATH]`
  - Runs `up project build` in `PATH` (defaults to current directory)
  - Loads generated `.uppkg` artifacts from `<PATH>/_output`
  - Pushes package images to local registry (`localhost:30500`)
  - Applies Crossplane `Configuration` resources pointing at `registry.crossplane-system.svc.cluster.local:5000/...`
- `local aws [--profile <AWS_PROFILE>]`
  - Exports temporary AWS credentials with `aws configure export-credentials --format process`
  - Uses profile resolution order: `--profile` -> `AWS_PROFILE` -> `AWS_DEFAULT_PROFILE` -> interactive prompt
  - If AWS SSO token is missing/expired, runs `aws sso login --profile <profile>` and retries once
  - Applies `xpkg.crossplane.io/crossplane-contrib/provider-family-aws:v2.4.0`
  - Waits for `providerconfigs.aws.m.upbound.io` CRD to exist
  - Applies a Secret (`aws-creds`) and AWS `ProviderConfig` (`default`) in namespace `default`
  - Supports overrides via `--namespace`, `--secret-name`, `--provider-config-name`, `--provider-name`, and `--provider-package`

## Logging

Set `LOG_LEVEL` to control output (default: `info`):

```bash
LOG_LEVEL=debug cargo run -- local start
```

## Development

```bash
cargo test
```
