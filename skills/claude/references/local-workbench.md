# Local Workbench (happy path)

Develop against the laptop control plane without learning volumes, hostPath, or sync tools.

## One-time prerequisite

Also need `helm`, `kubectl`, and `kind` on your PATH (for the preferred Mac path).

### Preferred on Mac (live edit / near-native HMR)

Use **kind** as the Kubernetes node provider and **Dory** as the docker engine.
hops mounts your home directory into the kind node so source delivery can use
**hostPath** (edit on the Mac → files appear in cluster-dev pods without a full
tree copy). You do not need to learn volume types.

```bash
# Dory app running (engine healthy). Product Dory Kubernetes is optional.
# Preferred flags (cluster-provider + docker-provider):
hops local start --cp kind --dp dory --gitops ./gitops/cluster
# Deprecated alias still works: --backend kind (auto-picks dory.sock when present)
```

Context is typically `kind-hops`. Confirm mounts:

```bash
hops local doctor   # "kind node projects-root mount (hostPath capable)"
```

**Changing mounts:** recreate the kind cluster — `hops local reset --backend kind`
(or destroy + start). Existing clusters created without mounts will not pick up
home mounts until reset.

### Alternative: product Dory Kubernetes

Stock Dory k8s (`--backend dory`) is fine for platform experiments but usually
**cannot** hostPath-mount Mac paths into the node; delivery falls back to sync.

```bash
hops local start --backend dory --gitops ./gitops/cluster
```

## Daily loop

```bash
# Shared CP watch (if start did not use --gitops, or after Ctrl+C)
hops local gitops cluster ./gitops/cluster

# Per-worktree apps (Application YAMLs → namespace = --name) — watches by default
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

Each name maps to namespace `<name>` and gets its own access URLs.

## Dogfood: e2e-ui

```bash
cd distributed/tests/e2e-ui
# Prefer kind-on-Dory for hostPath HMR (see One-time prerequisite)
hops local start --backend kind --gitops ./gitops/cluster
hops local gitops cluster ./gitops/cluster
hops local gitops worktree ./gitops/envs/local --name dogfood
# or: hops local up ./gitops/envs/local --name dogfood
hops local status
hops local open
hops local down --name dogfood --purge
```

Charts live under `api/.gitops/deploy` and `ui/.gitops/deploy`. You can also render them without hops:

```bash
helm template api ./api/.gitops/deploy --set local=true --set appRuntime=cluster-dev
helm template ui ./ui/.gitops/deploy --set local=true --set appRuntime=cluster-dev
```

### Agent rules (do not skip)

Dogfood apps run as **`appRuntime: cluster-dev`** in namespace `= --name` with
source delivery into the pods. The site you must fix is **that** stack — not a
host `make run` you invent.

**When the dogfood site is broken:**

1. **Confirm runtime first** (`KUBECONFIG` = dory, e.g. `~/.kube/dory-config`):
   ```bash
   kubectl -n dogfood get pods
   kubectl -n dogfood logs deploy/e2e-ui-api --tail=40
   kubectl -n dogfood logs deploy/e2e-ui-ui --tail=40
   ```
2. **Compile before theorizing.** API is `cargo run` in a rust image; UI is
   `npm install` + `js` build + `vite` in a node image. `rollout restart` alone
   is not “fixed” until:
   - API log shows `listening on http://0.0.0.0:8791` (not mid-`Compiling`)
   - UI log shows Vite ready
3. **Hit the real URLs** (cluster DNS / host FQDN path), not only pod logs:
   ```bash
   UI=http://e2e-ui-ui.dogfood.svc.cluster.local:5180
   API=http://e2e-ui-api.dogfood.svc.cluster.local:8791
   curl -sS -o /dev/null -w '%{http_code}\n' "$UI/" "$UI/chat"
   curl -sS -X POST "$API/graphql" -H 'content-type: application/json' \
     -d '{"query":"{ __typename }"}'
   ```
4. After service / GraphQL / pure / client changes on the host worktree, run
   **`make gen-client`** in `tests/e2e-ui` (js build + wasm + client generate)
   so delivery syncs artifacts the UI pod will load. Then restart pods and
   **wait for compile**.
5. Symptoms that are usually **stale binary / mid-compile / stale clients**, not
   a roles redesign: `replica artifact schema does not match…`,
   `createWasmJsonPure is not a function`, surface open errors that unit tests
   pass for. Recompile + re-hit the site first.
6. **Do not** paper over with more chart churn, host-only Makefile “watch”
   experiments, or long GraphQL protocol essays when the pod never finished
   building. **Do not** declare success without curling the live UI paths.

**Kubeconfig:** prefer `~/.kube/dory-config` for dory; map host access uses
cluster FQDNs (`*.svc.cluster.local`), not `localhost` alone.

## Layout

```text
gitops/
  cluster/          # shared CP (one per machine) — hops local gitops cluster
  envs/local/       # app Applications — hops local gitops worktree
```

- **cluster** — not per-worktree; packages + platform XRs on the local CP
- **worktree** — env Application YAMLs into namespace `= --name`

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
