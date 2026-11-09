### What's changed in v0.55.0

* feat: bind secrets operations to a repo AWS identity (#165) (by @patrickleet)

  * feat: bind secrets operations to a repo AWS identity

  Use optional profile/account settings for SOPS and AWS adapters. Preserve legacy credential behavior and reject account or SOPS profile conflicts before secret operations.

  [[tasks/hops-cli-aws-profile-account]]

  * fix: validate SOPS profiles in every ciphertext key group

  Inspect only schema-defined profile fields so encryption context remains data. Cover conflicting groups, inherited profiles, and untouched ciphertext with command regressions.

  [[tasks/hops-cli-aws-profile-account]]


See full diff: [v0.54.1...v0.55.0](https://github.com/hops-ops/hops-cli/compare/v0.54.1...v0.55.0)
