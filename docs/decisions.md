# Decisions

Short records of choices that aren't obvious from the code. The newest go last.

## D1. Thin `hyakume` launcher (2026-10-24)

`hyakume` is the CLI and never links GPUI. With no arguments it `exec`s
`hyakume-desktop`, found next to itself or on `PATH`, and exits 127 if that's
missing. It's one command name everywhere, and headless installs skip the GUI.
Rejected: linking the CLI into the desktop binary (agents would need a second
command name on servers), and a `gui` Cargo feature (two different builds
sharing one name).

## D2. GPUI 0.2.2 and the libc pin (2026-10-24)

`gpui = "=0.2.2"` is the latest published crate. Its `xattr` 0.2.3 dependency
needs `libc::ENOATTR`, which libc 0.2.191 removed on Linux, so `Cargo.lock`
pins libc 0.2.186. After any `cargo update`, run
`cargo update -p libc --precise 0.2.186` again. Drop the pin once GPUI publishes
a release without that dependency.

## D3. One schema for local and server databases (2026-10-24)

Separate files with the same tables and the same queries. Local work is a
`personal` workspace owned by one identity, `me`. Queries use SQLx's runtime
API and are tested against real migrated databases, which avoids keeping
offline macro metadata in sync with three schemas. Writes use
`BEGIN IMMEDIATE`. Reads that need several queries, such as a project
snapshot, run in one read transaction so they all see the same commit. A
workspace is always created together with its owner in one transaction.

## D4. Events name changes; saves check versions (2026-10-24)

WebSocket events say what changed, never the content, and clients refetch
through permission-checked endpoints. The server subscribes a socket before
sending `subscribed`, and clients fetch only after that, so no commit is missed.
Saves replace the whole task and must match `expected_version`. A stale save
gets 409 with the latest task, and nothing is merged automatically.

## D5. Dev login and credentials (2026-10-24, prototype only)

Dev login works only with `dev_mode = true`, for identities listed in the
config. Dev mode refuses a non-loopback bind unless `dev_allow_remote = true`.
Two machines use SSH forwarding. Clients store tokens in the OS keyring. If the
keyring is unavailable, sign-in fails with an explanation.
`credential_store = "dev-file"` writes 0600 files named by the hex of the
server URL and identity. The client uses those tokens only while the server
reports dev mode.

## D6. Layout sync scope (2026-10-24, known gap)

Shared-workspace layouts, including a server project's place on the combined
overview, live on that server under the person's identity, so they follow the
person across devices. Local project layouts stay on one device. That misses
the cross-device requirement and must be closed before the usable release.

## D7. Names match the way people type them (2026-10-25)

Stages, relation types, and roles are looked up with `name_key`: case, `-`,
`_`, and extra spaces don't count. Storage refuses a new relation type or role
whose name matches an existing one that way, and the CLI refuses a lookup that
matches two, so a typed name never quietly picks the wrong thing. A directed
relation type must have a reverse label, so a link reads correctly from both
ends. Snapshots and other multi-query reads run in one read transaction.

## D8. A Tokio thread and a painted canvas (2026-10-25)

All network, keyring, and SQLite work runs on one Tokio thread. The GPUI
window talks to it through two Tokio channels, so neither executor runs the
other's futures. The canvas is one `canvas()` element that paints everything
and hit-tests by hand, which keeps thousands of eyes cheap. Rejected: one GPUI
element per eye, and running GPUI work on Tokio. Meme is painted the same way,
from the mockup's geometry, because GPUI 0.2.2 swaps red and blue in SVG
images. Revisit SVG once GPUI converts SVG pixels to BGRA like other formats.

## D9. Two built-in themes, custom ones from TOML later (2026-10-25)

Hyakume ships only light and dark, following the system. Every color it draws,
Meme's included, comes from the one `Theme` struct, and new code keeps it that
way. Custom themes arrive with profile customization in the usable release:

- A file in the profile's `themes/` folder names a base (`base = "dark"`) and
  overrides only the colors it changes, so old themes survive new colors.
- One file can hold a light and a dark variant, so following the system still works.
- `client.toml` picks one (`theme = "kanagawa"`), and edits reload live.
- Eye shapes carry status, so no palette can hide it; the app warns when text
  or the blocked color is too close to the background. Meme's colors can
  change, its shape can't.
- Reading the current Omarchy palette is an optional extra, never a dependency.

Rejected for now: more built-in themes. Each one is another full palette to
check on every new screen while the palette is still settling.

## D10. Event streams are pinged (2026-10-25)

A WebSocket can die without a close frame, and a client waiting for events
can't tell that from a quiet workspace. Clients ping every 20 s; axum answers
pings on its own, so a stream that hears nothing for 60 s is treated as lost
and reconnects. No wire change and no server work. Rejected: a heartbeat
event in the `/v1` protocol, which every client would need to understand.

