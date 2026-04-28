### What's changed in v0.20.0

* feat(config-install): handle multi-function package builds (#42) (by @patrickleet)

  `up project build` for a Configuration repo with multiple `functions/<name>/`
  subdirectories produces multiple Function package images, named
  `<repo>_<funcname>` per function (not `<repo>_render`). The local-install
  path filtered on `_render` suffix and only created an ImageConfig rewrite
  + digest patch for the single render function, which broke multi-function
  repos:

  - Function images for `cluster`, `branch`, etc. were tagged and pushed
    to the local registry but no rewrite was created, so Crossplane tried
    to pull them from ghcr.io by their (unpublished) digest and failed.
  - The Configuration's package metadata wasn't patched with local digests
    for those functions, so dependency resolution failed even when the
    images were technically reachable.

  The configuration-vs-function distinction is already established by the
  `is_configuration_image` filter above the loop. All remaining images are
  Function packages; treat them uniformly.

  Tested against psql-stack (apis/{psqlstacks,psqlclusters,psqlbranches})
  on colima — all three Functions install Ready and all three XRDs become
  available.


See full diff: [v0.19.2...v0.20.0](https://github.com/hops-ops/hops-cli/compare/v0.19.2...v0.20.0)
