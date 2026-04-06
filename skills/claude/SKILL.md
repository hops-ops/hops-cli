---
name: hops
description: |
  Hops CLI and Crossplane platform toolkit. Use when working with hops commands,
  Crossplane configuration packages, XR lifecycle workflows (observe/adopt/manage),
  secrets management (SOPS + AWS Secrets Manager), or local control plane setup.
---

# Hops CLI

`hops` is a CLI for Crossplane development and XR lifecycle workflows. It manages local
control planes, configuration packages, secrets, and live infrastructure adoption.

## Quick Reference

| Command area | Purpose |
|-------------|---------|
| `hops local` | Local Colima-based control plane setup |
| `hops config` | Build, install, and uninstall Crossplane configuration packages |
| `hops secrets` | SOPS encrypt/decrypt, sync to AWS Secrets Manager or GitHub |
| `hops xr` | Observe/adopt/manage/orphan existing infrastructure |
| `hops validate` | Generate configuration manifests for validation |

## Key Workflows

For detailed reference on each area, see the bundled references:

- [Config install modes and local/published switching](references/config-install.md)
- [XR observe → adopt → manage workflow](references/xr-workflow.md)
- [Secrets management](references/secrets.md)
- [Local control plane setup](references/local-setup.md)
- [Available stacks and XRs](references/stacks-and-xrs.md)
- [Debugging with kubectl](references/debugging.md)

## Crossplane Conventions

- **Crossplane 2+**: Use `managementPolicies`, never `deletionPolicy` on managed resources
- **Packages**: Prefer `crossplane-contrib` packages over Upbound-hosted ones (paid-account restrictions)
- **Commits**: Conventional Commits (`feat:`, `fix:`, `chore:`) with subjects under 72 chars
- **XRD projects**: Use Upbound-format projects with `upbound.yaml`, `apis/`, `functions/`, `tests/`
- **Testing**: `make render` for quick validation, `up test run tests/test-render` for unit tests, `up test run tests/e2etest-* --e2e` for E2E
