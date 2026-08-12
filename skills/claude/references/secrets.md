# Secrets Management

## Overview

`hops secrets` manages repo-level secrets using SOPS for encryption and syncs
to AWS Secrets Manager or GitHub repository secrets.

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
secrets-encrypted/    # SOPS-encrypted (committed)
  aws/
  github/
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
