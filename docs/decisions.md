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


## D11. The keyboard map (2026-10-25)

All shortcuts live in one table in `keys.rs`, and the palette reads its hints
from it. Window-wide keys sit in a root `Hyakume` key context, because in GPUI
a binding with no context outranks every contextual one and would beat the
palette's own Escape. Letters without Ctrl or Alt are bound only on the canvas,
so they can never eat what someone types; a test enforces that.

- Ctrl+Shift+P palette, Ctrl+F find in the current view (one project when
  focused, everything on the overview), Esc steps back one level.
- Arrows move between eyes, or between webs on the overview; Shift+arrows pan.
- Enter opens what the arrows are on. O and I follow outgoing and incoming
  links; an undirected link counts both ways.
- Tab and Shift+Tab go through the canvas and every button, in reading order.

Rejected for now: Vim keys as a built-in mode (PROJECT.md calls them optional;
`keys.toml` from [D12](#d12-your-own-shortcuts-in-keystoml-2026-10-25) can bind
them) and `[`/`]` for links, which need AltGr on many layouts.

## D12. Your own shortcuts in `keys.toml` (2026-10-25)

`keys.toml` sits next to `client.toml` in the profile. Each section is a key
context, and each line binds a key to an action or unbinds it with `false`.
The file is added on top of the defaults rather than replacing them, so a
file that only adds `h = "MoveLeft"` keeps the arrows working. GPUI already
lets later bindings win at the same depth and treats `NoAction` as an unbind,
so the loader just appends.

Every entry is checked when it loads: the section exists, the action exists
and is handled in that section, keys that type a character only go in
`[canvas]`, an unbind names a key that's actually bound there, and spellings
like `esc` get pointed to `escape`. A bad entry is skipped on its own and
reported in a notice and on stderr. The app always starts. The built-in
table passes through the same checks in a test.

Hints come from GPUI's live keymap, not the built-in table, so the palette,
header, and canvas reminder show the person's keys. A key only shows if GPUI
would really run that action for it from where focus usually is, so a key
taken by another action, or bound only in `[text]`, never shows. The palette's "Reload
keyboard shortcuts" applies an edited file without a restart.

Rejected: Zed-style JSON (the rest of the profile is TOML); replacing the
defaults wholesale (one forgotten line would lose Escape); watching the file
for changes (a reload command is enough for now, and needs no watcher thread).

## D13. Threads keep clear of text (2026-10-26)

A thread never runs through a web's name, its task count, or a column
header. It bends sideways around them instead, picking the smallest bend that
clears them all. Eye labels are softer: a thread bends around one only when
that takes no steep swing, and otherwise passes behind it. Each label sits on
a plate painted in its web's own color. The plate can't be seen, but it hides
any line underneath, and arrowheads are drawn after the labels so none
disappears. Stubs aren't placed straight above or below a web either, since
that's where its name and count sit.

Rejected: routing around eye labels too (a thread can't leave an eye without
touching the label right under it, and avoiding every label at any cost
swung short links into loops); hiding a web's name when a thread crosses it
(the name matters more than the thread); and a general obstacle-avoiding path
search (one bent curve is enough for a few dozen links and looks like silk).

Amended the same day, after a 150-task test: eye labels no longer steer
threads at all. Dodging them cost 45 ms a frame in a busy web, and the plates
already hide every line behind a label. A thread on the overview also stays
inside its workspace's column, and is clipped to it if no bend gets clear.
Stubs keep clear of the name and count when several are spread around one
web as well.

## D14. Links at scale: bundles and lighting (2026-10-26)

A workspace with 150 tasks and about 90 links drew every link at once and
became a tangle. Now:

- The overview draws one bundle per pair of projects, thicker the more links
  it holds, with a count pill where one fits. Links inside a project are
  hidden there.
- An opened project draws all of its links, fainter the more there are.
- Hovering or selecting a task lights it, its links, and the tasks at their
  other ends; everything else fades. Hovering a web on the overview lights
  its bundles. A task with no links lights nothing, so moving the pointer
  across a web doesn't flicker.
- Labels yield instead of overlapping: they're placed in order (selected or
  hovered task, lit tasks, the rest), each trying eight spots around its eye,
  and one that would cover a heading, an eye, or another label is left out.
  The selected or hovered task's label always shows, like a tooltip.
  A label only takes a spot nearer its own eye than any other, so it never
  reads as a neighbour's.
- Zoomed out, the selected or hovered task's tooltip is one line. If no spot
  near the eye stays off its linked tasks, it becomes a card just outside
  the web, joined to the eye by a thin line, on the canvas and off the
  task's own links.
- Clicking the selected task again lets go of it: the details close, the
  light goes out, and the pointer resting on it doesn't light it again until
  it moves off. The details panel opening or closing doesn't move the view,
  so the clicked task stays under the pointer.
- A column header scrolled under the top edge stays pinned there on a band
  of background, and everything passes under the band. Web discs are opaque,
  so a bundle passing behind a web is hidden, and bundles steer around webs
  between their ends where the column has room.

Rejected: fading everything without bundling (the overview stays busy),
keeping every link drawn and only speeding it up (the screenshot was
unreadable at 60 fps too), and a pill on every bundle whatever it covers.

## D15. Zoom-out stops near Fit (2026-10-26)

Text keeps its size while webs shrink, so far enough out a web's name lands
on the web above it. The view now can't zoom out past 80% of what fits
everything, nor past the zoom where some web's name would touch the web
above. Fit itself never goes below that second limit: if a column is too tall
for both, it fits from the top and the rest is a scroll away. Webs in a
column sit 240 world units apart (was 110), so with a 150-task workspace
beside small ones the names still fit at about 20%.

Columns are also made wide enough for their workspace title and every web
name at that lowest zoom, never below 18%, so names don't shrink to their
key sideways either. Text keeps its size, so only the painter can measure
it: the first render after new data widens what's too narrow and lays the
webs out again. A web zigzags at most 60 world units off its column's
middle, so a widened column doesn't push its webs to the edges.

Rejected: hiding the name when there's no room (names come first), drawing
it on a plate over the web above (it would cover that web's tasks), and
moving it inside its own web. Rejected for widths: showing the full name only on hover (it
should be readable without the mouse), and two-line names (long names could
still be cut).

## D16. Libraries optimized in debug builds (2026-10-26)

`[profile.dev.package."*"] opt-level = 2` compiles every dependency
optimized in debug builds, our own crates as usual. With 150 tasks and labels
showing, a debug frame went from about 25 ms to about 1 ms: nearly all of it
was GPUI's text shaping, unoptimized. The first build after a clean takes
about a minute and a half longer, and a debugger can't step through library
code.

Rejected for now: caching label placement and routes between frames. After
the profile change there was no frame time left for it to save, and its
invalidation (zoom, data, highlight, pinned headers) would be a new place
for stale drawings to come from.

## D17. Stage filters go by name (2026-10-26)

A stage filter hides tasks by stage name across every project on screen,
compared with `name_key`, so hiding Done hides every project's Done. Most
projects keep the default stages, and someone hiding finished work wants
it gone everywhere, not one project at a time. The palette offers one entry
per stage name in use; a stage nobody's tasks are in has nothing to hide.

Rejected: filtering by eye shape (a custom "QA" stage would hide with
Review, under a name the person never typed), per-project filters (more
state for no gain while stages are mostly shared), and saving filters
across restarts (a forgotten filter makes work look missing).
