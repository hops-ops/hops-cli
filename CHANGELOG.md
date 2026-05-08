### What's changed in v0.22.0

* feat(provider): add hops provider install + --version-prefix flag (by @patrickleet)

  Spins out a new provider install command alongside the existing config
  install, sharing scaffolding via a new local/package_install.rs module.
  Mirrors the config-install flow: --path for a local source build, --repo
  for a remote source build, --repo + --version for a published-tag install.

  The --version-prefix flag prepends a SemVer-shaped prefix to the generated
  dev tag so locally-built provider images can satisfy a Configuration's
  '>=vN' dependency constraint. Example:

    hops provider install --path . --version-prefix v1
      → tag becomes v1-dev-<sha12> (instead of dev-<sha12>)

  Useful when a forked provider needs to substitute for an upstream-pinned
  dep on the same Configurations.


See full diff: [v0.21.0...v0.22.0](https://github.com/hops-ops/hops-cli/compare/v0.21.0...v0.22.0)
