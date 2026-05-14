### What's changed in v0.24.1

* fix(auth): SSO-aware AWS creds + Zitadel-compliant password generator

  Two follow-ups from end-to-end testing of `hops auth bootstrap`:

  - Switch from a raw rusoto ChainProvider to reusing `secrets::aws_clients`
    so bootstrap resolves SSO profiles the same way `hops secrets` does
    (AWS_PROFILE → aws configure export-credentials → StaticProvider).
    Makes `bootstrap` work cleanly under `AWS_PROFILE=hops` against the
    SSO-only hops account.
  - Add a `generate_complex_password` that satisfies Zitadel's default
    policy (HasUppercase + HasLowercase + HasNumber + HasSymbol). The
    prior generator emitted hex (digits + lowercase only); Zitadel's
    Human user creation rejected those.
  - Pass a UUID client_request_token to both CreateSecret and
    PutSecretValue (AWS SM requires it on PutSecretValue too).
  - Unit tests cover all four character classes across 200 rolls.

  Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>


See full diff: [v0.24.0...v0.24.1](https://github.com/hops-ops/hops-cli/compare/v0.24.0...v0.24.1)
