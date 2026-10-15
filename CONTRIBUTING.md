# Contributing

You need Rust 1.89 or newer. The program is for Windows only, and so is CI.

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
cargo build --release
```

These are the same checks as CI.

The sync logic (`src/sync.rs`) reaches the remote through the `Remote` trait, and the tests use a local directory instead of a real ssh server, so the tests need no server. The tests of `src/ssh.rs` run its real shell scripts through a stand-in for `ssh` that executes the remote command locally; they need an `sh`, and skip themselves without one. After changing a shell script in `src/ssh.rs`, try it against a real VPS by hand.

On Windows, `build.rs` embeds the icon and the version information into the executable. It needs `rc.exe`, which comes with the Visual Studio build tools.

Everything in the repository is written in English.

## Icons

`assets/icon.svg` is the source of the icon. The executable icon (`assets/icon.ico`), the notification icon (`assets/icon-256.png`) and the tray images (`assets/tray/*.rgba`, one per state) are generated from it and committed, so building the program does not need an SVG renderer. After changing the SVG, regenerate them with the small tool in `tools/make-icons`:

```sh
cargo run --release --manifest-path tools/make-icons/Cargo.toml -- build assets
```

Colors are chosen in OKLCH (lightness, chroma, hue), where equal numbers look equally light and equally colorful, and the tool turns them into sRGB when it renders. The SVG uses `oklch(...)`, and the colors of the tray states are the `TRAY_STATES` table in `tools/make-icons/src/main.rs`: four states share one lightness, so none of them stands out and the white glyph is equally readable on all of them. Change a color there, not in a generated file. The conversion has its own tests: `cargo test --release --manifest-path tools/make-icons/Cargo.toml`.

The tool looks for the background color and the `glyph` group in the SVG, so keep both. `preview` draws icons at several sizes into one picture, which helps when comparing designs:

```sh
cargo run --release --manifest-path tools/make-icons/Cargo.toml -- preview sheet.png a.svg b.svg
```

## Releasing

Change `version` in `Cargo.toml` on `main` (and update `Cargo.lock`), and commit with the message `chore(release): x.y.z`. When `release.yml` finds that the version has no tag yet, it builds the Windows binary and creates a GitHub release whose notes list the commits since the previous release.

The release has three files: the program (`outbound-x86_64-pc-windows-msvc.exe`), its sha256, and a Scoop manifest (`outbound.scoop.json`). `tools/package` (a small Rust program) makes all three from `Cargo.toml` (name, version, description, license, `repository` and the `[package.metadata.scoop]` table) and from the build, so the manifest repeats nothing that is written down elsewhere. CI runs it on every change.

### The Scoop bucket

After publishing, the release workflow starts the `Import manifest` workflow of the bucket (the repository `scoop-bucket` of the same owner, or the one named by the repository variable `SCOOP_BUCKET`). That workflow downloads the manifest, creates or updates it in the bucket, and opens a pull request there; see the README of the bucket. For this to work:

- The repository needs the secret `SCOOP_BUCKET_TOKEN`: a fine-grained personal access token for the bucket repository alone, with the permission *Actions: Read and write*.
- In the bucket repository, Actions must be allowed to create pull requests.

Without the secret, the release is still published and the workflow only prints a notice. A release can be imported later with `gh workflow run import-manifest.yml --repo <owner>/scoop-bucket -f repo=<owner>/outbound -f tag=vX.Y.Z`.
