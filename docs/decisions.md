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
