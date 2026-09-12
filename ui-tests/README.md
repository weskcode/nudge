# UI snapshots (point-in-time reference)

Date captured: 2026-09-12, bundled release build from commit working-tree
at that time (auto-dismiss + interval floor + settings-persistence batch,
`bdd12dd` and later). Commit new screenshots next to these whenever the
UI intentionally changes, named `<surface>-<YYYY-MM-DD>.png`.

- `overlay-current.png` — the fullscreen reminder over a real desktop
  (Retina capture, 2x), message + snooze pill + dismiss hint + counters.
- `settings-current.png` — pixel replica of the Settings webview at the
  live window's layout (real window is 440x500 logical; captured from the
  exact same HTML/CSS at its content width, webview-only differences:
  no native window chrome / traffic lights).

How the settings replica was produced: NSStatusItem menus are not
exposed to the accessibility bridge on modern macOS, so the tray menu
could not be clicked programmatically. `index.html`/`styles.css`/
`main.js` were served directly with a `window.__TAURI__` stub returning
the current state shape — same files the app loads, same CSS, same JS
render path.
