# UI snapshots (point-in-time reference)

## 2026-09-27

Captured from the signed release build (working tree after `59099a6`) on
macOS 27, dark appearance, Retina (2x), with `screencapture -l <window id>`.

- `settings-2026-09-27.png`: the redesigned Settings window (780x580
  logical), a System Settings–style sidebar over the native window material,
  showing the Reminder page with its live preview and style picker.
- `about-2026-09-27.png`: the About panel opened from the menu bar menu.

The overlay was not recaptured: showing it takes over every screen, and the
button styling change is small. `overlay-current.png` still shows the layout.

## 2026-09-12

Date captured: 2026-09-12, bundled release build from commit working-tree
at that time (auto-dismiss + interval floor + settings-persistence batch,
`bdd12dd` and later). Commit new screenshots next to these whenever the
UI intentionally changes, named `<surface>-<YYYY-MM-DD>.png`.

- `overlay-current.png` — the fullscreen reminder over a real desktop
  (Retina capture, 2x), message + snooze pill + dismiss hint + counters.
- `settings-current.png` — replica of the Settings window at its true
  440x500 logical content size (same HTML/CSS/JS as `index.html`; the
  real window differs only by native window chrome (title bar), which
  adds ~28px above this region).

How the settings replica was produced: NSStatusItem menus are not
exposed to the accessibility bridge on modern macOS, so the tray menu
could not be clicked programmatically. `index.html`/`styles.css`/
`main.js` were served directly with a `window.__TAURI__` stub returning
the current state shape — same files the app loads, same CSS, same JS
render path.
