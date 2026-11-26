# Privacy

Nudge does not collect your data. Its own code opens no network connections. It has no account, no
analytics, no advertising and no tracking, and nothing in it phones home.

The [in-app privacy policy](src/privacy.html) is included with Nudge.
This page says what Nudge keeps on your computer, the few times something leaves
it, and how to check each claim yourself. [SECURITY.md](SECURITY.md) covers how to
report a security problem.

## What Nudge keeps on your computer

- **Your settings**, in `config.json`: your nudges (their messages, intervals,
  symbols and looks), and the choices in Settings. Your reminder text is
  stored there. If Nudge cannot read or parse the file, it tries to move it aside
  as `config.json.bad-*` in the same folder. Each recovery copy can contain
  reminder text and stays there until you remove it. If the move fails, Nudge
  uses defaults and skips settings saves for that session to preserve the original.
- **A local diagnostics record**, in a `diagnostics` folder beside it. It holds
  counts (panics, stalls, unclean exits), fixed step names and coarse durations.
  It holds no reminder text, no paths, no exact times and no device identifiers.
  Turn it off in Settings > General.
- **The chime sound**, written to your user cache folder so the system player can
  read it.

On macOS these live in `~/Library/Application Support/nudge/` and
`~/Library/Caches/nudge/`. The Mac App Store build runs in the App Sandbox, so it
keeps them inside its container, `~/Library/Containers/in.nudge.app/Data/Library/`,
and starts with its own settings. On macOS and Linux, Nudge requests owner-only
permissions for new settings and diagnostics files (600) and their folders (700). The chime cache
uses the system defaults. On Windows and Linux the folders are in your user
config and cache directories.

To remove reminders, diagnostics, and the cached chime, quit Nudge and remove
its nudge config and cache folders.
For the Mac App Store build, use the folders inside its container. Removing
the config folder also clears your diagnostics preference. Local recording
starts again the next time you open Nudge.

Settings keeps the last page and selected reminder ID in local web storage.
These choices can remain after you remove the folders. If you enabled
**Open at login**, turn it off in General before quitting to remove the login
registration.

## What leaves your computer

Nudge does not upload your settings or diagnostics. Nudge opens a web page in your browser only when you ask:

- **The credit link** in About Nudge opens the page of remindful, the app Nudge
  is modeled on.
- **Open GitHub issue** in Settings > General opens GitHub in your browser.
  Opening it shares no diagnostics. To include the report, use Copy report and
  paste the text into the form yourself. Submitted issues are public.

## Your keyboard

Closing a reminder with any key, while another app is in front, needs the macOS
Input Monitoring permission. It is optional. Without it, a click, the Done
button, or a key typed while the reminder has focus all close it.

With the permission, Nudge listens for key presses only while a reminder is on
screen. It checks the key code and modifier flags to recognize shortcuts.
Nudge does not store or send keystrokes. You can revoke Input Monitoring
in System Settings under Privacy & Security.

In the Mac App Store build, closing a reminder from another app through Input
Monitoring has not been verified.

## How to check

The Nudge app holds the network-client entitlement in the Mac App Store build.
It allows outgoing connections and is enabled so WebKit can display the windows. Nudge loads its Settings and reminders from bundled files.

- Run `sh scripts/check-no-network.sh path/to/Nudge.app` from the repository root.
  It checks the current checkout’s dependency graphs for listed networking crates.
  It inspects the supplied app’s executable for listed imports and linked libraries.
  The script does not establish which checkout produced that executable.
- While Nudge runs, `lsof -i -a -p "$(pgrep -n -x nudge)"` shows connections
  for the most recently started nudge process at that moment. WebKit helpers and your
  browser run as separate processes and fall outside both checks.
- The source is in this repository, under the MIT license. Every change to this
  page, and to what Nudge stores or sends, shows in the git history.

## Changes

If Nudge ever gains anything that sends data, it will be opt-in, off by default,
documented here first, and visible before it is sent.
