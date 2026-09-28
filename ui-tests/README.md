# UI snapshots (point-in-time reference)

## 2026-09-27, multiple nudges

Captured from a debug build of the working tree (after `b9117d1`, not yet
committed) on macOS 27, light appearance, Retina (2x), with
`screencapture -l <window id>`. The build ran with `HOME` pointed at a temp
folder and three nudges in its config.json, so the installed app and its
settings were left alone. Pages were switched with AX press actions.

- `settings-nudges-2026-09-27.png`: the Schedule page with the nudge picker
  in the pane bar and the "This nudge is on" switch (shown with 2+ nudges).
- `nudge-picker-2026-09-27.png`: the picker open: each nudge, then New
  Nudge… and Delete.
- `settings-symbol-2026-09-27.png`: the Reminder page for a nudge using the
  eye symbol, the Forest style and the Ring layout, with the Symbol row.
- `tray-nudges-2026-09-27.png`: the menu bar menu with one countdown line per
  nudge, soonest first.

The overlay's "Also now" line was not captured from the app, because showing
the reminder takes over every screen.

## 2026-09-27

Captured from the signed release build on macOS 27, Retina (2x), with
`screencapture -l <window id>`.

- `settings-2026-09-27.png`: the redesigned Settings window (780x580
  logical), a System Settings–style sidebar over the native window material,
  showing the Reminder page with its live preview, style swatches and the
  Classic / Card / Ring layout picker (0.2.0, `b9117d1`, light appearance,
  Card selected).
- `about-2026-09-27.png`: the About panel opened from the menu bar menu
  (working tree after `59099a6`, dark appearance).

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
