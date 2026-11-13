### What's changed in v0.56.0

* feat(local): generate Zitadel inputs with auth-stack v1.13.0 (#167) (by @patrickleet)

  feat(local): generate Zitadel inputs with auth-stack v1.13.0

  The local template moves to auth-stack v1.13.0 and turns on
  externalSecrets.generate (Vault, local/zitadel), so a fresh cluster no
  longer needs a hand-made zitadel-masterkey.

  Existing clusters have that hand-made Secret and no store entry, and
  generation would replace the key Zitadel's data is encrypted with.
  Before cluster manifests apply, hops now copies a non-ESO masterkey
  Secret (and admin password, if any) to the AuthStack's secretPath with
  a PushSecret (IfNotExists), waits for it to sync, then removes the
  in-cluster copy. ESO then adopts the existing Secret with the same
  value.

  [[tasks/authstack-generated-inputs]]

  docs(local): warn that Vault PVC holds the Zitadel masterkey


See full diff: [v0.55.1...v0.56.0](https://github.com/hops-ops/hops-cli/compare/v0.55.1...v0.56.0)
