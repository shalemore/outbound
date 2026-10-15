# outbound

[![CI](https://github.com/shalemore/outbound/actions/workflows/ci.yml/badge.svg)](https://github.com/shalemore/outbound/actions/workflows/ci.yml)
[![License](https://img.shields.io/github/license/shalemore/outbound)](LICENSE)

> [!WARNING]
> outbound is in early development. Its design and documentation are still being worked out, and there may be breaking changes, including to the configuration file.

A Windows tray app that mirrors **new** files from local folders to a folder on your VPS over ssh.

- **Nothing to install on the server.** It runs the system `ssh`, so keys, ssh-agent, `~/.ssh/config` and ProxyJump work as in a terminal.
- **Verified.** A file goes to a temporary `.part` file, is checked with sha256, and is renamed only then.
- **Resumable.** After a dropped connection or a pause, the upload continues from the verified part. Failures are retried with a growing wait.
- **Never overwrites.** A file that already exists on the remote with different content is reported as a conflict and skipped.
- **Status in the tray.** A strip on the icon is the progress bar; its color is the state (teal idle, blue syncing, amber conflicts, red problems, gray paused).

## Install

```sh
scoop bucket add shalemore https://github.com/shalemore/scoop-bucket
scoop install outbound
```

Or download the `.exe` from [Releases](https://github.com/shalemore/outbound/releases) and run it. It is a single portable file. It is not code-signed, so SmartScreen may warn about a browser download; choose "More info", then "Run anyway".

## Quick start

Run `outbound`. The first run writes a configuration template and lists it as a problem in the tray menu. Then:

1. Pick the ssh host in the menu (it lists the aliases of `~/.ssh/config`).
2. Add a folder: choose the local folder, then the folder on the VPS, which must already exist.

Connect to the host once from a terminal first, to accept its host key. outbound runs `ssh` with `BatchMode`, so it never asks for a password. The remote needs a POSIX shell with `sha256sum`, `wc` and `mv`.

## Tray menu

- **Status.** What is going on, the progress, how many files are pending and uploaded. Conflicts and problems are in a submenu.
- **Pause / Resume** and **Sync now.** A pause stops the work at once and is not remembered after a restart.
- **SSH host.** Writes the chosen alias to `ssh.destination`.
- **Folders.** Tick a folder to turn it on or off. **Add folder mapping…** checks over ssh, while you type, that the remote folder exists. **Remove folder** only edits the configuration; no file is deleted, locally or remotely.
- **Open configuration file / log folder.**
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
- Subfolders are included, and missing remote subfolders are created.
- The sync state is kept in memory. After a restart, files that already exist remotely are compared again with sha256, not uploaded again.
- A `.part` file left on the remote after its source was deleted is not cleaned up.

## Clean up

To show notifications under its own name, outbound writes a registry entry (`HKCU\Software\Classes\AppUserModelId\shalemore.outbound`) and an `icon.png` next to the log, when the first notification is shown. Windows adds its own entry for your notification settings.

**Clean up and quit…** removes them. So does `outbound --cleanup`, which exits with 0 when everything is gone and 1 otherwise; the Scoop package runs it on uninstall. The configuration, the log and the program file are never deleted.

## Development

See [CONTRIBUTING.md](CONTRIBUTING.md).

## License

[MIT](LICENSE)
