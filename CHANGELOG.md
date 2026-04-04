### What's changed in v0.16.0

* feat: robust local/published config switching in config install (by @patrickleet)

  - Always delete stale render Function packages before pushing new images,
    not just with --reload. Fixes ImagePullBackOff when local registry has
    a different digest than the previously installed Function.

  - Fix config install --path naming: use org-repo (e.g. hops-ops-aws-secret-stack)
    instead of just repo name, matching published Configuration names.

  - Fix Docker build cache corruption: replace multi-stage Dockerfile patching
    (FROM source AS src / COPY --from=src) with docker create + export approach.
    The old method broke when Docker's snapshot cache was stale for images loaded
    via docker load.

  - When installing a published --version, clean up local install artifacts:
    delete stale render Functions, local ImageConfig rewrites, and inactive
    ConfigurationRevisions pointing at the local registry.

  Co-Authored-By: Claude Opus 4.6 (1M context) <noreply@anthropic.com>


See full diff: [v0.15.0...v0.16.0](https://github.com/hops-ops/hops-cli/compare/v0.15.0...v0.16.0)
