### What's changed in v0.57.0

* chore(deps): update rust crate clap to v4.6.8 (#172) (by @renovate[bot])

  Co-authored-by: renovate[bot] <29139614+renovate[bot]@users.noreply.github.com>

* chore(deps): update rust crate uuid to v1.29.3 (#173) (by @renovate[bot])

  Co-authored-by: renovate[bot] <29139614+renovate[bot]@users.noreply.github.com>

* chore(deps): update hops-ops/workflows-gitops action to v2.1.3 (#175) (by @renovate[bot])

  Co-authored-by: renovate[bot] <29139614+renovate[bot]@users.noreply.github.com>

* chore(deps): update rust crate clap to v4.6.9 (#176) (by @renovate[bot])

  Co-authored-by: renovate[bot] <29139614+renovate[bot]@users.noreply.github.com>

* feat(secrets): add --cleanup to sync github (#177) (by @patrickleet)

  Sync only writes, so a secret removed from the local tree stayed in every
  repository it had been synced to. --cleanup lists each target repository's
  Actions secrets after syncing and offers to delete the ones that are not in
  the local tree.

  GitHub secrets carry no ownership marker, so every deletion is confirmed
  individually with a default of no. --yes does not skip these prompts.

  [[tasks/cli-secrets-github-cleanup]]


See full diff: [v0.56.3...v0.57.0](https://github.com/hops-ops/hops-cli/compare/v0.56.3...v0.57.0)
