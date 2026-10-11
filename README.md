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

## Where things stand

`hyakume` prints help and version, and nothing else yet:

```sh
cargo run --locked -- --help
```

It has no pairing, transfers, or key storage, and it opens no network
connections.

**Current work:** the deterministic core and loopback protocol.

**Blockers:** none. The core design needs an external review before it is
finished, so arrange a reviewer early.

**Next three tasks:**

1. Model invitations, trust, revocation, item and batch outcomes, and the
   order of revocation against publication as explicit Rust states.
2. Define bounded, escaped projections of peer text and limits for control
   messages, before peer values reach consent prompts or logs.
3. Choose and record the PAKE, identity, and session design, with the Iroh
   version and API evidence it relies on and non-secret test vectors.

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

Set `HYAKUME_LOG=debug` (or `error`, `warn`, `info`, `trace`) to write
diagnostics to standard error. Diagnostics redact secrets, escape control
characters, and keep each event to one line of at most 4 KiB. Formatting
stops at the same limit, so a huge value costs no extra memory.

The lint policy is in `Cargo.toml`: no `unsafe` code, and Clippy warnings
for `unwrap`, `expect`, `panic!`, `todo!`, and `dbg!` outside tests. CI
treats warnings as errors.

Contact: [vincent@sshmoi.com](mailto:vincent@sshmoi.com).

## License

Licensed under [Apache-2.0](LICENSE).

Third-party code reaches Hyakume only through the crates in `Cargo.lock`.
Each binary release will include the license text and copyright notices of
every crate compiled into it, generated from the locked graph at release
time. Dependencies must use licenses compatible with Apache-2.0. Source
code copied from elsewhere is not accepted without a recorded license and
attribution.
