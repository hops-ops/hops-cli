# Secrets Management

## Overview

`hops secrets` manages repo-level secrets using SOPS for encryption and syncs
to AWS Secrets Manager, GitHub repository secrets, or HashiCorp Vault (KV).

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
  vault/              # → hops secrets sync vault (KV paths)
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
    address: http://127.0.0.1:8200   # or $VAULT_ADDR
    mount: secret                     # KV mount
    version: v2
    path_prefix: ""                   # optional prefix on every remote path
    token_env: VAULT_TOKEN
    kube:                             # port-forward when address is down
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

## Sync to HashiCorp Vault (KV)

```bash
export VAULT_TOKEN=root   # local SecretStack dev Vault; never commit
hops secrets sync vault
hops secrets sync vault --secret-path secrets/vault/e2e-ui/dogfood -y
hops secrets sync vault --port-forward   # force kubectl tunnel to in-cluster Vault
```

### Vault naming rules (same roll-up as AWS)

| Source | Vault KV path (mount `secret`) |
|--------|--------------------------------|
| `secrets/vault/e2e-ui/dogfood/oidc.json` | `e2e-ui/dogfood/oidc` (JSON object → properties) |
| `secrets/vault/e2e-ui/dogfood/human-passwords/{alice,bob}` | `e2e-ui/dogfood/human-passwords` |
| `secrets/vault/auth/zitadel-masterkey/masterkey` | `auth/zitadel-masterkey` property `masterkey` |

- Paths match ExternalSecret `remoteRef.key` (no `secret/data/` prefix)
- Writer token is separate from ESO’s read-only kubernetes auth role
- Unchanged remote maps are skipped (compare-before-write)
- Local Vault chart is often in-memory: re-run sync after `vault-0` restarts; keep SOPS plaintext as the durable copy
