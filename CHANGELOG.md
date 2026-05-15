### What's changed in v0.25.1

* fix(provider install): never reuse a Provider's existing DRC ref (by @patrickleet)

  apply_provider_resources now always names the DRC <provider>-runtime
  instead of inheriting the existing Provider's spec.runtimeConfigRef.name.
  A previous "avoid orphaning the DRC" heuristic blindly reused whatever
  DRC the upstream Provider already referenced, then overwrote that DRC
  with the new install's image — silently corrupting any other Provider
  that happened to reference the same DRC.

  Repro (the bug that surfaced this): provider-helm and provider-kubernetes
  both referenced DRC "local-dev"; a later 'hops provider install helm'
  wrote local-dev with the helm dev image, leaving provider-kubernetes
  pinned to that DRC and therefore running the helm binary in a pod
  labeled as kubernetes.

  After the fix:
  - Each install creates an owned DRC named <existing_provider>-runtime
  - If the existing Provider already pointed at a differently-named DRC,
    a log::warn surfaces the migration ("switching to owned DRC ...; the
    old DRC is not deleted") so leftovers are discoverable
  - The orphaned DRC is left in place; it may still be referenced by
    another Provider, and deletion is the operator's call

  Validated end-to-end on colima: 'hops provider install --repo
  jonasz-lasut/provider-helm' migrated provider-helm from local-dev to
  crossplane-contrib-provider-helm-runtime, helm pod healthy on
  v1.999.2, provider-kubernetes (after clearing its stale local-dev ref)
  healthy on upstream provider-kubernetes:v1.2.0.

  Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>


See full diff: [v0.25.0...v0.25.1](https://github.com/hops-ops/hops-cli/compare/v0.25.0...v0.25.1)
