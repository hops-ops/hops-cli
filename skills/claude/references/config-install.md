# Config Install Reference

> **Developing stacks or providers from source on a local CP?** Start with
> [local-source-packages.md](./local-source-packages.md) — published vs source,
> ImageConfigs, and the provider `vMAJOR.999.N` SemVer rule.

## Two Install Modes

### Source-build mode (`--path` or `--repo` without `--version`)

Builds an Upbound-format XRD project locally and pushes through the local registry.
Intended for local control planes started with `hops local start`.

```bash
# Build from current directory
hops config install

# Build from explicit path
hops config install --path /path/to/project

# Build from GitHub repo (interactive: choose source or published)
hops config install --repo hops-ops/aws-auto-eks-cluster
```

### Iterating on local changes

Each source build is tagged with a unique `dev-<sha256>` derived from the `.uppkg`
content, so the Configuration's `spec.package` changes on every build. To pick up
edits, re-run the same install command — Crossplane sees the new package ref and
creates a fresh ConfigurationRevision. **No flag is needed to force a rebuild**:
just run `hops config install --path <dir>` again.

For an even tighter loop, use `--watch` to re-run install automatically on save:

```bash
hops config install --path /path/to/project --watch
```

**What happens:**
1. Runs `up project build` to create `.uppkg` artifacts
2. Loads images via `docker load`
3. Ensures registry **host access** the same way workbench/gitops exposes services: map-mode `kubectl port-forward -n crossplane-system svc/registry 30500:5000` on `127.0.0.1` (PID under `~/.hops/local/`). Then pushes images to `127.0.0.1:30500`. No dory fork / NodePort publish required.
4. Creates ImageConfig rewrites so Crossplane pulls from the in-cluster registry
5. Patches the configuration package metadata with local render digests
6. Applies the Configuration resource

### Remote-package mode (`--repo ... --version ...`)

Applies a pinned package reference directly. Works against any connected cluster.

```bash
hops config install --repo hops-ops/aws-auto-eks-cluster --version v0.11.0
```

**What happens:**
1. Deletes any stale render Function packages from previous installs
2. Deletes any local ImageConfig rewrites left from source builds
3. Deletes inactive ConfigurationRevisions pointing at the local registry
4. Applies Configuration with `ghcr.io/<org>/<repo>:<version>`

## Cluster gitops: `--gitops` (+ `--local`)

Prefer this when dogfooding a **local control plane** so package pins live in
gitops (not only as one-shot kubectl applies).

```bash
# 1. Bootstrap CP (creates helm/k8s ProviderConfigs named "default")
hops local start --backend dory

# 2. Install published stacks + write package YAML under gitops/cluster
hops config install --repo hops-ops/psql-stack --version v0.9.1 \
  --gitops ./gitops/cluster --local

hops config install --repo hops-ops/auth-stack --version v1.6.0 \
  --gitops ./gitops/cluster --local

# 3. Day-to-day: apply/watch the tree (packages + XRs)
hops local gitops cluster ./gitops/cluster
# or: hops local start --backend dory --gitops ./gitops/cluster
```

| Flag | Effect |
|------|--------|
| `--gitops PATH` | After install, write package gitops under `PATH` (see modes below) |
| `--local` | Requires `--gitops`. Also scaffolds local XR YAMLs when known (e.g. `psql/stack.yaml`, `auth/stack.yaml`) with **`helmProviderConfigRef` / `kubernetesProviderConfigRef` → `name: default`**. Does **not** overwrite existing XR files. |

### Published vs source under `--gitops`

| Mode | Writes |
|------|--------|
| **`--repo --version`** (published) | `packages/<name>.yaml` only (ghcr pin) |
| **`--path` / source** | `packages/<name>.yaml` (local registry `dev-*` ref, `packagePullPolicy: Always`) **and** `imageconfigs/*.yaml` (ImageConfig rewrites so Crossplane pulls render functions from the in-cluster registry) |

Source packages **need** those ImageConfigs in gitops. Without them, `hops local gitops cluster` re-apply cannot resolve function packages. Switching back to published: remove stale `imageconfigs/hops-local-rewrite-*` files (or re-run `--repo --version --gitops` and delete them manually for now).

### Why `--local` ProviderConfigs?

`hops local start` installs Helm + Kubernetes ProviderConfigs named **`default`**.
Stack XRs default provider config names from `spec.clusterName` (e.g. `dory`), which
does **not** exist unless you create matching ProviderConfigs.

Local XR scaffolds therefore pin:

```yaml
helmProviderConfigRef:
  name: default
kubernetesProviderConfigRef:
  name: default
```

`clusterName` remains a logical label (and may match your backend name).

### What gets written

```text
gitops/cluster/
  packages/psql-stack.yaml       # Configuration (published or local registry)
  packages/auth-stack.yaml
  imageconfigs/hops-local-rewrite-….yaml   # source builds only
  psql/stack.yaml                # only with --local, if missing
  auth/stack.yaml                # only with --local, if missing
```

Re-apply anytime with `hops local gitops cluster ./gitops/cluster` (watches by default).

```bash
# Develop auth-stack XRs from source + keep gitops coherent
hops config install --path xrs/stacks/k8s/auth --gitops ./gitops/cluster --local
hops local gitops cluster ./gitops/cluster --once
```

## Provider install (`hops provider install`) + gitops

Same published vs source idea. **Source builds use a SemVer-compatible tag** so
Configuration deps like `>=v1` keep resolving:

| Mode | `spec.package` tag | Gitops extras |
|------|-------------------|---------------|
| **Published** `--repo --version` | real release (`v1.3.0`) | `providers/` + `runtime/` only |
| **Source** `--path` | **`vMAJOR.999.<N>`** (auto-increment) on the **upstream** URL prefix | + `imageconfigs/` rewrite to local registry |

Crossplane’s dep manager records the **upstream** package URL in the Lock;
ImageConfig only rewrites **fetch**. Never write a bare `dev-<sha>` tag into
gitops for providers that Configurations depend on — Masterminds/semver treats
prereleases specially and `>=v1` will not match.

```bash
# Bootstrap first so an upstream Provider exists to patch
hops local start --gitops ./gitops/cluster

# Iterate on provider-helm from source (preserves v1.999.N + ImageConfig in gitops)
hops provider install --path /path/to/provider-helm --gitops ./gitops/cluster
# or force major: --version-prefix v1
```

## Switching Between Local and Published

The CLI handles cleanup automatically when switching modes:

- **Local → Published**: Stale Functions, ImageConfig rewrites, and inactive local
  ConfigurationRevisions are deleted so Crossplane re-resolves with the correct
  published digests.
- **Published → Local**: Existing Functions are deleted before pushing new local images
  to avoid digest conflicts.

## Configuration Naming

Configurations are named `<org>-<repo>`, e.g. `hops-ops-aws-secret-stack`.
This matches both local and published installs. Gitops package **filenames** use
the short package name (`psql-stack.yaml`); `metadata.name` matches the applied
Configuration.

## Uninstall

```bash
# By name
hops config uninstall --name hops-ops-aws-auto-eks-cluster

# By repo
hops config uninstall --repo hops-ops/aws-auto-eks-cluster

# By path (derives names from build artifacts)
hops config uninstall --path /path/to/project
```

Uninstall waits for lock reconciliation and prunes orphaned packages (Configurations,
Functions, Providers) and ImageConfig rewrites.

## Flags

| Flag | Applies to | Purpose |
|------|-----------|---------|
| `--path` | Source build | Path to local XRD project |
| `--repo` | Both modes | GitHub `<org>/<repo>` |
| `--version` | Remote mode | Version tag (e.g. `v0.11.0`) |
| `--gitops PATH` | All modes | Write Configuration YAML under cluster gitops tree |
| `--local` | With `--gitops` | Scaffold local XRs using ProviderConfig `default` |
| `--watch` | Source build | Re-run install on filesystem changes |
| `--debounce` | Used with `--watch` | Quiet interval in seconds before rebuild (default 15) |
| `--skip-dependency-resolution` | All modes | Set `spec.skipDependencyResolution=true` |
| `--context` | All modes | Kubernetes context (e.g. `colima`) |
