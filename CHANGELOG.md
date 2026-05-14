### What's changed in v0.24.2

* refactor(auth): bootstrap writes plaintext, not AWS SM (by @patrickleet)

  `hops auth bootstrap <cluster>` now generates the durable AuthStack
  secret plaintexts into the repo's secrets/ tree, matching the AWS
  secret-path conventions the AuthStack composition's ExternalSecrets
  expect:

    secrets/aws/<cluster>/zitadel/masterkey/masterkey
    secrets/aws/<cluster>/zitadel/admin-password/password

  The platform's existing pipeline takes it from there:

    hops secrets encrypt    # SOPS-encrypts into secrets-encrypted/
    hops secrets sync aws   # pushes to AWS Secrets Manager

  This collapses two bootstrap pathways into one, makes the durable
  secrets reviewable in git (as SOPS-encrypted), and removes the
  direct AWS SDK dependency from this command.

  Idempotency unchanged: existing plaintexts are left alone unless
  `--force`.

  Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>


See full diff: [v0.24.1...v0.24.2](https://github.com/hops-ops/hops-cli/compare/v0.24.1...v0.24.2)
