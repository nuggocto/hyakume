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

Update 2026-10-26: the desktop app now saves and follows these layouts
([D20](#d20-hand-placed-nodes-are-pinned-2026-10-26)). Local layouts still
stay on one device, so the gap above remains open.

Closed 2026-10-27: local work itself lives on one device, so its layout
can only go where the work goes. `hyakume local export` and `import` carry
both together ([D21](#d21-local-work-moves-by-file-2026-10-27)). Nothing
is published anywhere: the person moves the file.

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

## D18. The window edits through the CLI's calls (2026-10-26)

The desktop app changes work through the same `OpenWorkspace` and `Remote`
calls as the CLI, sent to the I/O thread as `Command::Write` and answered
with `Update::Wrote`, matched by request number. A saved task goes on screen
from the answer straight away; the change event that follows refetches the
rest, so nothing is merged by hand on the client.

- Stage, assignee, and priority are picked in the palette. Picked while the
  form is on screen, they go into the draft; otherwise they save at once
  against the version on screen, and a conflict opens the same review as the
  form. A draft left open on another task is never the target.
- A refused save keeps the draft. The review shows the latest version and the
  draft, marks differing fields, and offers "Keep editing my draft", which
  bases the draft on the latest. Escape keeps the draft too, so Discard has no
  key.
- The window checks offline state and permissions before sending, to explain
  a refusal instead of failing. The server checks again regardless.
- Links are made in two picks: how it reads from this task ("blocks" or
  "blocked by"), then the other task. Local work has no roles to make or give.
- Forms stay editable while saving. The answer is matched against what was
  sent: a form typed into since stays open on top of the saved version, and
  a refused save takes the live form into the review. Dialogs and comment
  boxes only react to their own request.
- Labels are edited one per line, since a label may hold a comma but never
  a newline.
- Del asks before deleting, and Escape answers "keep it". The server can't
  bring a task back, so one key must never be enough.
- Roles are given and taken one at a time from the palette. The server
  replaces a person's whole set, built from what was last loaded; two
  admins at the same instant could undo each other, which the change event
  shows straight away. A versioned set would fix it, at a cost the
  5–30-person target doesn't justify yet.

Rejected: drop-downs for stage and assignee (a second list widget to build,
and the palette already searches), a field-by-field merge on conflict
(PROJECT.md asks for review, not merging), and optimistic local edits before
the server answers (an edit the server refuses would have to be undone on
screen).

## D19. Offline copies in cache.db (2026-10-26)

Each server's workspaces are stored as JSON in `cache.db`, one row per
workspace, keyed by server URL plus signed-in identity, and replaced whole
after every load. The key is the one the connection signed in with, read
together with its token rather than again at save time. A restart while the
server is unreachable shows that copy, greyed and read-only. A row that no
longer parses after an upgrade is skipped and replaced by the next load.

It's a separate file from local.db, so personal work and team copies never
share a table, and deleting it only loses the copies. Both files take the
umask's 0644, so Hyakume creates the data directory 0700: that's what keeps
other users on the machine out. A directory that already exists keeps its mode.

Update 2026-10-27: comments are kept for the tasks a person opened, one
row per task with the server's whole answer, under the same key. Offline,
that answer stands in with its age. Only an unreachable server falls back
to it, never a refusal. Replacing the workspace copy drops the comments
of tasks it no longer holds. A refusal also takes the workspace copy off
the screen: signed out, or failing for any reason other than not reaching
the server, the column shows that and nothing else. Rejected: fetching every comment with each
load, which would make a big workspace's refresh much heavier for a copy
that's rarely read.

Rejected: mirroring the server schema in a cache database (every schema
change would need two migrations for a copy that's only ever read whole),
keeping the copy in memory only (PROJECT.md wants offline work readable after
a restart), and allowing edits offline to sync later (that's a merge design
the milestone leaves out: offline shared work is read-only).

## D20. Hand-placed nodes are pinned (2026-10-26)

Dragging or nudging an eye or a web pins it where it lands. Re-layout places
everything else again and leaves pinned nodes alone. P pins or unpins
without moving. Positions save per node: a task's relative to its web's
center, a web's relative to its column's left edge, so a web that moves
carries its eyes along.

A device applies a saved position only when it differs from the last saved
value it applied for that node. A refresh fetched before this device's own
save lands carries the old value, and treating that as news would drag the
node back.

A pinned web also never moves for placement: unpinned webs are pushed past
it, its column grows to hold it, and widening a column for names slides
only unpinned webs.

On the overview, a plain drag pans wherever it starts and Ctrl+drag moves
the web or eye under the pointer. Webs cover most of the overview, so when a
drag on a web moved it, panning by hand meant hunting for empty canvas.
Inside a project a plain drag on an eye moves it, since eyes are small.
Rejected: moving a web by dragging its name (a small target nobody finds).

Update 2026-10-27, after review:
- Saves to a source run one at a time, and what piles up meanwhile goes as
  one batch with the latest spot per node. Two saves in flight could land
  swapped, and the server keeps the last one to land.
- The layout remembers what it last sent per node and skips any other
  saved value until that one comes back. Without that, the echo of an
  earlier nudge undid a later one, and a drag could save that older spot.
- Accepted gap: if the same person moves the same node on another device
  before this device's echo comes back, this device keeps its own spot
  until the next move or restart. Fixing it needs a version per node.
- Re-layout saves the new spots of nodes saved unpinned, and a web keeps
  room for its pinned eyes, so neither a restart nor a smaller web moves
  something it shouldn't.


## D21. Local work moves by file (2026-10-27)

`hyakume local export FILE` writes all of local work and the person's
positions as JSON with a format number; `hyakume local import FILE` puts
it in another computer's local.db. Ids travel, so links and positions
still match. Whoever the file names becomes this computer's "me", and
built-in link types map to this database's own.

An import replaces and never merges. Into empty local work it just goes
in; otherwise it needs `--replace`, which confirms first. It runs as one
transaction and checks every field like a normal save, so a bad file
changes nothing. The file is written 0600, since it's someone's own work,
also when it replaces an existing file.

Rejected: merging (two copies edited apart would need the conflict review
for every task, offline, with no server to say which is newer); a sync
folder pointed at local.db (SQLite in a folder synced by another tool can
corrupt if both computers run at once); and sending local work through a
team server (PROJECT.md forbids publishing personal work automatically).
