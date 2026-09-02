### What's changed in v0.46.0

* feat: add repository import command (#103) (by @patrickleet)

  * feat: add repository import command

  * feat: support Knative services in import

  * fix: leave local runtime out of import

  * feat: add import dry-run preview

  * feat: support preview-only imports

  * fix: grant preview promotion write permission

  * fix: promote previews through environment PRs

  * fix: promote previews directly to environments

  * fix: merge protected environment promotions

  * feat: bundle Hops import skill

  Implements [[tasks/hops-2]]

  * fix: harden import preview workflows

  Implements [[tasks/hops-import-command]].

  * fix: isolate irrelevant preview labels

  Implements [[tasks/hops-import-command]].


See full diff: [v0.45.0...v0.46.0](https://github.com/hops-ops/hops-cli/compare/v0.45.0...v0.46.0)
