# Residual local secrets

These inputs are shared by every locally registered Environment using the
Cluster.

Tracked GitOps files contain names and keys only. They do not contain
masterkeys, PATs, OIDC credentials, Stripe credentials, or application
secrets.

## Cluster secret backend

`.gitops/local/cluster/secrets/stack.yaml` installs External Secrets Operator
and a durable, in-cluster Vault. The `vault` `ClusterSecretStore` is shared by
all worktree namespaces. Vault persists on its own PVC; deleting that PVC is a
full local secret reset.

Plaintext local inputs live only under the root `secrets/` directory, which is
gitignored. No SOPS step is required because no encrypted secret is committed.
`make local-gitops` generates or reuses stable shared application values,
resolves the same per-developer Stripe sandbox used by `make dev`,
verifies/provisions its catalog, and writes:

```text
secrets/vault/harmony/stripe/.env
secrets/vault/harmony/local/application/.env
```

The application path contains the shared local service/session tokens and any
optional PostHog settings. The Stripe path contains `STRIPE_API_KEY`, `STRIPE_WEBHOOK_SECRET`,
`STRIPE_PORTAL_CONFIG_ID`, and `PUBLIC_STRIPE_PUBLISHABLE_KEY`. It then syncs
the ignored directory after SecretStack is Ready. To repeat only those phases:

```bash
make dev-gitops-vault-secrets
make dev-gitops-vault-sync
```

The sync is idempotent. The Hops Cluster controller repeats the configured
ignored-directory sync before every Environment reconcile and watches that
directory for changes. Harmony reads Vault's locally persisted writer token
from the explicitly selected Kubernetes context and passes it through the
`VAULT_TOKEN` environment variable without printing it. Hops owns the
kubectl port-forward lifecycle and KV v2 synchronization through
`hops secrets sync vault`.

## Phase 1: cluster bootstrap

Create the AuthStack masterkey and MinIO credentials before starting the
Cluster controller:

```bash
make dev-gitops-cluster-secrets
```

The script is idempotent and preserves generated infrastructure values. It
creates:

| Namespace | Secret | Keys |
|---|---|---|
| `auth` | `zitadel-masterkey` | `masterkey` |
| `harmony-system` | `harmony-minio` | `MINIO_ROOT_USER`, `MINIO_ROOT_PASSWORD` |
| `default` | `harmony-local-human-passwords` | `approved-admin`, `waitlisted`, `iac-approved`, `device-login`, `fixture-owner`, `fixture-viewer`, `bob`, `alice`, `carol` |
| `default` | `harmony-local-smtp` | `password` |

All local personas use `Password1234!`, matching the existing Terraform and
Compose developer contract. Override the shared value with
`LOCAL_AUTH_PERSONA_PASSWORD`. This is a well-known local-only credential; do
not use it outside a disposable local environment.

The SMTP password is a local-only generated credential. The declarative
`identity/smtp.yaml` resource uses it to configure Zitadel to deliver mail to
`mailpit.harmony-system.svc.cluster.local:1025`; Mailpit accepts any local
credentials and exposes its inbox at
`http://mailpit.harmony-system.svc.cluster.local:8025`.

`auth` is the Cluster Zitadel namespace. Provider bootstrap uses
`zitadel.auth.svc.cluster.local`; browser OIDC uses the AuthStack Gateway
issuer `https://auth.gitkb.localhost`.

After AuthStack is Ready, configure the Zitadel provider from its generated
admin PAT:

```bash
hops local zitadel --context kind-hops --source-context kind-hops \
  --source-namespace auth \
  --domain zitadel.auth.svc.cluster.local --port 8080 --insecure

```

The `hops local gitops cluster` controller keeps reconciling tracked manifest
changes after package CRDs and providers
become available. No second watcher or manual file touch is required.

After the Project reports an external ID, supply its two provider residuals:

```bash
make dev-gitops-identity
```

This explicit residual step exists because provider-upjet-zitadel `v0.1.1`
requires a generated `orgId` on Role managed resources but exposes no
Crossplane reference for it. The script therefore creates `approved` and
`admin` through the management API and supplies the observed Project org ID to
the raw smoke MachineUser, whose API has no `orgIdRef`. It accepts existing
roles as success and never writes the org ID or PAT to Git. AuthStack
`HumanUser` and `Grant` resources resolve their IDs declaratively. By default
the command port-forwards through `HOPS_LOCAL_CONTEXT`, avoiding same-named
services on other local clusters. Override `ZITADEL_INTERNAL_URL` and, when
needed, `ZITADEL_HOST_HEADER` for a custom endpoint.

The canonical users are `admin@gitkb.com`, `waitlisted@gitkb.com`,
`member@acme.com`, `device@gitkb.com`, `owner@acme.com`, `viewer@acme.com`,
`bob@acme.com`, `alice@acme.com`, and `carol@acme.com`. Their Grants reconcile
after the roles exist. The `harmony-local-smoke` MachineUser supplies client
credentials to the local smoke launcher. Read a generated local password only
when needed by selecting its persona key:

```bash
kubectl --context kind-harmony -n default get secret harmony-local-human-passwords \
  -o 'go-template={{ index .data "approved-admin" | base64decode }}{{ "\n" }}'
```

## Generated outputs

Cluster-owned `PushSecret` resources publish generated credentials to Vault:

| Source | Vault path |
|---|---|
| login-client PAT | `harmony/local/identity/login-client` |
| IAM admin PAT | `harmony/local/identity/iam-admin` |
| smoke MachineUser client | `harmony/local/identity/smoke-tests` |

The Environment's `.gitops/local/environment-secrets` chart materializes these
as `harmony-zitadel-login`, `harmony-zitadel-admin`, and
`harmony-zitadel-smoke`. The chart normalizes PAT whitespace when constructing
the environment-facing Secrets. The gateway chart similarly publishes its
generated OIDC client to
`harmony/local/environments/<namespace>/gateway-oidc` and consumes it through
an `ExternalSecret` named `harmony-gateway-oidc`.

## Phase 2: Environment-owned application Secret

After the shared Zitadel Project is Ready, publish its generated IDs and the
shared MinIO credentials into `harmony/local/application`, and register the
Environment gateway's residual trusted domain:

```bash
make dev-gitops-environment-secrets
```

The command does not create a Kubernetes application Secret. The Environment's
own `ExternalSecret/harmony-local` materializes `<environment>/harmony-local`
from the shared `harmony/local/application` Vault path. Every Environment owns
its own ExternalSecret and Secret resources while using the same local values.

The billing chart materializes `<environment>/harmony-stripe` from the
Cluster-shared Vault path `harmony/stripe`. The OIDC managed resource writes
its connection Secret, which the gateway's `PushSecret`/`ExternalSecret` pair
routes to a stable Environment-owned Secret. Generated PATs and OIDC client
secrets are not copied by this command.

The same command registers
`harmony-gateway.<environment>.svc.cluster.local` as a Zitadel trusted domain.
That remains imperative because provider-upjet-zitadel requires an
`instanceId` for `TrustedDomain`, while AuthStack does not expose that generated
ID. This is a provider boundary rather than hidden desired state.

If `harmony-stripe` is not Ready, sync Vault before retrying. Never put Stripe
values into an Application, Helm values file, or another namespace's Secret.
