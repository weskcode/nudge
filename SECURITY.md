# Security

## Reporting a problem

Please report security problems privately through GitHub: open the
[Security tab](https://github.com/weskcode/nudge/security) of this repository
and choose "Report a vulnerability". Don't open a public issue for them.

Include the version (shown in About Nudge), your operating system, and steps
to reproduce. You should hear back within a week.

## Supported versions

Fixes go into the latest release only.

## What Nudge does on your computer

- It reads and writes its settings in `config.json` in your user config
  folder (`~/Library/Application Support/nudge/` on macOS) and writes the
  chime sound to your user cache folder.
- It keeps local diagnostics by default in a `diagnostics` folder next to
  `config.json`: a journal of counts, fixed step names and coarse durations
  for the last 20 launches, a lock file, and a marker file when you turn the
  setting off. They hold no reminder text, paths, names, exact times, device
  identifiers or panic messages, and Nudge never sends them. Review
  diagnostics in Settings shows the exact text that would be shared, and
  copying it is your choice. Deleting the folder removes the record; quit Nudge
  before you delete it. Turning the setting off is also kept in
  `diagnostics.off`, beside the folder, so deleting the folder does not turn
  recording back on.
- It makes no network connections. The only link it opens is the credit to
  remindful, in your browser, when you click it.
- On macOS, with Input Monitoring allowed, it is told the key code of each
  key press so that any key can close a reminder. It ignores key presses
  while no reminder is showing and never records, stores or sends them.
- Open at login adds a LaunchAgent in `~/Library/LaunchAgents` on macOS, a
  Run entry in the registry on Windows, and an entry in `~/.config/autostart`
  on Linux.
