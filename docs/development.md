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
rustup target add x86_64-pc-windows-msvc
cargo clippy --all-targets --target x86_64-pc-windows-msvc -- -D warnings
```

## Device list

`src/device_map.rs` is generated from an OpenRazer checkout. Do not edit it by
hand:

```bash
tools/extract_openrazer_map.py <path-to-openrazer> src/device_map.rs
rustfmt src/device_map.rs
```

The `update-device-map` workflow does this every week and opens a pull request
when the list changes.

## README image

`docs/assets/tray-icons.svg` is drawn from the real icon pixels. A test fails
when the icon code changes and the image does not. To draw the image again:

```bash
UPDATE_ICON_PREVIEW=1 cargo test icon_preview
```

## Releases

See [Releases](releases.md) for release notes, labels and the release workflows.
