# Hyakume ༼⁠ ⁠つ⁠ ⁠◕⁠‿⁠◕⁠ ⁠༽⁠つ

> A lovely bridge between trusted devices, where sharing is a choice and trust
> is earned.

Hyakume is a planned app for sending files, text, links, and explicitly chosen
clipboard text between devices without an account.

Its mascot is Meme, a friendly hundred-eyed creature. Each paired device is one
of Meme's eyes.

## How it will work

1. Pair two devices and confirm that their verification displays match.
2. Choose a device and the content to send.
3. The receiving device shows an offer that you can accept or reject.
4. Hyakume transfers encrypted content and verifies it before making a received
   file available, without overwriting existing files.

Paired devices remember each other. Temporary sharing will connect one verified
recipient for one transfer without creating permanent trust.

Local transfers will work without internet access. Internet transfers will prefer
a direct connection, with an encrypted relay when needed. Clipboard access always
requires an explicit action.

## What we are building

Start with a Linux command-line app, then a GPUI desktop interface for Linux and
Windows. Android follows later in the v1 plan, along with internet support.
macOS and iOS are planned after v1. Rust will provide the shared trust and
transfer logic; mobile apps will use native interfaces.

The landing page will live at `hyakume.org`, which is already acquired. It will
use static Astro and pnpm in the separate `hyakume-front` repository, hosted on
Cloudflare Pages. It will provide project information, release notes,
documentation, and downloads; pairing and transfers stay in the installed apps.

See [PROJECT.md](PROJECT.md) for the full plan.

## Development

Use [mise](https://mise.jdx.dev/) to install Rust from `rust-toolchain.toml` and
run the project commands:

```sh
mise trust
mise install rust
mise run ci
```

| Command | Runs |
| --- | --- |
| `mise run fmt` | `cargo fmt --all` |
| `mise run fmt-check` | `cargo fmt --all -- --check` |
| `mise run lint` | `cargo clippy --locked --all-targets -- -D warnings` |
| `mise run check` | `cargo check --locked --all-targets` |
| `mise run test` | `cargo test --locked` |
| `mise run build` | `cargo build --locked` |

`mise run ci` checks formatting, runs Clippy and tests, then builds the app.
The Linux CI runs the same task for pull requests and pushes to `shrek`.
Clippy already checks compilation, so CI does not repeat `mise run check`.

Contact: [vincent@sshmoi.com](mailto:vincent@sshmoi.com).

Licensed under [Apache-2.0](LICENSE).
