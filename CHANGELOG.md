### What's changed in v0.19.0

* feat: context | install --watch (by @patrickleet)

* fix(watch): unbreak tests, lower debounce default, stop losing edits (by @patrickleet)

  - Update ConfigArgs fixtures in validate_reload_args tests to include
    the watch/context/debounce fields added in b916b6d (unbreaks CI quality).
  - Lower --debounce default from 30s to 15s; agent-driven edits felt
    sluggish at 30s.
  - Remove post-rebuild drain_pending + wait_for_quiet + drain_pending
    block in run_watch. The drain silently discarded edits made during
    a rebuild, and recv_timeout inside the second wait_for_quiet
    consumed events in the 15s post-rebuild window without requeueing
    them. The main loop's rx.recv() + wait_for_quiet(debounce) already
    debounces correctly on its own; should_ignore_path filters the
    build's own writes to _output/.
  - Log each notify event at debug level so LOG_LEVEL=debug reveals
    which paths the rebuild itself actually touches.

  Implements [[tasks/cli-config-install-watch-context]]


See full diff: [v0.18.1...v0.19.0](https://github.com/hops-ops/hops-cli/compare/v0.18.1...v0.19.0)
