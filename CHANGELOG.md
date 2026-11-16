# Changelog

## v1.1.0 — 2026-11-16

### Fixed
- **Hotkeys `F6` (start/stop) and `F9` (emergency stop) could be dead on some machines.**
  v1.0 relied solely on `RegisterHotKey`, which fails when another application already
  owns the combination — and the warning sat inside a collapsed section, so the failure
  looked silent. v1.1 adds a 10 ms `GetAsyncKeyState` fallback poll: the bindings keep
  working even when registration fails (or never delivers), the two delivery paths are
  deduplicated so one physical press fires exactly one action, and a failed registration
  is retried every 5 seconds.
- Key capture no longer races the hotkey thread's message queue: `PostThreadMessageW`
  could silently fail right after startup, leaving the old hotkeys registered while a
  new key was being bound.
- The tray menu now dismisses when you click elsewhere: the tray window is a real hidden
  window instead of a message-only window, which cannot become foreground (required by
  `TrackPopupMenu`).
- The hotkey message loop was rewritten around `MsgWaitForMultipleObjectsEx` +
  `PeekMessageW`; a `GetMessageW` error (−1) could previously spin the loop.
- A corrupt `config.json` with out-of-range hotkey VK codes now falls back to `F6`/`F9`
  instead of leaving the bindings dead.
- `focus_existing_instance` no longer touches a same-titled window of another process.
- Repository layout repaired: `main.rs` → `src/main.rs`, `icon.ico`/`icon.rgba` →
  `assets/`. The uploaded 1.0 tree did not compile as-is (`include_bytes!` and `build.rs`
  expect this layout); the Explorer/taskbar icon is embedded again.

### Changed
- Rust 1.97.1 → **1.100.0** (current stable, pinned via `rust-toolchain.toml`).
- egui / eframe 0.36 → **0.37**
- parking_lot 0.12.5 → **0.13.0**
- serde_json 1.0.151 → **1.0.154**
- known-folders 1.3 → **1.4.2**
- windows 0.62 → **0.62.2**, winresource → **0.1.34**, tracing → **0.1.44**
- Removed the unused direct `windows-core` dependency.

### Performance
- Settings are pushed into the click engine only when they actually change
  (was: ~30 atomic stores + a mutex lock + a `Vec` clone on every UI frame).
- The point list is cached inside the click loop and re-read only when edited
  (was: lock + clone on every single click).
- Idle UI repaints dropped from 10 Hz to 2 Hz; still 10 Hz while running, capturing a
  key or picking a point. Input events repaint immediately, so nothing feels slower.
- CJK fonts and emoji now come from eframe 0.37's system font provider instead of
  reading a ~20 MB font file into memory at startup; emoji render in color.

### Added
- Clipboard support in text fields (eframe `clipboard` feature).
- FAQ: executables launched from inside an agent-sandbox workspace carry a Low
  mandatory integrity label, which makes Windows restrict global hotkeys/background
  input to focus-only. Copy the exe to a normal folder or use the release download.
  (Diagnosed end-to-end: the same binary works globally once copied out of the
  labeled workspace; verified with integrity-level logging and live key tests.)

### Added
- Unit tests for the hotkey dedup state machine and hotkey sanitization.
- `Cargo.lock` committed for reproducible builds; `.gitignore` for build artifacts.

## v1.0.0 — 2026-08-14
- Initial release.
