### What's changed in v0.31.0

* feat: add distributed service scaffold commands (#55) (by @patrickleet)

  * feat: add distributed service scaffold commands

  Implements [[tasks/hops-service-create-microsvc-scaffold]].

  Updates [[customize-hops-service-scaffold-and-schema-output]].

  Updates [[gitops-knative-service-scaffold]].

  Updates [[replace-model-booleans-with-repeatable-model]].

  Updates [[add-service-bus-flag]].

  Updates [[make-service-read-models-opt-in]].

  Updates [[rename-service-create-to-scaffold]].

  Updates [[add-service-scaffold-github-workflows]].

  * refactor: back service scaffold with distributed_tooling crate

  Replace the ~2100-line in-CLI generation logic (ScaffoldNames/ModelScaffold/
  MessageHandler, all the *_rs / *_yaml templates, Knative broker/trigger
  inference, GitHub workflow rendering) with a thin adapter over the new
  distributed_tooling crate. The CLI now:

  - keeps the clap surface (ScaffoldArgs + Framework/Transport/Store/Bus/
    GitopsPromote enums) and maps it to distributed_tooling::ServiceScaffoldSpec
    via From impls;
  - computes output_dir + the relative distributed dependency path as before;
  - calls generate_service_scaffold(), writes each GeneratedFile (creating parents,
    honoring FileMode::Executable), prints warnings, and runs the
    EnsureGithubRepository post-create action via the existing gh logic;
  - keeps describe/schema and the manifest compile-harness unchanged.

  Generation rules now live in (and are tested by) distributed_tooling. Verified
  byte-for-byte identical output against the previous implementation across five
  variants (HTTP, model+read-models, Knative+bus+gitops+promote, full GitHub, and
  preview-only) — the only intended difference is the generated service.rs builder
  (Service::new().with_repo(repo)).

  Dependency uses the meta-repo sibling path (../distributed/distributed_tooling);
  a git dependency will replace it for released/standalone builds.

  cli mod.rs: 2589 -> 930 lines.

  Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>

  * fix(ci): depend on distributed_tooling via git instead of a local path

  The `../distributed/distributed_tooling` path dep only resolves in the meta-repo
  sibling layout; standalone hops-cli CI checks out only this repo, so the build
  failed reading the missing Cargo.toml. distributed is public, so a plain HTTPS
  git dep resolves with no secrets. Tracks the PR #53 branch until the crate is
  published to crates.io.

  Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>

  * chore: depend on published distributed_tooling 1.5 from crates.io

  distributed v1.5.0 published distributed_tooling, so drop the temporary
  git-branch dependency in favor of the registry version. No git source or
  secrets needed; the crate only pulls in serde_json.

  Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>

  ---------

  Co-authored-by: Claude Opus 4.8 (1M context) <noreply@anthropic.com>


See full diff: [v0.30.0...v0.31.0](https://github.com/hops-ops/hops-cli/compare/v0.30.0...v0.31.0)
