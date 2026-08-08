---
name: hops
description: |
  Hops CLI and Crossplane platform toolkit. Use when working with hops commands,
  Crossplane configuration packages, provider install (local source or published),
  local control plane / gitops workbench, XR lifecycle (observe/adopt/manage),
  secrets (SOPS + AWS SM), or iterating on stacks from source with
  `hops config install --path` / `hops provider install --path` and `--gitops`.
---

# Hops CLI

`hops` is a CLI for Crossplane development and XR lifecycle workflows. It manages local
control planes, configuration packages, providers, secrets, and live infrastructure adoption.

## Quick Reference

| Command area | Purpose |
|-------------|---------|
| `hops local` | Local CP (dory/colima/kind), gitops cluster/worktree, workbench |
| `hops config` | Install Configuration packages (published **or** source + gitops) |
| `hops provider` | Install/patch Providers (published **or** source + SemVer-safe tags + gitops) |
| `hops secrets` | SOPS encrypt/decrypt, sync to AWS Secrets Manager or GitHub |
| `hops vars` | Declarative non-secret config (e.g. GitHub Actions repo variables) |
| `hops xr` | Observe/adopt/manage/orphan existing infrastructure |
| `hops validate` | Generate configuration manifests for validation |

## Key Workflows

For detailed reference on each area, see the bundled references:

- **[Local source packages & providers](references/local-source-packages.md)** — **read this** when developing configs/providers on a laptop CP
- [Config install modes and gitops](references/config-install.md)
- [Local workbench (gitops cluster / worktree)](references/local-workbench.md)
- [Local control plane setup](references/local-setup.md)
- [XR observe → adopt → manage workflow](references/xr-workflow.md)
- [Secrets management](references/secrets.md)
- [Vars management (declarative GH Actions variables)](references/vars.md)
- [Available stacks and XRs](references/stacks-and-xrs.md)
- [Debugging with kubectl](references/debugging.md)

---

## Local control plane + platform packages (dogfood)

```bash
hops local start --backend dory --gitops ./gitops/cluster
# bootstrap writes helm/k8s providers + ProviderConfigs (default) into the tree,
# then runs cluster gitops (apply + watch)

hops config install --repo hops-ops/psql-stack --version v0.9.1 \
  --gitops ./gitops/cluster --local
hops config install --repo hops-ops/auth-stack --version v1.6.0 \
  --gitops ./gitops/cluster --local

hops local gitops cluster ./gitops/cluster          # watches by default; --once for CI
hops local gitops worktree ./gitops/envs/local --name dogfood
```

- **`--gitops`** materializes pins under the cluster tree (not one-shot-only kubectl)
- **`--local`** on config install scaffolds stack XRs with ProviderConfig **`default`**
- Backend preference is **user-local** (`~/.hops/local/backend`), not `.hops.yaml`

---

## Developing packages from source (must know)

When changing XRDs/compositions or a provider implementation on the local CP:

### Configuration packages (stacks)

```bash
hops config install --path xrs/stacks/k8s/auth --gitops ./gitops/cluster --local
# or --watch for rebuild-on-save
```

| Mode | Gitops writes |
|------|----------------|
| Published `--repo --version` | `packages/*.yaml` only |
| Source `--path` | `packages/*.yaml` **+** `imageconfigs/*` (required) |

Without ImageConfigs, source function packages will not pull from the local registry on gitops re-apply.

### Providers

```bash
# Cluster must already have the upstream Provider (e.g. from start)
hops provider install --path /path/to/provider-helm --gitops ./gitops/cluster
```

| Mode | `spec.package` | Gitops |
|------|----------------|--------|
| Published | real tag `v1.3.0` | `providers/` + `runtime/` |
| Source | **upstream URL** + **`vMAJOR.999.N`** (not bare `dev-sha`) | + `imageconfigs/` |

**Why `v1.999.N`:** Configuration deps use `>=vMAJOR`. Masterminds/semver **excludes prereleases**, so `dev-abc` or `v1.999.1-dev-sha` break dep resolution. ImageConfig rewrites **fetch** only; Lock source stays the upstream package path.

Optional: `--version-prefix v1` to force major for the 999 scheme.

### Auth-stack XR surface

| XR | When |
|----|------|
| **AuthStack** | Platform Zitadel install |
| **MachineUser** | Machine identity + optional PAT |
| **Grant** | Roles on a project (same/cross-org) — prefer over raw grant MRs |
| Provider MRs | Project, HumanUser, Oidc app — no thin 1:1 hops HumanUser wrapper |

Full detail: [local-source-packages.md](references/local-source-packages.md).

---

## Crossplane Conventions

- **Crossplane 2+**: Use `managementPolicies`, never `deletionPolicy` on managed resources
- **Packages**: Prefer `crossplane-contrib` packages over Upbound-hosted ones (paid-account restrictions)
- **Commits**: Conventional Commits (`feat:`, `fix:`, `chore:`) with subjects under 72 chars
- **XRD projects**: Use Upbound-format projects with `upbound.yaml`, `apis/`, `functions/`, `tests/`
- **Testing**: `make render` for quick validation, `up test run tests/test-render` for unit tests, `up test run tests/e2etest-* --e2e` for E2E
