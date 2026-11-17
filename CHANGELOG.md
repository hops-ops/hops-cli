### What's changed in v0.56.3

* fix(provider): re-run a source install over a Provider hops created (#171) (by @patrickleet)

  fix(provider): re-run a source install over a Provider hops created

  hops provider install --path creates a new Provider that names the local
  registry directly (#170). A second run refused it: the existing package
  was in the local registry with no ImageConfig rewrite, which used to
  mean a lost upstream URL. A Provider whose package is exactly the
  source-build path <registry>/hops-ops/<name> is now re-pointed at the
  new local tag, keeping its runtime config.


See full diff: [v0.56.2...v0.56.3](https://github.com/hops-ops/hops-cli/compare/v0.56.2...v0.56.3)
