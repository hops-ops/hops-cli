### What's changed in v0.56.1

* chore(deps): update rust crate ureq to v3.5.2 (#168) (by @renovate[bot])

  Patch: body limits applied after decoding, NetworkDown address fallback. hops-cli uses ureq without gzip/brotli features; vault read_json responses are small and registry uses unbounded as_reader, so unaffected. quality green.

* fix(deps): update hops-ops/workflows-gitops action to v2.1.1 (#169) (by @renovate[bot])

  Updates import templates (shipped in the CLI binary) to workflows-gitops v2.1.1 SHA 15299bf (verified tag). v2.1.1 adds optional merge_existing_values input defaulting to true (existing merge behavior preserved); templates pass no new inputs. fix so generated workflows ship in a release. quality green.


See full diff: [v0.56.0...v0.56.1](https://github.com/hops-ops/hops-cli/compare/v0.56.0...v0.56.1)
