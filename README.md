# hops-cli

`hops-cli` is a Rust CLI for running a local Crossplane development environment on Colima.

## Overview

This tool manages a local Kubernetes stack and package workflow for Crossplane:

- Installs and manages Colima
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
```

From source without installing:

```bash
cargo run -- --help
cargo run -- local --help
```

## Quick Start

```bash
# 1) Install Colima (via Homebrew)
cargo run -- local install

# 2) Start local k8s + Crossplane + providers + local registry
cargo run -- local start

# 3) Build and load a Crossplane configuration package from an XRD project
cargo run -- local config /path/to/project
```

## Commands

- `local install`
  - Runs `brew install colima`.
- `local reset`
  - Runs `colima kubernetes reset`.
- `local start`
  - Runs `colima start --kubernetes --cpu 8 --memory 16 --disk 60`
  - Installs Crossplane from `crossplane-stable/crossplane`
  - Applies manifests from `bootstrap/` for runtime config, providers, provider configs, and registry
  - Configures Docker in Colima for insecure pulls from `registry.crossplane-system.svc.cluster.local:5000`
  - Adds host mapping in Colima VM for the registry service DNS name
- `local stop`
  - Runs `colima stop`.
- `local destroy`
  - Runs `colima delete --force`.
- `local uninstall`
  - Prompts for confirmation, then runs `brew uninstall colima`.
- `local config [PATH]`
  - Runs `up project build` in `PATH` (defaults to current directory)
  - Loads generated `.uppkg` artifacts from `<PATH>/_output`
  - Pushes package images to local registry (`localhost:30500`)
  - Applies Crossplane `Configuration` resources pointing at `registry.crossplane-system.svc.cluster.local:5000/...`

## Logging

Set `LOG_LEVEL` to control output (default: `info`):

```bash
LOG_LEVEL=debug cargo run -- local start
```

## Development

```bash
cargo test
```
