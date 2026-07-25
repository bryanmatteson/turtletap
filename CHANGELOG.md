# Changelog

## 0.2.1 — 2026-07-25

- Updated the optional Termosaic integration to 0.2.2 and its Laidout 0.3
  layout backend.

## 0.2.0 — 2026-07-24

- Added the installable `turtletap` command and durable resident sessions.
- Added crash recovery, request deduplication, bounded framing and output spooling,
  isolated worker transport, and process-group termination.
- Added dashboard, session, and keybinding-editor action-bar commands.
- Added configurable context-specific bindings with KDL and TOML support.
- Added active command recovery without redispatch after leader replacement.
- Added human and JSON command output, shell completions, manual generation, and
  raw `config path` output for shell composition.
- Added Termosaic 0.2.1 semantic documents and retained Ratatui rendering at the
  surface boundary.

## 0.1.0 — 2026-07-22

- Added the reusable terminal shell, surface lifecycle, action bar, and resident
  library foundations.
