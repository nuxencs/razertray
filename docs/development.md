# Development

## Build

razertray targets Windows. Build it on Windows with the MSVC Rust toolchain and
the Visual Studio Build Tools:

```bash
cargo build --release --target x86_64-pc-windows-msvc
```

The binary is `target/x86_64-pc-windows-msvc/release/razertray.exe`.

On macOS and Linux, the code compiles, the tests run, and `--once` runs. The tray
mode exits with an error on these platforms.

## Checks

Pull requests must pass the same checks as CI:

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
cargo audit
```

Most of the tray code compiles only for Windows. On macOS or Linux, also run
clippy for the Windows target:

```bash
cargo clippy --all-targets --target x86_64-pc-windows-msvc -- -D warnings
```

## Rust version

`rust-toolchain.toml` pins the Rust version, with clippy, rustfmt and the
Windows target. rustup installs it on the first `cargo` command in the
repository, and CI installs it with `rustup toolchain install`.

The pin keeps CI stable: a new Rust release cannot add clippy lints that fail
an unchanged branch. To move to a newer Rust version:

1. Change `channel` in `rust-toolchain.toml`.
2. Run all [checks](#checks), on both targets.
3. Fix the new lint findings in the same pull request.

## Device list

`src/device_map.rs` is generated from an OpenRazer checkout. Do not edit it by
hand:

```bash
tools/extract_openrazer_map.py <path-to-openrazer> src/device_map.rs
rustfmt src/device_map.rs
```

The `update-device-map` workflow does this every week and opens a pull request
when the list changes.

## Icon image in the docs

`docs/assets/tray-icons.svg` is drawn from the real icon pixels. A test fails
when the icon code changes and the image does not. To draw the image again:

```bash
UPDATE_ICON_PREVIEW=1 cargo test icon_preview
```

## Releases

See [Releases](releases.md) for release notes, labels and the release workflows.
