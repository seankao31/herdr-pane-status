# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.1.0] - 2026-10-20

First release.

### Added

- Pane-border markers with herdr's own state icons: `×` blocked, `✓` done
  until the pane is focused, `◐` working (`show_working`). The pane's label
  stays in the title.
- A per-pane done flag, kept across daemon restarts in `finished.json`.
- One binary for the hook and the daemon. The hook runs on startup,
  `pane.created` and `pane.focused`, so a daemon that died is back on the
  next focus change.
- `herdr-pane-status stop` for a clean shutdown on every platform.
- Titles change only when what they say changes, since every new title
  fires `pane.agent_status_changed` for every plugin hook on it; an
  unchanged title is re-sent every 2 seconds to keep it from expiring.
