### What's changed in v0.14.0

* feat: secrets (#35) (by @patrickleet)

  * **New Features**
    * Added a comprehensive secrets management CLI with subcommands: init, encrypt, decrypt, list, and sync
    * Local SOPS-based encrypt/decrypt and directory mirroring with overwrite control
    * Interactive init with optional KMS creation/registration and example secrets generation
    * Syncing to AWS Secrets Manager (create/update/delete, tagging) and GitHub repo secrets (bulk apply)
    * Secrets inventory that merges local and remote state and reports status and KMS info


See full diff: [v0.13.0...v0.14.0](https://github.com/hops-ops/hops-cli/compare/v0.13.0...v0.14.0)
