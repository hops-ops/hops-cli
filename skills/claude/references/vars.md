# Vars Reference

`hops vars` is the parallel of `hops secrets` for **non-secret** config — values
that drive workflows but don't need encryption (subnet IDs, account IDs, IAM
role ARNs, S3 bucket names). Files are committed to git in cleartext; the CLI
syncs them to GitHub Actions repository variables via `gh variable set`.

## Why a separate command

`hops secrets` round-trips through SOPS + AWS Secrets Manager because secrets
need encryption at rest and in version control. Variables don't — they're
public-by-design (anyone with read access to the repo can already see them via
`gh variable list`). Putting them through SOPS would just be ceremony.

## Configuration

`.hops.yaml` at the repo root (typically the `hops` meta-repo, but works in any
directory):

```yaml
vars:
  dir: vars                # local directory, default `vars`
  github:
    owner: hops-ops        # GitHub owner/org
    path: github           # subdir under `dir/`
    shared:
      path: _shared        # subdir of "synced everywhere" vars
      repos:
        - psql-stack       # repos that receive the shared bundle
        - aws-observe-stack
```

## Layout

```
vars/
  github/
    _shared/                    # synced to every repo in shared.repos
      ADMIN_ROLE_ARN
      PRIVATE_SUBNET_ID_A
      PRIVATE_SUBNET_ID_B
    psql-stack/                 # repo-specific overrides + extras
      EXTRA_FOR_PSQL_ONLY
    aws-observe-stack/
      SPOT_FEED_BUCKET_NAME
```

- One file per variable. Filename → variable name (uppercased,
  non-alphanumerics → `_`). File contents (trimmed) → variable value.
- Per-repo dir overrides shared on name collision.
- Subdirectories are flattened: `path/to/file` → `PATH__TO__FILE`.

## Commands

```bash
# Bootstrap config + dir layout
hops vars init --owner hops-ops

# Show local + remote diff (per repo)
hops vars list

# Push everything to GitHub
hops vars sync github -y

# Limit to specific repos
hops vars sync github --repo psql-stack --repo aws-observe-stack
```

`hops vars list` calls `gh variable list --json name --repo OWNER/REPO`.
`hops vars sync github` calls `gh variable set NAME --repo OWNER/REPO` with
the value piped on stdin.

## When to use vars vs secrets

| Use vars for | Use secrets for |
|---|---|
| Subnet IDs, VPC IDs, account IDs | API tokens, signing keys |
| IAM role ARNs (no auth value, just identity) | Database passwords, webhook URLs |
| S3 bucket names, region names | Anything you wouldn't put in a public commit |
| Anything already visible in workflow logs | Anything that grants access on its own |

In a GitHub Actions workflow, vars are referenced as `${{ vars.NAME }}`,
secrets as `${{ secrets.NAME }}`.
