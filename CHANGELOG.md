### What's changed in v0.26.0

* feat(secrets): three-bucket list with platform-pushed section, symmetric managed-by tagging (by @patrickleet)

  `hops secrets sync aws` now writes `hops.ops.com.ai/managed-by=sync`
  alongside the existing `hops.ops.com.ai/secret=true`, mirroring the
  `managed-by=pushsecret` tag that AuthStack-style Crossplane stacks
  attach to ESO-pushed AWS SM entries. Bucketing in `secrets list`
  becomes a positive match instead of "secret=true minus everything
  else."

  `secrets list` gains a "Platform-pushed secrets" section between the
  existing "Managed secrets" and "Other AWS secrets" sections. Routing
  by `hops.ops.com.ai/managed-by`:
    - sync (or absent + secret=true for legacy entries) → Managed
    - pushsecret                                        → Platform-pushed
    - everything else                                   → Other

  Platform-pushed columns: Name | Owner | Cluster | Namespace | KMS Key |
  Status. `Owner` is derived from the explicit `hops.ops.com.ai/kind` +
  `hops.ops.com.ai/name` tags (renders e.g. `AuthStack/pat-local`); falls
  back to scanning for the older `hops.ops.com.ai/<kind.lower>=<name>`
  label for entries pushed before the explicit tags were added.

  `has_managed_secret_tag()` (used by `sync --cleanup` to decide which
  remote secrets to consider for deletion) was tightened to require
  either `managed-by=sync` OR no managed-by tag at all. Previously it
  matched any `secret=true` entry, which would have swept up PushSecret-
  managed entries the moment we added `secret=true` to that flow.

  Existing managed secrets show `remote tags differ` in the list output
  until the next `hops secrets sync aws` writes the new managed-by tag.
  No data is at risk — the migration is just additive.

  Pairs with the AuthStack PushSecret commit + the SecretStack
  `push/*` IAM grant.


See full diff: [v0.25.1...v0.26.0](https://github.com/hops-ops/hops-cli/compare/v0.25.1...v0.26.0)
