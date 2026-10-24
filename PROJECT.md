# Hyakume

Hyakume is a work tracker written in Rust with GPUI, the UI framework from the
Zed editor team. It combines kanban workflows with an open, connected canvas
inspired by Obsidian's graph view.

Linux is the first native target, supporting x86_64 and ARM64 from the first
packaged release. Teams and collaboration are part of the design from the
beginning, alongside personal use without an account.

Support local personal use and self-hosted team collaboration. An official
hosted collaboration service is out of scope: the project does not operate a
central server or account system for users.

## Development tooling

Pin the latest stable Rust version in `rust-toolchain.toml`, currently 1.99.1,
with rustfmt and Clippy. Update that pin deliberately as new stable releases
arrive rather than letting builds silently change compiler version.

Use `mise.toml` for short development commands. Mise reads the Rust version from
`rust-toolchain.toml`, without a duplicate version setting. Tasks cover `dev`,
`build`, `build-release`, `check`, `fmt`, `fmt-check`, `lint`, `test`, and `ci`;
run them with `mise run TASK`. Cargo commands remain usable directly through
rustup.

## Name and mascot

Hyakume means "hundred eyes" and takes its name from the Japanese yōkai. Its
mascot is **Meme**, a cute, soft, rounded interpretation of that figure: one
large expressive eye in the center and exactly 99 tiny eyes arranged around it
like freckles or a constellation. The silhouette should feel warm and friendly,
never grotesque, wet, or frightening.

Meme should inform the app's design, rather than appear only in its icon.
Rounded shapes, small eye-like nodes, and constellation-like connections can
carry that identity into everyday use.

## Canvas and workflow

The canvas is open, not bounded by Meme's body. Tasks look like small eyes,
with meaningful connections between them. Meme sits in the header beside the
workspace/project controls. Support light and dark palettes following the system
theme and use the system UI font. The mascot's 100 eyes do not limit task count.

The overview combines local personal projects and projects from multiple
self-hosted team servers in one canvas. Keep each project's workspace/server
origin clear. This is a combined client view, not a transfer of data between
servers; each source retains its own identity and permissions. Cross-project
relationships remain within a workspace.

Zooming into a project reveals its tasks and relationships. People should be
able to follow each project's work through its workflow stages.

Status must remain readable without relying only on color or eye expressions.
People can pan and zoom, select a project to focus it, and return through
breadcrumbs. Stage filters keep node positions stable. A separate kanban board
shows the same tasks and can follow the first canvas prototype.

Group projects into clusters, using a force-directed layout for task eyes within
each project. Allow dragging and pinning nodes. Re-layout is explicit and
preserves pinned nodes; arrangements remain personal. Single-clicking a task
selects it and opens a side inspector. Selecting a project offers focus/zoom
into that project.

## Keyboard navigation

Keyboard use is a first-class way to operate the native GUI, not a substitute
that requires switching to the CLI. Core workflows must work without a mouse:

- Switch workspaces and projects, focus task eyes, and follow connections.
- Pan and zoom, return to the overview, and arrange or pin nodes.
- Search and filter tasks, open details, edit fields, and change workflow stage.
- Create relationships and navigate the kanban board.
- Use dialogs and review conflicting edits.

Use standard desktop shortcuts and a command palette by default, with optional
Vim-like navigation outside text fields. Tab and Shift+Tab move among interface
regions and controls. Within the canvas, arrows select nearby eyes; separate
commands follow incoming and outgoing connections. Keep the focused eye visible.

Keep focus visible and predictable, support normal text editing, and avoid
trapping focus in the canvas. Provide discoverable commands and shortcut hints.
Start with conventional defaults:

- Ctrl+Shift+P opens the command palette.
- Ctrl+F searches within the active view.
- Enter opens or confirms; Escape closes or returns, according to context.
- Arrow keys navigate task eyes when the canvas has focus.

Define remaining bindings consistently during implementation. Shortcuts must
not interfere with typing in task fields.

## Tasks and project workflows

Each task has a title, description, status, assignee, labels, and comments.
Priority and due date are optional.

Workflows are customizable per project, starting with these default stages:

```text
Backlog → To do → In progress → Review → Done
```

Each stage has a "counts as finished" flag. Done starts enabled; the other
default stages start disabled. The flag, not the stage's name, determines
whether its tasks stop blocking dependent work.

Teams can define custom relationship types between tasks, including connections
across projects within the same workspace. Start with two built-in types:

- Blocks: directional, with the reverse shown as "blocked by".
- Related to: undirected.

An unfinished blocker gives the dependent task a blocked indicator, but does not
prevent status changes. Completing the blocker clears its blocking effect
without moving the dependent task. Other unfinished blockers still apply.
Custom types can be directional or undirected, with no automatic task behavior
initially.

## Personal and team work

A workspace contains projects. A person can have local personal work and belong
to several shared workspaces. Hyakume owns its tasks and planning; a repository
is never required.

Shared workspaces share tasks, relationships, assignments, and status. Canvas
layouts are personal: moving a node changes your view, not everyone else's.
Synchronize personal canvas layouts across that person's devices, rather than
keeping positions device-only. Shared-workspace layout data belongs to the user
on that workspace's server, not a central Hyakume account. Synchronizing layouts
must not automatically publish local personal work or copy another server's
data into a team workspace. Resolve the synchronization mechanism for local and
combined-overview layouts during implementation within those privacy boundaries.

The app should make it easy to find your own assignments and follow team work.

Workspace administrators can define custom roles and their permissions. Start
with four editable presets:

- Owner: controls workspace ownership and deletion.
- Admin: manages access, projects, and their settings.
- Member: creates and edits tasks and comments. Task deletion and relationship
  management require additional permission.
- Guest: starts read-only, limited to explicitly invited projects.

Permissions cover viewing, task changes, comments, project configuration,
invitations, and role management. Members' roles apply across workspace projects;
guests are limited to invited projects. Team groups organize people but do not
grant permissions. Permissions from multiple roles add together within membership
access boundaries; there are no explicit deny rules initially. Only the owner
can transfer ownership or delete the workspace. Custom roles cannot grant those
protected powers.

## Collaboration and storage

Personal work is stored locally. Each shared workspace has an authoritative
self-hosted server. A team operator runs `hyakume-server`; teammates connect to
it through invitations and do not each need to run a server. The operator
provides and pays for their own infrastructure, rather than relying on a service
funded by the Hyakume project.

Use SQLite for both local and server storage, with separate databases. Start
with a single-server deployment, keeping its database on that server's local
disk rather than a shared network filesystem. Clients access shared work
through the server, never by opening its database file.

Use WAL mode and short write transactions. Writes take turns per database;
reads can generally continue during a write. Keep network requests and user
interaction outside database transactions. Validate contention under the
expected workload rather than treating team size as a performance guarantee.

Use the Rust skill's backend stack: Axum, Tokio, and Tower for the server, SQLx
for SQLite, serde for data formats, and tracing for diagnostics. GPUI remains
the native UI framework. Check compatible versions and enable only needed
features during implementation; record resolved versions in `Cargo.lock`.

Organize a small Cargo workspace with shared core, storage, and client libraries,
plus separate desktop, CLI, and server packages. Keep GPUI dependencies out of
headless server and CLI builds.

Design the relational schema during implementation around workspaces,
memberships, roles, projects, stages, tasks, relationships, comments, identities,
and personal layouts. Use versioned SQL migrations. Upgrade the local desktop
database automatically and run server migrations at startup before accepting
requests. A failed migration must prevent the server from serving against an
incompatible schema.

Use versioned REST/JSON HTTP endpoints for commands and queries, and WebSocket
for server change notifications. GUI and CLI use the same server operations.
On reconnect, refresh active-project data and the workspace summary while
preserving drafts and personal layouts.

Distribute the self-hosted server as both a container image and a native Linux
binary with the same configuration model. Use TOML configuration with environment
overrides and explicit secret-file options. Keep secrets out of command-line
arguments and normal logs. SQLite data must remain on persistent local storage.

Provide secure remote access through an external HTTPS reverse proxy, including
WebSocket forwarding. Hyakume serves an internal HTTP port and does not manage
certificates. Plain HTTP is for deliberate local development or the internal
connection behind the configured HTTPS proxy.

Before a usable release, provide a consistent backup command producing one
portable archive with a format/version manifest, a database snapshot, and
separately stored uploads. Do not blindly copy live SQLite files. Provide a
documented and tested restore command. Operators handle scheduling and external
backup protection or encryption initially.

Shared changes appear live while connected. Offline, previously loaded shared
work is read-only; local personal work remains editable. Stale conflicting
edits must be rejected for review rather than silently overwriting another
person's changes. Atomically check a task's version when saving. Reject stale
updates to the task as a whole and preserve the person's draft. Show the latest
version and draft side-by-side, highlight changed fields, and let the person
revise and retry against the latest version. Stack the latest and draft panels
when the window is too narrow for side-by-side comparison. Do not automatically
merge independent fields initially. Clearly mark disconnected shared views as
offline and read-only.

## Accounts and joining

- Local personal use needs no account, email, or server connection. Open the app
  and start working; a name and profile picture are optional.
- Shared or remotely stored work needs an identity on the team's self-hosted
  server. Accounts, invitations, sessions, and permissions are managed there;
  no registration with Hyakume or central identity service is required.
- Invite links are the primary way to join. Someone opens a link, chooses a
  display name, and joins without mandatory email, a provider connection, or a
  separate signup form. Create a persistent server identity and device session.
- The inviter chooses the role and any guest project access. Invitations can
  expire, have usage limits, and be revoked. Default to seven days and one use,
  with configurable expiration and usage limits. Anyone holding an unrestricted
  invitation link may be able to accept it.
- Email delivery is optional. People can add and verify an email afterward to
  sign into the same identity on other devices through an email link or code.
  They can also connect a supported provider instead.
- Offer a set of single-use recovery codes for people who choose neither email
  nor a provider. Regenerating the set invalidates the old codes; using a code
  establishes a new session. Before completing shared-workspace onboarding,
  require saving recovery codes or verifying an email/provider. Email remains
  optional. Explain that losing every recovery or sign-in method can mean losing
  access. An invitation is not a reusable login credential.
- Remember signed-in devices securely, let people view and revoke sessions,
  and expire sessions after a configurable inactivity period, defaulting to
  90 days without authenticated use.
- Store remembered desktop and CLI credentials in the OS keyring first. If it
  is unavailable, explain the problem and require an explicit fallback choice;
  never silently write plaintext credentials. The fallback mechanism is still
  to be selected.
- A guest is a person with limited access, not an anonymous identity. Assignments
  and history remain attached to a stable identity when a display name changes.
- GitHub and Codeberg are optional identity connections. A connected profile can
  supply the initial username/display name and avatar. People without a connected
  provider start with a neutral placeholder. Everyone can customize their name
  and picture; provider updates should not overwrite those choices.
- Linking an email or provider requires verification and attaches it to the
  existing identity. Never silently merge identities based on matching email
  addresses, names, or profile details.

Joining a team must never upload local personal projects or expose them to
teammates. Moving personal work to a server must be an explicit action.

### Optional email delivery

Each server operator configures their own SMTP service and sender address and
covers any delivery costs. Hyakume does not provide a central mail service or
send team messages from `hyakume.org`.

Without SMTP, email verification and email sign-in are unavailable, but joining
through invitation links and recovering access with saved codes still work.
Admins can share invite links through chat or their existing email without
Hyakume sending a message. Email addresses and accounts belong to that server;
using the same email on another instance does not merge identities.

## Repository connections and CLI

GitHub and Codeberg connections can bring repository issues into the workspace
and link development activity to tasks. They supplement Hyakume's own planning
rather than define it. Profile sign-in, repository access, and workspace
membership are separate concerns.

Build GitHub and Codeberg support together in the first repository integration
milestone. Initially, integrations are read-only: import and refresh linked
issues without writing changes back to either provider. Keep repository-owned
fields distinct from Hyakume's own planning fields:

- Repository title, body, open/closed state, labels, and assignees are read-only.
- Also import remote comments and available linked pull-request context as
  read-only data. Distinguish remote discussion from Hyakume comments.
- Hyakume maintains its own workflow stage, assignment, priority, due date, and
  comments. Imported assignees do not automatically become workspace members.

Initially, imported authors and assignees are display-only provider identities,
with names, avatars, and profile links. Do not automatically associate them with
Hyakume identities; local assignment remains independent.

Workspace admins configure repository credentials/authorization on the server,
select repositories, and confirm that their imported content may be shared in
the workspace. Imported content follows Hyakume's workspace and guest access
rules; provider permissions are not automatically reproduced for each person.

The server refreshes connected repositories approximately every five minutes,
respecting provider limits and backing off after errors or throttling. People
can request a manual refresh and see the last successful refresh and any errors.
Provider-specific field mappings remain an implementation detail.

If a remote issue disappears or becomes inaccessible, keep the task, links,
local planning, and previously imported data. Mark its source unavailable and
retry later. Do not infer deletion from an access failure or silently delete
the task.

A simple CLI should let people and AI agents work with the same tasks as the
GUI. It should offer stable task IDs, structured output, and non-interactive
commands, while respecting workspace permissions.

Use resource-then-action commands, such as `hyakume task list`,
`hyakume task show ID`, `hyakume task move ID STAGE`, and
`hyakume relation create`. Support explicit workspace selection over a saved
default and JSON output.

Human-readable text is the default; `--json` enables stable structured output.
Send errors to stderr with meaningful exit codes. Destructive commands require
confirmation or an explicit non-interactive confirmation flag.

The installed `hyakume` command opens the native GUI when run without arguments
and dispatches CLI subcommands otherwise. Use `hyakume-server` for hosting.
Keep GUI and headless components separable internally despite the shared
launcher interface.

Local work needs no server credential. For shared work, sign the CLI in once
and reuse its saved session. An AI agent runs the same commands with that
identity's current permissions; no separate AI account or integration is
required. Shared changes are attributed to that identity, and commands must
not print credentials.

## First milestone

Start with a collaboration prototype: two people can update tasks across
multiple projects through a basic eye-node canvas and CLI. They share task
changes and relationships while retaining their own canvas layouts.

Use preconfigured, development-only test identities for this prototype, not as
production authentication. Real sign-in and invitations follow before a usable
release.

The prototype succeeds when:

- Two clients create, edit, move, and link tasks across two projects.
- The overview combines local projects and projects from multiple team servers.
- The GUI's core task and navigation workflows work without a mouse.
- GUI and CLI see committed shared changes.
- Canvas positions remain personal and shared-workspace layouts synchronize
  across devices using the same server identity.
- Conflicting task updates are rejected with drafts preserved for comparison.
- Disconnected shared work is read-only.
- A custom role and a custom relationship type work through the implementation.

The first usable release targets roughly 5–30 people, 20 projects, and 10,000
tasks per workspace. These are design and test targets, not promised performance
or permanent limits. Load detail on demand rather than drawing every task at
once. The initial prototype focuses on functional correctness; broader load
validation follows.

The usable release adds production sign-in, invitations and recovery, optional
SMTP and provider connections, the kanban board, profile customization,
repository integrations, backups, and packaging. These remain requirements,
but are not prerequisites for the development-only collaboration prototype.

## Website and installation

Use `hyakume.org` for a simple Astro landing page on Cloudflare Pages, separate
from the native app and Rust collaboration server. Its purpose is to present
the project, show screenshots, and explain how to install it, not host an account
dashboard or the application backend.

Support command-line installation through:

- General Linux: versioned prebuilt archives for x86_64 and ARM64, with checksums
  and documented download/install commands.
- Arch: maintain only the AUR package `hyakume-bin`, downloading and verifying
  release binaries rather than compiling Rust. Do not maintain source or `-git`
  AUR variants.
- NixOS: provide a source-based Nix package exposed through a flake, with explicit
  build and runtime dependencies. Reuse compatible cached outputs when available;
  otherwise Nix builds locally. Do not run a project-specific binary cache
  initially or depend on wrapping generic Linux release binaries for NixOS.

Keep release archives outside the Pages site. Use repository release hosting
with the provider selected before publishing releases.

## Upload storage

Use local files for personal uploads and store shared uploads in the self-hosted
server's local upload directory. Keep file references and metadata in SQLite.
Personal files must never upload automatically when someone joins a shared
workspace.

Read and write shared uploads through the team's server, applying workspace and
guest permissions. Include the server's upload directory in portable backups.

Cloudflare is used for the public website's DNS and Pages hosting, not application
data. The app and self-hosted server must not require a Cloudflare account or
service. No cloud object-storage backend is part of the current scope.

## Decisions to resolve while building

The combined overview and cross-device personal layouts are agreed requirements.
Resolve these remaining product details while building, before the usable release:

- How does an operator create the first owner and workspace on a new server,
  before anyone can send invitations?
- Should standalone personal work import GitHub/Codeberg issues directly, without
  a server? Shared-workspace imports are already server-managed.
- Which explicit credential-storage fallback is available without an OS keyring?

The development-only two-person prototype can start with test identities while
the release onboarding and integration details are resolved.

## Engineering details

Resolve these during implementation, following the requirements above:

- Compatible dependency versions, including GPUI and SQLite, exact workspace
  boundaries, tables, constraints, indexes, and migration contents.
- HTTP endpoint definitions, request/error formats, WebSocket event payloads,
  and the subscribe/refresh ordering needed to avoid missing committed changes.
- Layout synchronization for local and combined-overview records without a
  central service or automatic publication of personal or cross-server data.
- Remaining keyboard bindings and focus order, layout parameters, colors,
  spacing, and exact inspector/command-palette interactions.
- Provider-specific authorization, field mappings, and pull-request linkage.
- Device-session, identity-linking, and recovery-code implementation details.
- Final CLI flags and exit-code definitions, configuration keys, backup format
  details, release hosting provider, and packaging/dependency baselines on Linux
  x86_64 and ARM64, including the Arch and Nix packages.
