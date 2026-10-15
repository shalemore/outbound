# Contributing

You need Rust 1.89 or newer and Windows. `build.rs` embeds the icon, so the Visual Studio build tools (`rc.exe`) are required.

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
cargo build --release
```

These are the checks CI runs. The sync logic reaches the remote through the `Remote` trait, and the tests use a local folder instead of a server. The tests of `src/ssh.rs` run its real shell scripts through a stand-in for `ssh` and need an `sh`; after changing one of those scripts, also try it against a real VPS.

Everything in the repository is written in English.

## What the program writes outside its folders

- To show toasts under its own name, `HKCU\Software\Classes\AppUserModelId\shalemore.outbound` and an `icon.png` next to the log, when the first toast is shown. Windows adds its own entry under `Notifications\Settings`.
- For **Start with Windows**, the value `outbound` in `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`. It is rewritten at start if the program has moved.

**Clean up and quit** and `outbound --cleanup` remove all of it. The command exits with 0 when everything is gone and 1 otherwise. The Scoop manifest runs it before an uninstall, but not before an update, which would lose the startup entry.

## Icons

`assets/icon.svg` is the source. The other icons (`assets/icon.ico`, `assets/icon-256.png`, `assets/tray/*.rgba`) are generated from it and committed, so building needs no SVG renderer. After changing the SVG:

```sh
cargo run --release --manifest-path tools/make-icons/Cargo.toml -- build assets
```

Colors are written in OKLCH, and the tool converts them to sRGB. The four tray states share one lightness; their hues are the `TRAY_STATES` table in `tools/make-icons/src/main.rs`. Change a color there, not in a generated file. `preview` draws icons at several sizes into one picture:

```sh
cargo run --release --manifest-path tools/make-icons/Cargo.toml -- preview sheet.png a.svg b.svg
```

## Releasing

Change `version` in `Cargo.toml` on `main` (and `Cargo.lock`), and commit it as `chore(release): x.y.z`. When the version has no tag yet, `release.yml` builds the binary and creates a GitHub release listing the commits since the previous one.

The release has three files: the program, its sha256 and a Scoop manifest. `tools/package` makes them from `Cargo.toml` (including its `[package.metadata.scoop]` table) and the build; CI runs it on every change.

### The Scoop bucket

After publishing, the workflow starts `Import manifest` in the `scoop-bucket` repository of the same owner (or the one in the repository variable `SCOOP_BUCKET`), which opens a pull request with the new manifest. It needs:

- the secret `SCOOP_BUCKET_TOKEN` here: a fine-grained token for the bucket repository only, with *Actions: Read and write*;
- *Allow GitHub Actions to create and approve pull requests* enabled in the bucket repository.

Without the secret the release is still published. To import a release later:

```sh
gh workflow run import-manifest.yml --repo <owner>/scoop-bucket -f repo=<owner>/outbound -f tag=vX.Y.Z
```
