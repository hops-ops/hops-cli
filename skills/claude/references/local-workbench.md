# Local Workbench (happy path)

Develop against the laptop control plane without learning volumes, hostPath, or sync tools.

## One-time prerequisite

Start the local control plane (optional `--gitops` = bootstrap then full
`gitops cluster` apply + watch until Ctrl+C):

```bash
hops local start --backend dory --gitops ./gitops/cluster
```

Also need `helm` and `kubectl` on your PATH.

## Daily loop

```bash
# Shared CP watch (if start did not use --gitops, or after Ctrl+C)
hops local gitops cluster ./gitops/cluster

# Per-worktree apps (Application YAMLs → hops-wt-* namespaces) — watches by default
hops local gitops worktree ./gitops/envs/local --name dogfood

# See workspaces and app URLs
hops local status

# Open the UI in a browser
hops local open

# When finished
hops local down
# Optional: delete the namespace too
hops local down --purge
```

Watch is the default for both gitops commands. Use `--once` for a single reconcile (CI/scripts).

## Concurrent worktrees

Use a distinct name per worktree so namespaces and URLs stay isolated:

```bash
# Terminal A
hops local gitops worktree ./gitops/envs/local --name alice

# Terminal B
hops local gitops worktree ./gitops/envs/local --name bob

hops local status
hops local down --name alice
hops local down --name bob
```

Each name maps to namespace `hops-wt-<name>` and gets its own access URLs.

## Dogfood: e2e-ui

```bash
cd distributed/tests/e2e-ui
hops local start --backend dory --gitops ./gitops/cluster
hops local gitops cluster ./gitops/cluster
hops local gitops worktree ./gitops/envs/local --name e2e
hops local status
hops local open
hops local down --name e2e --purge
```

Charts live under `api/.gitops/deploy` and `ui/.gitops/deploy`. You can also render them without hops:

```bash
helm template api ./api/.gitops/deploy --set local=true --set appRuntime=cluster-dev
helm template ui ./ui/.gitops/deploy --set local=true --set appRuntime=cluster-dev
```

## Layout

```text
gitops/
  cluster/          # shared CP (one per machine) — hops local gitops cluster
  envs/local/       # app Applications — hops local gitops worktree
```

- **cluster** — not per-worktree; packages + platform XRs on the local CP
- **worktree** — env Application YAMLs into isolated `hops-wt-*` namespaces

## Developing configs & providers on this CP

Iterate from **source** while keeping the cluster gitops tree coherent:

```bash
# Stack XRDs / compositions
hops config install --path xrs/stacks/k8s/auth --gitops ./gitops/cluster --local

# Provider implementation (SemVer-safe vMAJOR.999.N + ImageConfig in gitops)
hops provider install --path /path/to/provider-helm --gitops ./gitops/cluster
```

Source installs write **`imageconfigs/`** rewrites; published installs do not.
Provider source tags must stay **`vMAJOR.999.N`** (not bare `dev-sha`) so
Configuration deps like `>=v1` still resolve.

See [local-source-packages.md](./local-source-packages.md).

## Advanced

- `--once` on either gitops subcommand for one-shot / CI
- Compose-style host run remains available via e2e-ui `make up` / `make run`
