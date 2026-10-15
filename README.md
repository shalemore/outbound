# outbound

[![CI](https://github.com/shalemore/outbound/actions/workflows/ci.yml/badge.svg)](https://github.com/shalemore/outbound/actions/workflows/ci.yml)
[![License](https://img.shields.io/github/license/shalemore/outbound)](LICENSE)

> [!WARNING]
> Early development: the design and docs are still in flux, and breaking changes are likely.

A Windows tray app that mirrors **new** files from local folders to a folder on a Linux server over ssh.

- **No server setup.** It uses your system `ssh`, so nothing is installed on the server.
- **Verified.** Every upload is checked with sha256 before it appears.
- **Resumable.** An interrupted upload continues where it stopped, and failures are retried.
- **Never overwrites.** A different file with the same name is a conflict, not a replacement.

## Install

```sh
scoop bucket add shalemore https://github.com/shalemore/scoop-bucket
scoop install outbound
```

Or download the portable `.exe` from [Releases](https://github.com/shalemore/outbound/releases) and run it.

> [!NOTE]
> The `.exe` is not code-signed, so SmartScreen may warn about a browser download. Choose "More info", then "Run anyway". Installing with Scoop avoids this.

## Quick start

> [!IMPORTANT]
> The server must run Linux. macOS, BSD and Windows servers are not supported.

> [!NOTE]
> outbound never asks for a password. Connect to the server once from a terminal first, to accept its host key.

Run `outbound`. The first run writes a configuration template and lists it as a problem in the tray menu. Then:

1. Pick the ssh host in the menu (it lists the aliases of `~/.ssh/config`).
2. Add a folder: choose the local folder, then the folder on the server, which must already exist.

## Tray menu

- **Icon.** A strip on the icon is the progress bar, and its color is the state: teal idle, blue syncing, amber conflicts, red problems, gray paused.
- **Pause.** It is not remembered after a restart.
- **SSH host.** Writes the chosen alias to `ssh.destination`.
- **Folders.** Tick a folder to turn it on or off.
- **Add folder mapping…** Checks over ssh, as you type, that the folder on the server exists.
- **Remove folder.** Only edits the configuration. No file is deleted.
- **Clean up and quit…** See [Clean up](#clean-up).

## Configuration

`%APPDATA%\outbound\config.toml`. The menu edits it in place and keeps your comments, and a change is picked up on the next scan. The log is `%LOCALAPPDATA%\outbound\outbound.log`.

```toml
# Global settings, all optional (defaults shown). Keep them above the first table.
scan_interval_secs = 30
settle_secs = 60              # upload a file once it has been left alone this long
ignored_extensions = ["part", "crdownload", "tmp", "!qb", "aria2", "downloading", "opdownload"]
after_upload = "keep"         # "trash" moves the local file to the recycle bin
notifications = true
retries = 3
retry_delay_secs = 2          # doubles with every retry, up to a minute
# bandwidth_limit = "10 MiB/s"

[ssh]
destination = "user@example.com"   # or an alias from ~/.ssh/config
# port = 22
# identity_file = "C:/Users/you/.ssh/id_ed25519"

[[mapping]]
local = "D:/Downloads/outbox"
remote = "/data/videos"
# enabled = false             # keep the mapping but ignore it
# after_upload, settle_secs and ignored_extensions can be set here too
```

`after_upload = "trash"` moves a file only once the remote is known to hold identical content, and never touches a conflicting file.

## Notes

- Only additions are mirrored. Remote files are never deleted or modified.
- The sync state is kept in memory. After a restart, files that already exist remotely are compared again with sha256, not uploaded again.
- A `.part` file left on the remote after its source was deleted is not cleaned up.

## Clean up

To show notifications under its own name, outbound writes a registry entry (`HKCU\Software\Classes\AppUserModelId\shalemore.outbound`) and an `icon.png` next to the log, when the first notification is shown. Windows adds its own entry for your notification settings.

**Clean up and quit…** removes them. So does `outbound --cleanup`, which exits with 0 when everything is gone and 1 otherwise; the Scoop package runs it on uninstall. The configuration, the log and the program file are never deleted.

## Development

See [CONTRIBUTING.md](CONTRIBUTING.md).

## License

[MIT](LICENSE)
