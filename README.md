# outbound

[![CI](https://github.com/shalemore/outbound/actions/workflows/ci.yml/badge.svg)](https://github.com/shalemore/outbound/actions/workflows/ci.yml)
[![License](https://img.shields.io/github/license/shalemore/outbound)](LICENSE)

> [!WARNING]
> outbound is in early development. Its design and its documentation are still being worked out, and there may be breaking changes, including to the configuration file, between versions.

A tray app that mirrors **new** files from local folders to a folder on your VPS over ssh. Drop a file into a folder and the rest happens in the background.

- **Nothing to install on the server.** It runs the system `ssh`, so keys, ssh-agent, `~/.ssh/config` and ProxyJump work the way they do in a terminal. The remote only needs a POSIX shell and coreutils (`sha256sum`, `wc`, `mv`).
- **No half-written files.** A file is first uploaded to a temporary file on the remote, verified with sha256, and only then renamed to its final name.
- **Failures are retried.** A dropped connection is tried again after 2, 4, 8 seconds and so on, and the upload carries on where it stopped. An optional speed limit keeps the uploads from using the whole connection.
- **Interrupted uploads resume.** If the connection drops, or you pause, the partial `.outbound-*.part` file stays on the remote. The next attempt checks that it is really a prefix of the local file (sha256 on both sides, like `rsync --append-verify`) and sends only the missing rest. A partial file that does not match is discarded and the upload starts over. A `.part` file whose source was deleted is never cleaned up, so remove it by hand.
- **Never overwrites.** If the remote already has a file with the same name, the file is reported as a conflict and skipped when the size differs, or when the size is the same but the sha256 differs. A byte-identical file counts as already synced.
- **Several folders at once.** Every folder mapping runs in the background together, and each can override the global settings.
- **Status in the tray.** A strip at the bottom of the icon is the progress bar, and the color is the state (teal: idle, blue: syncing, amber: conflicts, red: problems, gray: paused). The menu shows what is going on, how many files are pending, how much was uploaded, the conflicts and the problems.

## Platforms

| Platform | Status |
| --- | --- |
| Windows | Supported. A single portable `outbound-x86_64-pc-windows-msvc.exe` is published on the [Releases](https://github.com/shalemore/outbound/releases) page. There is no installer: run it and it lives in the tray. |
| macOS, Linux | The code is portable and CI builds and unit-tests it, but it has never been run by hand and no binaries are published. Build it yourself with `cargo build --release`. On Linux you need GTK 3, libxdo and an app indicator library (`libgtk-3-dev libxdo-dev libayatana-appindicator3-dev` on Debian and Ubuntu), and a desktop with tray support (GNOME needs an extension). |

## Install

With [Scoop](https://scoop.sh) on Windows:

```sh
scoop bucket add shalemore https://github.com/shalemore/scoop-bucket
scoop install outbound
```

Or download the `.exe` from the Releases page and run it. Nothing is installed or written next to the program; the configuration and the log go to the folders listed below.

The executable is not code-signed. Windows SmartScreen may therefore show "Windows protected your PC" for a file that was downloaded with a browser; choose "More info" and then "Run anyway". Installing with Scoop does not show it.

## The tray menu

| Item | What it does |
| --- | --- |
| Two status rows | The first says what is going on and with which file. The second shows the progress while a file is sent, and otherwise how many files are pending and how much was uploaded since the program started. Long file names are shortened in the middle. |
| Conflicts and problems | A submenu that is grey while there is nothing to see, and otherwise counts the problems and conflicts and lists them. Long texts are shortened, the log has them in full. |
| Pause / Resume | Stops the work at once, in the middle of an upload if needed, and carries on later from the partial file. The pause is not remembered across a restart. |
| Sync now | Starts the next cycle immediately and compares earlier conflicts again. |
| SSH host | Lists the host aliases of `~/.ssh/config` (wildcard patterns are skipped). Choosing one writes it to `ssh.destination`. |
| Folders | Lists the configured folders. A check mark turns a folder on or off. |
| Folders → Remove folder | Asks, then takes the folder out of the configuration file. Nothing is deleted, neither locally nor on the VPS, and the files that were already sent stay where they are. |
| Folders → Add folder mapping… | Pick the local folder in the system folder picker. outbound then asks the VPS for its home directory and shows an input box that suggests `<home>/<folder name>`. The folder must already exist on the VPS, which is checked over ssh before anything is written. |
| Open configuration file / log folder | Opens them in the default program (Notepad for the configuration on Windows). |
| Clean up and quit… | Removes what the program has put on this computer outside its own folders, checks that it is gone, and quits. See [Clean up](#clean-up). |

Pending means files that need an upload and have not been handled yet, or that failed and will be tried again. Files that were modified so recently that they are probably still being downloaded are counted separately as "still being written".

On Linux the input box needs `zenity` or `kdialog`, and the folder picker needs a desktop portal (or `zenity`).

## Configuration

The first run writes a configuration template and shows it as a problem in the tray menu. Set it up from the tray menu, or edit the file by hand. A change is picked up on the next scan, so no restart is needed. The tray menu edits the file in place and keeps its comments.

| | Windows | macOS | Linux |
| --- | --- | --- | --- |
| Configuration | `%APPDATA%\outbound\config.toml` | `~/Library/Application Support/outbound/config.toml` | `~/.config/outbound/config.toml` |
| Log | `%LOCALAPPDATA%\outbound\outbound.log` | `~/Library/Application Support/outbound/outbound.log` | `~/.local/share/outbound/outbound.log` |

```toml
# Optional global settings. Keep them above the first table.
# scan_interval_secs = 30
# settle_secs = 60
# after_upload = "keep"
# notifications = true
# retries = 3
# bandwidth_limit = "10 MiB/s"

[ssh]
destination = "user@example.com"   # or an alias from ~/.ssh/config
# port = 22
# identity_file = "C:/Users/you/.ssh/id_ed25519"

# Local folder -> remote folder. Add as many mappings as you like;
# they all run in the background together. Both folders must already exist.
[[mapping]]
local = "D:/Downloads/outbox"
remote = "/data/videos"

[[mapping]]
local = "D:/Videos/to-send"
remote = "/data/library"
after_upload = "trash"   # per-folder settings override the global ones
settle_secs = 300
```

| Setting | Default | Meaning |
| --- | --- | --- |
| `scan_interval_secs` | `30` | How often the folders are scanned. |
| `settle_secs` | `60` | A file is uploaded only after it has not been modified for this many seconds. |
| `ignored_extensions` | `part`, `crdownload`, `tmp`, `!qb`, `aria2`, `downloading`, `opdownload` | Temporary extensions of downloaders. These files are never uploaded. |
| `after_upload` | `"keep"` | `"keep"` leaves the local file alone, `"trash"` moves it to the recycle bin. |
| `notifications` | `true` | Show system notifications. |
| `retries` | `3` | How many times a failed upload is tried again before it is reported. |
| `retry_delay_secs` | `2` | The wait before the first retry. Every further retry waits twice as long, up to a minute. |
| `bandwidth_limit` | none | The most the uploads may send per second, such as `"500 KiB/s"` or `"10 MiB/s"`. KiB, MiB and GiB count in 1024s, KB, MB and GB in 1000s. |
| `enabled` (per folder) | `true` | `false` keeps the mapping in the file but ignores it. |

`after_upload`, `settle_secs` and `ignored_extensions` can also be set inside a `[[mapping]]`, and then apply to that folder only.

With `after_upload = "trash"`, a local file is moved to the recycle bin once the remote is known to hold exactly the same content, either because it was just uploaded and verified or because an identical file was already there. It can be restored from the recycle bin. Conflicting files are never touched. If the recycle bin is not available the problem is reported and the file stays, and the next cycle retries the move without uploading again.

## Notifications

outbound shows a system notification when a cycle has uploaded files, when a conflict appears and when a problem appears. A conflict or a problem is announced once, not on every scan, and several new ones are combined into one notification. Set `notifications = false` to turn them off.

- **Windows.** Toasts are attributed to "Outbound" and carry its icon. For that the program writes one registry entry, `HKCU\Software\Classes\AppUserModelId\shalemore.outbound`, and the icon as `icon.png` next to the configuration. It does this when it shows its first notification, and a run that never notifies leaves nothing behind. Windows also creates a matching entry under `HKCU\Software\Microsoft\Windows\CurrentVersion\Notifications\Settings` for your choices in the notification settings.
- **macOS.** A program without an app bundle cannot own notifications, so they appear as coming from Finder. Not tested.
- **Linux.** A notification daemon has to be running. Not tested.

## Clean up

outbound is meant to be portable, and the only things it puts on a Windows computer outside its own folders are the notification entries above. "Clean up and quit…" in the tray menu removes them, checks that they are really gone, and only then quits. Your configuration and log stay, and so does the program file, which is never deleted. If something cannot be removed, the dialog says what, and the program keeps running with notifications turned off.

The same thing from a terminal or a script, without any window:

```sh
outbound --cleanup
```

It exits with 0 when everything is gone and with 1 otherwise. A Scoop package would run it before an uninstall. If you delete the program file without cleaning up first, what is left is harmless: a "Outbound" item in the notification settings, which you can remove by deleting the two registry keys mentioned above.

## Behavior

- It only mirrors additions from local to remote. It never deletes or modifies remote files. Local files are left alone unless you set `after_upload = "trash"`, and even then they only go to the recycle bin.
- Subfolders are handled recursively and missing remote subfolders are created. The mapped root folders must exist on both sides, otherwise they are reported as problems.
- It runs `ssh -o BatchMode=yes`, so it never asks for a password. Connect once from a terminal first to accept the host key.
- Sync state is kept in memory only. After a restart, files that already exist on the remote are compared once more (sha256 on both sides) and are not uploaded again.
- A failed upload is tried again at once, after a wait that doubles each time, and resumes from the partial file, so a dropped connection costs little. Only when all retries fail is it reported as a problem, and then it is tried again with the next scan. Problems that retrying cannot fix, such as a missing `ssh`, are reported at once. Pausing during a wait stops the retries.
- Pausing takes effect within a chunk of an upload. Comparing a large file with an identical-looking remote file finishes first.

## Development

See [CONTRIBUTING.md](CONTRIBUTING.md).
