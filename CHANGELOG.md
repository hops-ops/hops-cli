### What's changed in v0.19.1

* refactor(install): drop --reload flag; rely on unique dev tags (by @patrickleet)

  Each source build is already tagged with a unique dev-<sha256> derived from
  the .uppkg content, so the Configuration's spec.package changes on every
  rebuild and Crossplane creates a fresh ConfigurationRevision on its own.
  Force-deleting revisions before re-apply (what --reload did) is now dead
  weight.

  Removes:
  - ConfigArgs.reload and reload: bool parameters threaded through
    run_local_path, run_repo_install, run_repo_clone,
    resolve_repo_install_target, run_watch, apply_configuration
  - force_reload_configuration_revisions + list_configuration_revisions_for +
    revision_belongs_to_configuration helpers and their unused struct siblings
  - validate_reload_args and 4 obsolete unit tests

  Skill doc (skills/claude/references/config-install.md) updated to document
  the re-run-to-upgrade loop, add --watch/--debounce/--context to the flags
  table, and drop the --reload row.

  Implements [[tasks/hops-remove-install-reload-flag]]

  Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>


See full diff: [v0.19.0...v0.19.1](https://github.com/hops-ops/hops-cli/compare/v0.19.0...v0.19.1)
