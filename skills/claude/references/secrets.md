# Secrets Management

## Overview

`hops secrets` manages repo-level secrets using SOPS for encryption and syncs
to AWS Secrets Manager, GitHub repository secrets, or HashiCorp Vault KV.

## Setup

```bash
hops secrets init
```

Creates the directory structure, `.sops.yaml`, and `.hops.yaml` configuration.

### Directory Layout

```
secrets/              # Plaintext (gitignored)
  aws/
  github/
    _shared/
  vault/
secrets-encrypted/    # SOPS-encrypted (committed)
  aws/
  github/
  vault/
```

### Configuration (`.hops.yaml`)

```yaml
secrets:
  plaintext_dir: secrets
  encrypted_dir: secrets-encrypted
  aws:
    path: aws
    region: us-east-2
    tags:
      hops.ops.com.ai/secret: "true"
  github:
    owner: hops-ops
    path: github
    shared_secrets:
      path: _shared
      repos:
        - repo-a
        - repo-b
  vault:
    path: vault
    address: http://127.0.0.1:8200
    mount: secret
    version: v2
    token_env: VAULT_TOKEN
    kube:
      enabled: true
      namespace: vault
      service: vault
      local_port: 8200
```

## Encrypt / Decrypt

```bash
hops secrets encrypt    # Encrypts secrets/ → secrets-encrypted/
hops secrets decrypt    # Decrypts secrets-encrypted/ → secrets/
```

Uses the KMS ARN from `.sops.yaml` for SOPS operations.

## Sync to AWS Secrets Manager

```bash
hops secrets sync aws
```

### AWS Naming Rules

| Source | AWS Secret |
|--------|-----------|
| `secrets/aws/app.json` | Secret `app` (JSON stored as-is) |
| `secrets/aws/github/token` + `secrets/aws/github/owner` | Secret `github` (directory roll-up) |
| `secrets/aws/slack/.env` with `WEBHOOK_URL=...` | Secret `slack` (env parsed to JSON) |

- `.json` files → one secret with JSON stored as-is
- Directories → one secret, each filename becomes a JSON key
- `.env` files → parsed into key/value pairs, stored as one JSON secret
- `--cleanup` removes secrets not in the plaintext tree (only works from full root)
- Tag `hops.ops.com.ai/secret=true` is always applied

## Sync to GitHub Repository Secrets

```bash
hops secrets sync github
```

### GitHub Naming Rules

| Source | GitHub Secret |
|--------|-------------|
| `secrets/github/repo-a/NPM_TOKEN` | `NPM_TOKEN` in `repo-a` |
| `secrets/github/repo-a/actions.json` with `{"SLACK_WEBHOOK":"..."}` | `SLACK_WEBHOOK` in `repo-a` |
| `secrets/github/_shared/ORG_TOKEN` | `ORG_TOKEN` in all configured repos |

- Each file → separate GitHub secret (no roll-up like AWS)
- `.json` files → one secret per top-level key
- `.env` files → one secret per `KEY=value` entry
- Shared secrets fan out to all repos in `shared_secrets.repos`
- Repo-specific values override shared values

## Sync to HashiCorp Vault KV

```bash
export VAULT_TOKEN=root # local development only
hops secrets sync vault --no-port-forward --yes
```

| Source | Vault KV path |
|--------|---------------|
| `secrets/vault/harmony/stripe/.env` | `harmony/stripe` |
| `secrets/vault/harmony/oidc.json` | `harmony/oidc` |

- Vault inputs must be untracked and gitignored beneath the configured root.
- `.json` objects map to one path; plain files and `.env` entries roll up by directory.
- KV v1/v2 and an optional remote `path_prefix` are supported.
- Values travel in the HTTP body and are never placed in argv, logs, or Hops state.
- Hops compares before writing, does not prune unspecified paths, and can open a quiet kubectl port-forward to local Vault.
