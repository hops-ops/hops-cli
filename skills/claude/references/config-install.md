# Config Install Reference

## Two Install Modes

### Source-build mode (`--path` or `--repo`)

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
3. Pushes render function images to `localhost:30500` (local registry)
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

## Switching Between Local and Published

The CLI handles cleanup automatically when switching modes:

- **Local → Published**: Stale Functions, ImageConfig rewrites, and inactive local
  ConfigurationRevisions are deleted so Crossplane re-resolves with the correct
  published digests.
- **Published → Local**: Existing Functions are deleted before pushing new local images
  to avoid digest conflicts.

## Configuration Naming

Configurations are named `<org>-<repo>`, e.g. `hops-ops-aws-secret-stack`.
This matches both local and published installs.

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
| `--watch` | Source build | Re-run install on filesystem changes |
| `--debounce` | Used with `--watch` | Quiet interval in seconds before rebuild (default 15) |
| `--skip-dependency-resolution` | All modes | Set `spec.skipDependencyResolution=true` |
| `--context` | All modes | Kubernetes context (e.g. `colima`) |
