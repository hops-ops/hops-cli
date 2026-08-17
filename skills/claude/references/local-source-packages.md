# Local source: Configuration packages & Providers

Use this when **developing or dogfooding** Crossplane packages on a laptop CP
(`hops local start`), not when consuming published tags only.

Always pair installs with **`--gitops ./gitops/cluster`** so
`hops local gitops cluster` can re-apply the same pins (and ImageConfigs).

## Mental model

| | Configuration packages (`hops config install`) | Providers (`hops provider install`) |
|--|-----------------------------------------------|-------------------------------------|
| **Published** | `ghcr.io/…:vX.Y.Z` | same |
| **Source** | local registry `dev-<hash>` tag + **ImageConfig** rewrites for render functions | **upstream URL** + **`vMAJOR.999.N`** tag + **ImageConfig** rewrite + runtime image |
| **Why ImageConfig** | Crossplane must pull function packages from the in-cluster registry | Same; Lock still records **upstream** package URL |
| **SemVer / deps** | Configurations depend on providers with `>=vMAJOR` | Source tags **must** be real SemVer (`v1.999.3`), **not** bare `dev-sha` (prereleases fail `>=v1`) |

ImageConfig **rewrites fetch only**. Package identity in the Lock stays the
upstream path — do not put `registry.crossplane-system…` in `spec.package` for
providers that Configurations depend on.

## Prerequisites

```bash
hops local start --cluster-provider kind --docker-provider dory \
  --cluster-name hops --gitops ./gitops/cluster
# Creates: Crossplane, helm/k8s providers (pinned), ProviderConfigs named "default",
# local OCI registry, and writes bootstrap YAML under gitops/cluster/
```

ProviderConfigs for stack XRs: always pin `helmProviderConfigRef` /
`kubernetesProviderConfigRef` to **`name: default`** on a local CP.

## Configuration packages (XRD stacks)

### Published (stable dogfood)

```bash
hops config install --repo hops-ops/auth-stack --version v1.6.0 \
  --gitops ./gitops/cluster --local
hops config install --repo hops-ops/psql-stack --version v0.9.1 \
  --gitops ./gitops/cluster --local
```

Writes: `packages/<name>.yaml` only (+ optional XR scaffolds with `--local`).

### Source (edit compositions / XRDs)

```bash
# From meta (or any checkout of the Upbound project)
hops config install --path xrs/stacks/k8s/auth --gitops ./gitops/cluster --local
# tight loop:
hops config install --path xrs/stacks/k8s/auth --watch --gitops ./gitops/cluster
```

Writes:

```text
packages/auth-stack.yaml          # local registry pull ref, packagePullPolicy: Always
imageconfigs/hops-local-rewrite-*.yaml   # REQUIRED for source — do not omit
auth/stack.yaml                   # --local only, if missing
```

Then:

```bash
hops local gitops cluster ./gitops/cluster --once   # or leave watching
```

### Switch source ↔ published

- **→ published:** `hops config install --repo … --version … --gitops …`  
  CLI cleans stale Functions / ImageConfigs on the cluster; **delete** stale
  `gitops/cluster/imageconfigs/hops-local-rewrite-*` if left from source.
- **→ source:** `hops config install --path … --gitops …` rewrites package + imageconfigs.

## Providers

`hops provider install` **patches an existing Provider** (from bootstrap or
gitops). It does not create a brand-new Provider from nothing.

### Published

```bash
hops provider install --repo crossplane-contrib/provider-helm --version v1.3.0 \
  --gitops ./gitops/cluster
```

### Source (preserve SemVer for Configuration deps)

```bash
# Existing cluster Provider must already be the upstream one (e.g. from start)
hops provider install --path /path/to/provider-helm --gitops ./gitops/cluster

# Force major for >=v1 constraints (same 999 scheme):
hops provider install --path /path/to/provider-helm --version-prefix v1 \
  --gitops ./gitops/cluster
```

Source **`spec.package`** looks like:

```text
xpkg.crossplane.io/crossplane-contrib/provider-helm:v1.999.3
```

not `…:dev-abc` and not `registry.crossplane-system…/…`.

Writes:

```text
providers/<name>.yaml
runtime/<name>.yaml              # DRC + ClusterRoleBinding
imageconfigs/hops-local-rewrite-*.yaml   # source only
```

## Auth-stack XR surface (while developing from source)

| XR | Use |
|----|-----|
| **AuthStack** | Install Zitadel platform |
| **MachineUser** | Machine identity + optional PAT |
| **Grant** | User → project roles (same-org / cross-org) — prefer over raw grant MRs |
| Provider MRs | Project, Role, HumanUser, Oidc app — thin 1:1 types; no hops HumanUser XR |

Thin wrappers that only compose one MR are misdirection; Grant is multi-path and worth it.

## Cluster gitops tree (typical)

```text
gitops/cluster/
  providers/helm.yaml kubernetes.yaml   # start --gitops and/or provider install
  providerconfigs/helm.yaml kubernetes.yaml
  runtime/…
  packages/…                      # config install
  imageconfigs/…                  # source config or provider only
  auth/stack.yaml psql/stack.yaml # platform XRs
```

Day-to-day:

```bash
hops local gitops cluster ./gitops/cluster          # watches by default
hops local gitops environment ./.gitops/local/environment.yaml --name dogfood
```

## Do / don’t

**Do**

- Use `--gitops` whenever installing for a shared cluster tree
- Keep source provider tags as `vMAJOR.999.N`
- Re-run install after source changes (`--watch` optional)
- Pin local XRs to ProviderConfig `default`

**Don’t**

- Commit bare `dev-<sha>` provider tags into gitops when stacks depend on `>=vN`
- Drop ImageConfigs for source packages/providers and expect gitops re-apply to work
- Put machine backend choice in project `.hops.yaml` (use `~/.hops/local/backend`)

## See also

- [config-install.md](./config-install.md) — flags and mode details  
- [local-workbench.md](./local-workbench.md) — Cluster vs Environment gitops
- [local-setup.md](./local-setup.md) — `hops local start` bootstrap  
