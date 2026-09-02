---
name: hops-import
description: |
  Adopt an existing GitHub application repository into Hops GitOps delivery
  with `hops import`. Use when assessing or running an import; choosing
  Dockerfile versus Railpack, Deployment versus Knative, or preview-only versus
  release delivery; reconciling existing release automation; configuring
  environment repositories and GitHub App credentials; or verifying preview,
  cleanup, image publication, and staging promotion.
---

# Hops Import

Use `hops import` as a deterministic generator, not as an architecture detector.
The command can render the delivery contract safely; the repository-specific
build, runtime, release, registry, and authentication choices remain yours.

## Invariants

- Work from an existing Git repository with a GitHub `origin`, or pass
  `--repository OWNER/REPO` explicitly.
- The importer owns only `.gitops/{deploy,promote}` and its named workflows. It
  does not modify application source or create `.gitops/local`.
- Start with `--dry-run`. It performs no writes, needs neither `gh` nor `vnext`,
  and classifies generated paths as `CREATE`, `UPDATE`, or `UNCHANGED`.
- A normal run refuses any existing importer-owned path. `--force` replaces all
  such paths, so use it only after reviewing the complete dry-run.

## Preflight decisions

Inspect the repository before choosing flags:

```bash
git status --short --branch
git remote -v
git symbolic-ref --short refs/remotes/origin/HEAD 2>/dev/null || git branch --show-current
rg --files -g 'Dockerfile*' -g 'package.json' -g 'Cargo.toml'
rg --files .github/workflows .gitops 2>/dev/null
```

Resolve these choices explicitly:

| Choice | Default | When to override |
|---|---|---|
| Build | Root `Dockerfile`, else Railpack | Pass `--dockerfile` for a non-root file. Prefer a tested custom image for static bundles or unusual monorepos. |
| Workload | Deployment + Service | Use `--knative-service` for scale-to-zero HTTP services. This does not add OIDC or other sidecars. |
| Delivery | Full release lifecycle | Use `--preview-only` for a bounded pilot or when existing release automation must remain authoritative. |
| Branch | `origin/HEAD`, then current branch | Pass `--branch` when either is missing or not the release branch. |
| Environments | Owner-derived repositories | Usually pass `--preview-repository` and `--staging-repository`; organization naming rarely matches both defaults. |
| Port | `3000` | Set the port actually listened on inside the image. |
| Argo project | `default` | Pass `--project` when the destination environment restricts applications. |

Full mode generates vNext main-branch tagging. If the repository already uses
Release Please or another tag producer, do not install a competing version
workflow. A `v*.*.*` tag can still trigger the generated release/promotion
workflow after you deliberately reconcile the workflow set.

## Render before writing

Prefer a command with every repository-specific choice visible:

```bash
hops import . \
  --repository OWNER/APP \
  --name APP \
  --port 3000 \
  --preview-repository OWNER/PREVIEW-ENVS \
  --staging-repository OWNER/STAGING-ENV \
  --project default \
  --dry-run
```

Add `--knative-service`, `--preview-only`, `--dockerfile PATH`, or `--branch`
only from the preflight decisions. Review the rendered image repository,
workload port, source repository, environment repositories, Argo project, and
all workflow triggers before removing `--dry-run`.

## Authentication and registry checks

- Preview and staging promotion require repository Actions secrets
  `GH_APP_ID` and `GH_APP_KEY`.
- The GitHub App must be installed on each target environment repository with
  the permissions required to commit or create and merge promotion PRs.
- The generated publisher targets GHCR and the deploy chart references that
  image. For a private image, confirm the cluster has compatible pull
  credentials before promotion. If the platform standard is ECR or dual
  publication, reconcile the generated publisher and image repository before
  committing; the importer does not infer registry policy.
- OIDC exposure is a deploy-chart concern. Confirm the environment's chosen
  AuthStack/OIDC integration and proxy pattern separately; `--knative-service`
  alone does not protect the service.

Full mode also configures vNext's `DEPLOY_KEY` through authenticated `gh` and
`vnext`. Use `--skip-deploy-key` when intentionally deferring that step.
Preview-only mode omits vNext setup automatically.

## Validate the generated repository

After writing, inspect only the paths the importer should own:

```bash
git status --short
git diff -- .gitops .github/workflows
helm lint .gitops/deploy
helm template verify .gitops/deploy >/dev/null
helm lint .gitops/promote
helm template verify .gitops/promote >/dev/null
actionlint .github/workflows/*.yaml
```

Then verify the lifecycle instead of stopping at generated YAML:

1. Label a same-repository pull request `preview`; confirm its image is
   published before promotion, the environment commit lands, and the Argo
   application and workload become healthy.
2. Remove the label or close the PR; confirm the cleanup job removes that
   preview without disturbing other previews in the shared repository.
3. In full mode, create a semver `v*.*.*` tag through the repository's chosen
   release authority; confirm image publication completes before staging is
   promoted.
4. For Knative, verify the Service is Ready and its URL matches the cluster
   domain template. For OIDC-protected apps, an unauthenticated request should
   redirect to the identity provider and return to the service callback URL.

## Common failure meanings

- Existing-path refusal: generated delivery files already exist; inspect and
  reconcile them before considering `--force`.
- No Dockerfile selected: Railpack was chosen; verify detection locally or add
  the intended Dockerfile explicitly.
- Preview does not promote: check the `preview` label, same-repository guard,
  image job, GitHub App secrets/installations, and target-repo protection.
- Workload cannot pull: the image exists but registry authentication or the
  repository path does not match the deploy values.
- Staging never changes: confirm a semver tag was pushed and the image job
  succeeded; promotion intentionally waits for publication.
