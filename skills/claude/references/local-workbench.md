# Local Workbench (happy path)

Develop against the laptop control plane without learning volumes, hostPath, or sync tools.

## One-time prerequisite

Start the local control plane once:

```bash
hops local start
```

Optional but useful: install [kubefwd](https://github.com/txn2/kubefwd) so service URLs look like cluster DNS. Without it, hops maps unique localhost ports automatically.

Also need `helm` and `kubectl` on your PATH.

## Daily loop

```bash
# From your project (or pass an absolute env path)
hops local up ./gitops/env/local

# See workspaces and app URLs
hops local status

# Open the UI in a browser
hops local open

# When finished
hops local down
# Optional: delete the namespace too
hops local down --purge
```

That is the full happy path: **up / status / open / down**.

## Concurrent worktrees

Use a distinct name per worktree so namespaces and URLs stay isolated:

```bash
# Terminal A
hops local up ./gitops/env/local --name alice

# Terminal B
hops local up ./gitops/env/local --name bob

hops local status
hops local down --name alice
hops local down --name bob
```

Each name maps to namespace `hops-wt-<name>` and gets its own access URLs.

## Dogfood: e2e-ui

```bash
cd distributed/tests/e2e-ui
hops local up ./gitops/env/local --name e2e
hops local status
hops local open
hops local down --name e2e --purge
```

Charts live under `api/.gitops/deploy` and `ui/.gitops/deploy`. You can also render them without hops:

```bash
helm template api ./api/.gitops/deploy --set local=true --set appRuntime=cluster-dev
helm template ui ./ui/.gitops/deploy --set local=true --set appRuntime=cluster-dev
```

## What `up` does (plain language)

1. Checks the control plane is reachable (if not: run `hops local start`)
2. Registers your workspace name → namespace
3. Applies the Applications in the env directory into that namespace
4. Attaches source delivery automatically (you do not pick a mode)
5. Prints app URLs

## Advanced (not required for day-to-day)

- `hops local gitops <env> --once` — reconcile only
- `hops local gitops <env> --watch` — re-apply when env YAML or chart files change
- Compose-style host run remains available via e2e-ui `make up` / `make run`

Prefer `up` / `down` / `status` / `open` unless you are debugging the reconcile path.
