# Nudge repository guidance

Nudge is a tray and menu-bar app that shows full-screen break reminders.
It uses Tauri for macOS, Windows and Linux. The backend is Rust under
`src-tauri/`; the frontend is plain JavaScript, HTML and CSS under `src/`.
Do not apply SwiftUI or Xcode project procedures to this app.

Read the relevant README section for the requested task. `package.json` defines
commands: `npm run dev` starts Tauri development and `npm run build` builds the
bundle. These commands launch or build the real app; neither is a doc check.

For Rust core checks, use `cargo test --locked --offline --manifest-path src-tauri/Cargo.toml`
when the existing dependency cache is available. Missing cached dependencies
block that check; ask before installing them. Core tests do not establish UI,
signing or another platform's behavior.

- Preserve existing configuration data and the installed app. Settings live in
  the user config directory; on macOS this is Application Support/nudge.
- `npm run install:app` installs and relaunches `/Applications/Nudge.app` and uses
  signing credentials. Do not run it for routine verification. Ask the maintainer
  before changing signing, permissions, the LaunchAgent, reminder behavior or
  releases.
- Scope tests to changed behavior and failure paths. For UI checks, use a debug
  build with `NUDGE_TEST_DATA_DIR` set to a temporary folder so real settings stay
  untouched (README, Building from source). It does not stop reminders from
  appearing full-screen, and it does not isolate permissions or the LaunchAgent.
  If you cannot run without interrupting the person at the machine, report UI
  verification as blocked.
- `ui-tests/README.md` records how the screenshots were captured. They do not
  prove the current build or another platform. Do not redirect `HOME` to
  reproduce them.
- Ask the maintainer before installing dependencies, altering lockfiles,
  rewriting architecture or changing stored-data formats.
- For documentation-only changes, proofread and validate references. Do not launch,
  build, install, sign or trigger a reminder solely to check prose.
