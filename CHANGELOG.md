### What's changed in v0.57.1

* fix(deps): adopt sha2 0.11 (#174) (by @patrickleet)

  * fix(deps): update rust crate sha2 to 0.11.0

  * fix(deps): adopt sha2 0.11

  sha2 0.11 (digest 0.11) finalize() returns a hybrid-array Array that no
  longer implements LowerHex, and Sha256 no longer implements io::Write.

  - hex-encode digests explicitly ({:02x} per byte); output is identical
    lowercase hex, so dev tags, OCI digests and content revisions are unchanged
  - use Digest::update instead of io::Write::write_all in sha256_reader

  ---------

  Co-authored-by: renovate[bot] <29139614+renovate[bot]@users.noreply.github.com>


See full diff: [v0.57.0...v0.57.1](https://github.com/hops-ops/hops-cli/compare/v0.57.0...v0.57.1)
