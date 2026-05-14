### What's changed in v0.25.0

* feat(secrets): --secret-path scope on encrypt/decrypt (by @patrickleet)

  SOPS encryption is non-deterministic — re-encrypting unchanged
  plaintexts produces different ciphertext on every run, polluting the
  git diff with files the operator didn't actually touch. `hops secrets
  encrypt` and `decrypt` now accept an optional `--secret-path` that
  scopes the traversal to a single file or subdirectory inside the
  configured source root.

    # encrypt only the AuthStack durable secrets, no spurious diffs
    hops secrets encrypt --secret-path secrets/aws/pat-local/zitadel

    # symmetric — decrypt one subtree for inspection
    hops secrets decrypt --secret-path secrets-encrypted/aws/pat-local

  Validates the scope resolves inside the source root before running.

  Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>

* refactor(auth): bootstrap writes flat files (one shared AWS SM secret) (by @patrickleet)

  Both durable AuthStack values now live as JSON properties under a
  single AWS SM secret. Bootstrap writes flat plaintext files instead
  of single-file directories so `hops secrets sync aws` groups them
  into one AWS SM blob:

    secrets/aws/<cluster>/zitadel/masterkey
    secrets/aws/<cluster>/zitadel/admin-password

    → AWS SM `<cluster>/zitadel`:
        { "masterkey": "...", "admin-password": "..." }

  Drops the `masterkey/masterkey` and `admin-password/password` directory
  shapes — the property-name=directory-name redundancy was awkward and
  the values are seeded together / rotate together, so grouping them as
  one secret is the natural fit.

  Pairs with the AuthStack composition's collapsed `externalSecrets.secretPath`.

  Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>


See full diff: [v0.24.2...v0.25.0](https://github.com/hops-ops/hops-cli/compare/v0.24.2...v0.25.0)
