---
tags: [rust, project, cli, gui, gpui, astro, pnpm, cloudflare-pages, p2p, file-transfer, android, ios, security]
created: 2026-07-10
updated: 2026-10-13
status: planning
license: Apache-2.0
---

# Hyakume

> A friendly cross-device portal where every trusted device is one of Meme's
> eyes. Pair once, then pass files, text, URLs, and clipboard contents from eye
> to eye.

**The promise:** no account, no public profile, no hidden clipboard reading, and
no rendezvous or relay that receives the keys needed to decrypt payloads.

## Start Here

Hyakume is in planning. Begin with Phase 1. Do not create the public protocol,
hosted service, or production platform applications before the phase that owns
them. Desktop presentation and mobile support have separate gates; their
unfinished checks do not block the Linux CLI.

The first useful proof is two isolated Linux endpoints that introduce each
other, verify the same identity, transfer one generated file, reject one offer,
revoke trust, and leave no unsafe output. Linux desktop and Linux-to-Windows
work follow the CLI; Phase 8 runs the contract with Android. macOS and iOS
implementation, feasibility probes, CI, packaging, signing, and distribution
begin only after v1 publication and a recorded continuation decision. Their
unfinished work never blocks v1.

GPUI is the fixed desktop toolkit, and Iroh is the confirmed P2P transport.
Neither is a replacement candidate. Phase 1 selects the PAKE and identity design
for the core. Phase 3 pins the accepted Iroh version and configuration. Phase 5
pins and qualifies GPUI on Linux before production GUI work; Phase 7 qualifies
Windows presentation. Phase 16 qualifies macOS after v1.
Each feasibility decision states its evidence, remaining risk, and required work.
Open dependency choices also record rejected material alternatives and a `go`,
`replace`, `descope`, or `stop` verdict. GPUI and Iroh decisions record passing or
failing checks and select versions and integration approaches within those
dependencies. Spikes are disposable; only qualified code and dependency
configurations enter the production package.

Replace, descope, or stop if representative users cannot distinguish permanent
from temporary trust, the first device pair is not useful, or visual
verification is not understood. Missing graph, accessibility, platform, or
release capabilities become required GPUI integration or upstream work. The
GPUI gate passes only when its checks pass; it never selects another toolkit.
Missing transport capabilities require Iroh integration or upstream work; the
Iroh gate validates that work and never selects another P2P transport.
Linux, Windows, and Android are the v1 target. A failed feasibility
gate may replace an open dependency or amend the roadmap within these rules.

## End Goal

Hyakume v1 is the end of a long `0.x` journey. It is not an early prototype.
The list below is the v1 target. GPUI and Iroh remain fixed. Changing a required
platform or another committed implementation choice requires a recorded decision
and roadmap amendment.

The execution plan assumes one primary developer. Phases run sequentially, and
the roadmap does not assume parallel desktop, mobile, web, service, security, or
release teams.

### First supported release

The first supported release is a Linux CLI that works over an isolated LAN. Two
Linux devices permanently pair, verify each other, reconnect, transfer files and
text, approve or reject offers, revoke trust, and publish received files safely.
It requires no account, hosted service, GUI, project website, mobile
application, packaging channel, or finished brand. Phases 1 through 4 build and
qualify this release; completing an earlier internal phase does not require
publishing it.

The Linux desktop and Windows are the next targets after the Linux CLI contract
works. Android follows those desktop systems. Internet relay, the desktop GUI,
Android, package channels, and final artwork remain v1 work, but they do not
block learning whether the core product is useful.

V1 includes:

- a Rust-led application codebase licensed under Apache-2.0, with a native Kotlin
  application and the smallest required FFI, build, and packaging integration;
- a canonical `hyakume` command with CLI, desktop service, and a GPUI desktop
  application;
- full Linux and Windows desktop support;
- a native Android application using Kotlin and Jetpack Compose, with share
  target, QR, notifications, secure storage, resume, and qualified lifecycle
  behavior;
- permanent accountless device trust and explicit revocation;
- QR and short human-code pairing through a vetted password-authenticated
  exchange;
- stable visual key verification;
- LAN discovery and direct transfer without internet access;
- authenticated end-to-end encrypted internet transfer with relay fallback;
- files, bounded batches, text, URLs, standard input, and explicit clipboard
  content;
- visible offers, rejection, no-clobber output, atomic publication,
  cancellation, cleanup, integrity checking, and resumable files;
- temporary one-use recipients through installed CLI or native applications;
- a static Astro website in the separate `hyakume-front` repository, built with
  pnpm and hosted on Cloudflare Pages at `hyakume.org`, with a landing page,
  release notes, downloads, documentation, security, and privacy information;
- first-party rendezvous and relay services with self-hosting support;
- a local Constellation Graph plus an accessible list view;
- a professionally commissioned human-made mascot, eye system, brand, and icon
  package;
- English UI and documentation with qualified Unicode names and filenames;
- direct verified artifacts plus Scoop, AUR `hyakume-bin`, and a qualified
  Android distribution channel;
- versioned protocol, state, config, invitation, and CLI JSON contracts;
- checksums, SBOM, provenance, upgrade notes, privacy notes, and exact-artifact
  native QA.

Linux, Windows, and Android are required for v1. Development begins with
Linux-to-Linux, then the Linux desktop and Linux-to-Windows, then Android.
Each platform becomes supported only when its phase is complete.

### After v1

The platform scope decision recorded on 2026-10-11 moves macOS, iOS, and iPadOS
after v1 to focus development on Linux and Windows first and defer Apple
distribution costs. Android remains in v1, after the desktop work.

Post-v1 work includes a GPUI macOS application and a native iOS/iPadOS
application using Swift and SwiftUI, with a share extension, QR, notifications,
Keychain, resume, and qualified lifecycle behavior. Apple platform support,
Homebrew distribution, and any paid signing or store enrollment are separate
later commitments. They are not v1 compatibility, QA, or release requirements.
No release date is promised. Each added platform must pass its own native
integration, security, packaging, and release qualification against the shipped
protocol before support is claimed.

## Name and Mascot

Hyakume means "hundred eyes" and takes its name from the Japanese yōkai. Its
mascot is **Meme**, a cute, soft, rounded interpretation of that figure: one
large expressive eye in the center and exactly 99 tiny eyes arranged around it
like freckles or a constellation. The silhouette should feel warm and friendly,
never grotesque, wet, or frightening.

The command is `hyakume`. The project owner has already acquired `hyakume.org`.
The website will be hosted on Cloudflare Pages. The public contact email is
[vincent@sshmoi.com](mailto:vincent@sshmoi.com).

Domain acquisition does not establish trademark clearance or a working website
deployment. The brand and website phases verify material collisions, registrar
and DNS control, renewal, and recovery without storing credentials, recovery
material, billing details, or private registrar contact records in the
repository. The public contact address above is intended for publication.

Meme passes sealed parcels, text slips, and links from one eye to another. A
permanent eye sits in the constellation, while a temporary eye has a dotted
orbit and disappears after use or expiry. Meme always waits for a visible offer
and approval before opening a path. The character is curious, careful, and
reassuring rather than all-seeing or intrusive.

The full mascot design keeps the central eye and all 99 smaller eyes. Device
eyes in the interface borrow this visual language but represent the user's
actual devices; they do not create a 99-device requirement or limit. Tiny icons
may use an approved central-eye mark when all 100 eyes cannot remain legible.

Trust, transfer path, progress, expiry, rejection, and errors must never depend
on Meme's pose, animation, or color. They also need explicit text and accessible
symbols. Artwork must not suggest background clipboard reading, decrypted relay
access, public profiles, or trust based only on a device name. The final design
is commissioned from a professional human artist in the brand phase.

## Product Boundaries

Hyakume is not:

- a cloud drive or permanent remote inbox;
- a chat or social system;
- an account, organization, username, or contact directory;
- an account-recovery or server-side device-key backup service;
- an automatic clipboard synchronizer;
- a remote shell, port forwarder, or device manager;
- a plugin host or third-party runtime integration platform;
- a directory synchronizer or archive extractor;
- a substitute for backups.

macOS and iOS/iPadOS are the planned native additions after v1. No other native
platform is planned. Human-reviewed translations may also arrive after the
English v1. The first candidates are Japanese, Korean, Simplified Chinese, and
Traditional Chinese.

### User benefit and continuation

The first cohort is Linux users who already move content between two owned Linux
devices; record their current workaround and one recurring transfer problem.
Linux-to-Windows is the next benefit hypothesis, with Linux-to-Android following
the desktop work. Neither is proved by the Linux protocol demo. At Phase 3,
declare a pairing/verifier confusion threshold and a success measure before
trials. At Phase 4, observe repeated real workflow use and record whether the CLI
earns desktop GUI and Windows work. Phase 7 records whether the desktop product
earns Android work. A technically correct demo alone is not evidence that people
prefer this utility.

Test explicit receiver approval on already trusted personal devices. Simplify
the wording and actions when confused; do not silently introduce auto-accept or
weaken the trust contract. Continuation decisions include developer/support time,
native device/build access, signing/channel costs, dependency/upstream work, and
relay bandwidth. A fixed dependency is a constraint to budget, not a guarantee
that required platform integration is already available.

## Product Shape

### Permanent eyes

Every installation owns a local identity and a private list of trusted devices.
Hyakume models devices, not people.

```sh
hyakume pair
hyakume pair MOSS-IRIS-LANTERN

hyakume send phone photo.png
hyakume send workstation notes.txt diagram.svg
hyakume send tablet --text "meet at 18:00"
hyakume send phone https://example.com
hyakume send laptop --clipboard
printf '%s\n' "remember the milk" | hyakume send phone -
```

A local alias is private. A declared device name is presentation. Neither grants
trust.

### Temporary eyes

```sh
hyakume share presentation.pdf
```

The sender receives a QR code, native invitation link, and short code for an
installed Hyakume client. The sender remains online and no service stores
payload bytes. A recipient claims the invitation. Both endpoints show a verifier
bound to that claim's authenticated session. The sender compares it with the
intended recipient in person or over an independently trusted channel before
approving the claimant. A declared name or possession of the invitation alone
does not identify the intended recipient. Approval atomically consumes the
invitation and rejects every other claimant. The sender then offers the payload
metadata, which the recipient must approve before anything is written.

An approved session may resume an interrupted transfer until it completes,
expires, or is cancelled, but its invitation cannot admit another recipient.
The temporary trust then disappears.

A successful temporary transfer does not create a permanent trusted device.
Permanent pairing requires a separate explicit action.

### Desktop experience

The GPUI desktop application always includes the complete Constellation Graph
of locally known trusted devices and active relationships. The graph is a
primary product view, not an optional visualization or a list replacement. The
same service projection also drives a complete accessible list.

The desktop experience lets the user:

- drag files onto an eye;
- choose an eye and send text, a URL, or explicit clipboard content;
- see whether a connection is local, direct through the internet, or relayed;
- review incoming names, types, counts, and total size before writing;
- follow progress, failure, cancellation, retry, and resume;
- use the complete workflow with a keyboard and list view.

Closing the GUI does not cancel accepted work owned by the desktop service.

## Rules That Do Not Move

### Codes introduce, keys authenticate

- QR and native invitation links carry long random single-use secrets under the
  native invitation format below.
- Human codes are random, short-lived, single-use, and rate-limited.
- Human codes use a maintained balanced (symmetric) PAKE such as SPAKE2 or
  CPace, because both devices hold the same short code. Asymmetric PAKEs such as
  OPAQUE target a client and a server that stores a password file, so they do
  not fit pairing. Hyakume invents no cryptographic primitive.
- Pairing exchanges long-term public identities through the authenticated
  session.
- Both devices show the same short visual verifier and require explicit
  confirmation that it matches before trust is saved.
- Reconnection uses saved keys, never a device name or old code.
- Removing a device revokes it locally. Trust returns only through fresh pairing.
- Revocation invalidates existing session authorization, rejects queued offers,
  cancels active and paused transfers, and invalidates their resume authority.
  Cancelling work cannot recall bytes already sent to the peer.
- Loss, corruption, or reset of a local identity creates a new device identity.
  V1 has no server-side recovery; affected peers must pair again.
- Suspected key compromise also requires removing the old identity from each
  peer. Accountless v1 has no global remote-revocation authority.

The exact PAKE, transport identity, session construction, KDF, and content
encryption are selected in Phase 1 after platform and dependency review.

Native invitations use the `hyakume` URI scheme, with a versioned invitation
token in the fragment, for example `hyakume://invite#<token>`. The token carries
the secret and bounded routing data; its exact encoding is selected with the
invitation protocol. It is never an HTTP or HTTPS invitation URL. QR codes
encode the same native invitation. Emit a complete invitation only through the
explicit pairing or sharing action, never through diagnostics or logs.

When the caller can detect absent or failed URI handling, report unavailable.
Successful dispatch does not prove that Hyakume received the invitation. Never
translate the invitation into a web URL, send it to the website, or redirect it
through a browser fallback. Offer a separate secret-free installation link.
Native clients validate the scheme, version, routing fields, length, and expiry
before using the invitation. Another registered handler may receive the entire
token, including its secret. Provide QR scanning and invitation entry inside
Hyakume so users can avoid OS dispatch. Dispatch never authenticates the handler
or peer; permanent pairing and temporary-claim verification remain required.
If interception is suspected, cancel the invitation and create a fresh one.

### Direct first, relay honest

- LAN operation works without DNS, `hyakume.org`, or internet access.
- Internet operation prefers authenticated direct connections.
- A relay forwards encrypted traffic only.
- A relay streams temporary transfers only while both endpoints are online. It
  does not store payload bytes.
- The published endpoint protocol never gives rendezvous or relay services the
  payload keys.
- Services may still correlate endpoint identifiers and observe IP addresses,
  timing, approximate sizes, and availability. The UI and privacy notes say so.
- A malicious service can drop, delay, or record encrypted traffic. Endpoint
  encryption protects payload content; it cannot enforce a compromised
  operator's retention policy or guarantee availability.
- Hosted services are replaceable through self-hosting.

### Files are data

- Never execute or open a received file automatically.
- Treat display names and output paths as separate values.
- Reject traversal, reserved names, symlinks, special files, and unsafe races.
- Do not overwrite existing files by default.
- Keep partial files private until authentication and digest verification pass.
- Publish only into a receive location qualified for atomic no-clobber
  publication. Refuse unsupported direct destinations before acceptance, or
  offer qualified app-owned storage with separate export as described below.
- A failed, rejected, cancelled, or incomplete transfer never appears complete.
- V1 sends files and batches, not directory trees or extracted archives.

### Peer text is an output boundary

- Treat device names, offered filenames, text previews, URLs, and peer errors
  as untrusted presentation input, including before pairing or offer acceptance.
- Preserve original bounded values separately from safe display projections.
  Validating an output filename does not make its original display name safe.
- CLI output visibly escapes terminal controls, unexpected line breaks in
  single-line fields, and bidirectional formatting controls. Only application
  code emits terminal commands or prompt structure. Never interpret peer text
  as markup, a hyperlink command, or an approval instruction.
- GUI consent screens, mobile views, notifications, and logs use bounded plain
  text projections with explicit field boundaries and visible escaping of
  deceptive controls. Payload previews cannot impersonate application prompts.
- Apply byte, line, and rendered-width limits before formatting. Mark shortened
  previews explicitly without changing the underlying payload or trust identity.
- Redaction precedes safe log formatting. A safe projection is not permission
  to log payloads, clipboard data, invitation secrets, or keys.

### Clipboard and consent stay explicit

- Read the clipboard only after a focused user action.
- V1 clipboard input is bounded UTF-8 plain text only. Rich text, images, file
  lists, and background synchronization are unsupported; explain that before
  reading/sending rather than silently converting unsupported clipboard formats.
- Send it only to the chosen eye.
- Require approval for incoming content unless a future narrow rule is created.
- V1 has no broad auto-accept rule.
- Do not log payloads, clipboard contents, complete invitations, or private keys.

### Local knowledge stays local

The Constellation Graph is built only from the current device's trust and
session state. Peers never exchange address books or inferred social graphs.

### Updates follow the install channel

Hyakume has no silent self-updater in v1. Direct-download users may check for an
update. Scoop, AUR, and Android users update through the channel that installed
the product. Post-v1 Homebrew and iOS distribution follow the same rule. Package
manifests point to immutable versioned artifacts and checksums.

## Trust and Transfer Model

### Permanent pairing

```text
Unknown
  -> one-use invitation
Introduced
  -> authenticated exchange and human verification
Trusted
  -> disconnect
Trusted offline
  -> remove or revoke
Revoked
  -> fresh invitation and verification
Trusted
```

A permanent invitation is consumed when the inviting device's person accepts
the verifier, before that device sends its decision. Trust is saved only after
both authenticated decisions are yes. A verified exchange that ends without
acceptance closes the invitation, and an interrupted unverified claim uses an
attempt. A temporary invitation is consumed atomically when the sender
approves one claimant; all other claimants are rejected. Expired, consumed,
rejected, or raced invitations fail closed, and a restart never reopens one.

Revocation and final file publication share one per-peer authorization ordering
owned by the service or mobile host. If revocation wins, an unpublished item
cannot become `Published`. If atomic publication already committed, preserve
that file and cancel the remaining work. Aggregate batch outcomes still derive
from item outcomes. A revocation acknowledgement follows durable trust-state
invalidation and enforcement of the publication barrier; task shutdown and
partial-file cleanup follow the deadline and failure policies below. Restart
must retain the revocation and must not restore old session or resume authority.
Fresh pairing permits new offers but never silently revives cancelled transfers.
Phase 2 qualifies the durable ordering and crash reconciliation with the chosen
store and filesystem.

### Transfer states

```text
Offered -> Rejected
Offered -> Accepted -> Transferring -> Verifying -> Published
                    -> Paused -> Resuming -> Transferring
                    -> Cancelled
                    -> Failed
```

Resume trusts authenticated chunk state, not the mere presence of partial
bytes. `Published` means the verified file has committed under its final name in
a receive location qualified for atomic no-clobber publication. Completion
requires this commit and its recoverable local record; export is separate.
For a file batch, each item has its own transfer and publication state. A batch
remains nonterminal while any item is active, queued, or resumably paused. Once
all items are terminal, it is `Completed` only when every item is published,
`PartiallyCompleted` when at least one item is published and at least one ended
unsuccessfully, and otherwise reports the actual failure, rejection, cancellation,
or expiry outcome. Hyakume does not imply portable all-or-nothing batch
publication.

When a requested destination cannot provide that contract, explain the limit
before acceptance and let the recipient choose qualified app-owned receive
storage or reject the offer. Never silently substitute a destination. Android
document-provider access does not by itself qualify atomic publication. The
same rule applies to post-v1 iOS provider integration.

Export copies an already published file to a user-selected provider or other
destination. It has separate `NotRequested`, `Exporting`, `Exported`, `Failed`,
and `Cancelled` states and never changes the transfer's publication outcome.
Require explicit user action, bounded work, and no overwrite of an existing
document; refuse export when the provider cannot guarantee no-clobber creation.
An export failure retains the verified local file and reports
`Received; export failed`. Report any partially created external document and
cleanup failure.
Do not claim that provider export is atomic or that a failed export completed.

### Payloads

The protocol supports:

- one file;
- a bounded file batch;
- UTF-8 text;
- a URL treated as text and never opened automatically;
- standard input;
- clipboard content provided by an explicit action.

Each offer carries bounded metadata, type, count, total size, and a stable
transfer identifier. Unknown payload kinds are rejected or negotiated by the
versioned protocol.

V1 has no unknown-length offer. Spool standard input into private sender-owned
storage before creating an offer. Apply per-input and aggregate spool byte
limits, an input deadline, cancellation, and disk-space checks while reading.
At EOF, seal the spool as immutable content and record its exact length and
digest. Empty input is a valid zero-byte payload. A limit, read error, timeout,
or cancellation before EOF produces no offer and cleans up the spool.

Accepted stdin transfers resume only from the retained sealed spool, including
after sender restart. Retention is bounded by the transfer's expiry and storage
quota. Completion, terminal failure, rejection, cancellation, revocation, or
expiry immediately invalidates resume authority and schedules spool deletion;
there is no intentional payload retention after a terminal outcome. Startup
recovery preserves authenticated, unexpired resumable state and schedules
deletion of invalid, expired, or orphaned spools and receive partials.

Cleanup starts immediately and uses bounded retry counts and backoff within the
declared cleanup retry window. Bytes remain charged to their storage quota until
removal succeeds. If deletion still fails when the window ends, report the
retained private data and stop automatic retries until the next startup or
explicit retry. Never claim deletion or silently release quota for bytes that
remain on disk.

A missing or corrupt spool makes resume unavailable; never reread the current
stdin or substitute new bytes under the old transfer ID. Other sources with an
unknown length use the same preparation contract or are refused before offer.

### Local key storage

The device key and the per-pair secrets never reach a plain file, a log, or
command output. Each device chooses one store at `hyakume init` and keeps it.
Hyakume never switches stores silently, and never because of how the user
connected, so a device's identity lives in exactly one place.

| Platform | Stores |
| --- | --- |
| Linux | Secret Service, or a passphrase-encrypted key file |
| Windows | Credential Manager / DPAPI |
| Android | Android Keystore |
| iOS (post-v1) | Keychain |

On Linux, `hyakume init` offers the Secret Service when it is reachable and
unlocked. Otherwise it offers the passphrase-encrypted key file: over SSH, on
headless machines, and where the keyring is locked or missing. If the user
declines both, init refuses and explains how to set up either one.

- The key file uses a maintained format and crates, such as `age` with a
  passphrase, or Argon2id with XChaCha20-Poly1305. It adds no new primitive.
- Hyakume reads the passphrase from the terminal without echo. Scripts may pass
  `--passphrase-fd`, with a warning that a stored passphrase gives up the
  protection against a stolen disk.
- `hyakume unlock` decrypts the keys into the desktop service, which keeps
  them in memory until `hyakume lock`, service stop, or reboot. Running the
  service as a systemd user service with lingering keeps it unlocked after the
  SSH session ends.
- While the store is locked, the device answers no discovery queries, peers see
  it as offline, and queued work waits. `hyakume status` reports the store and
  whether it is locked.
- Every Secret Service call has a short timeout. A keyring that would need a
  graphical unlock prompt reads as locked; it never hangs an SSH session.
- A forgotten passphrase or a lost store follows the lost-identity rule: local
  reset and fresh pairing.

This protects keys from other local users and from a stolen disk, backup, or
copied home directory. It does not protect them from malware running as the
same user while the store is unlocked; no supported store does.

Hyakume does not use plain permission-only key files, keys derived from an SSH
agent, or automatic fallback between stores. TPM sealing or `systemd-creds`
are considered only if a recorded need appears for unattended restart without
anyone unlocking the device.

## Domain Model

The product relationships are decided. Phase 1 chooses the cryptographic
representation beneath them, and Phase 3 the transport configuration. Later
phases decide persistence and wire fields when real consumers exist.

```text
LocalIdentity
  -> TrustRecord[]
      -> RemoteDeviceIdentity
          -> optional LocalAlias

Invitation
  -> IntroducedPeer
      |-> permanent verification -> TrustRecord(Trusted)
      `-> TemporarySession -> one transfer -> Forgotten

LocalIdentity + RemoteDeviceIdentity
  -> AuthenticatedSession
      -> Offer
          -> Transfer
              |-> PayloadManifest
              |-> ItemTransfer[]
              |   |-> ChunkState
              |   `-> optional PublishedPayload
              `-> TransferOutcome

DesktopService
  |-> owns LocalState
  |-> owns AuthenticatedSession[]
  |-> owns TransferTask[]
  `-> projects local trust and session state -> ConstellationView
```

A temporary session has no transition to permanent trust. It must end as
`Forgotten`. A separate verified invitation creates a `TrustRecord`.

### Ownership decisions

- Domain values use stable IDs and owned data across async tasks. They do not
  borrow from network frames, database rows, or UI state.
- Private key material is never a plain field copied through domain structs.
  `LocalIdentity` holds an opaque key handle or protected secret wrapper chosen
  in Phase 1.
- Secure storage protects persisted transport secrets; it does not automatically
  make an Iroh signing key hardware-non-exportable. Qualify the selected key
  loading/signing API, required in-process secret lifetime, and OS storage path.
  Make no non-exportability claim unless that exact integration proves it.
- Trust and transfer progress use enums with validated transitions, not sets of
  independent booleans.
- A batch outcome is derived from item states. It is never an independently
  mutable flag that can disagree with published files.
- The desktop service owns runtime handles, sockets, databases, and tasks.
  Domain entities contain no `JoinHandle`, connection object, or GUI handle.
- Separate domain, protocol, persistence, IPC, and presenter types when their
  validation, security, or compatibility requirements differ. Reuse a type when
  an extra representation would only add mapping code.
- The Constellation Graph is a read-only projection. It never becomes an
  authority for trust or peer relationships.

### Principal types and decision points

Names may change during implementation. The boundaries and transition rules are
the stable part.

| Type or type group | Responsibility | Decide in | Why then |
| --- | --- | --- | --- |
| `DeviceId`, `LocalIdentity`, `RemoteDeviceIdentity`, `KeyHandle` | Represent long-term endpoint identity without spreading private key bytes | Phase 1 | Identity derivation depends on the accepted transport, secure store, and rotation design |
| `DeviceName`, `LocalAlias` | Keep peer-declared presentation separate from private local naming | Phase 3 | LAN discovery and pairing provide the first real naming and spoofing cases |
| `TrustState`, `TrustRecord` | Record trusted or revoked identity, verification, and local policy | Phase 1, persisted in Phase 2 | The state machine must exist before storage shape is chosen |
| `InvitationId`, `InvitationSecret`, `InvitationKind`, `InvitationState` | Model permanent and temporary introductions, expiry, claims, use, and failure | Phase 1, extended in Phase 3 and Phase 10 | Loopback defines the core; human codes and native temporary sessions add boundary-specific fields later |
| `ProtocolVersion`, `CapabilitySet`, protocol DTOs | Negotiate compatible behavior and reject unsupported messages | Phase 1, frozen in Phase 13 | Vectors and a second real consumer must shape the wire contract before v1 freeze |
| `SessionBinding`, `SessionProof`, `SessionSlot`; `SessionId` and a session route when something outside the node must name a session or its path | Bind an authenticated peer to negotiated capabilities and a local, internet, or relayed path | Phase 1, extended in Phase 4 and Phase 10 | Route and migration fields are added only when those transports work |
| `OfferId`, `Offer`, `OfferState`, `PayloadDescriptor` | Describe bounded proposed content and the receiver's consent decision | Phase 1 and Phase 2 | The safe file pipeline defines which metadata is required before acceptance |
| `TransferId`, `TransferState`, `ItemTransferState`, `TransferOutcome`, `PayloadManifest`, `ChunkState` | Own item and batch progress, integrity, cancellation, verification, partial completion, and resume | Phase 1, completed in Phase 4 | Small single-item and batch loopback transfers start the model; interruption and restart prove resume invariants |
| `SafeOutputName`, `Destination`, `PublishedPayload` | Separate hostile display names from validated local publication | Phase 2 | Native filesystem tests must determine the platform-specific rules |
| `SafeDisplayText`, `DisplayName`, `ConsentView` | Project hostile peer values without terminal commands, markup, or forged prompt structure | Phase 1 and Phase 2 | Pre-trust and offer output must be safe before the first CLI receive flow |
| `PreparedPayload`, `SpoolRecord` | Own sealed sender bytes, exact offer size, retention, and resume availability | Phase 2 | Standard input must finish bounded preparation before an offer exists |
| `ExportState`, `ExportDestination` | Track explicit copying of published content separately from receipt | Phase 8, extended after v1 in Phase 17 | Native providers determine export capabilities and partial-output handling |
| Persistence records and schema versions | Store identity, trust, invitations, migrations, and resumable work | Phase 2 | Storage is an adapter; its rows must follow proven domain states, not define them |
| `IpcRequest`, `IpcResponse`, `IpcErrorCode` | Connect CLI and GUI to the one desktop service owner | Phase 2 | The first real service boundary determines authentication and error needs |
| `DiscoveryRecord` | Advertise bounded reachability hints without granting trust or exposing a globally stable identity or raw hostname | Phase 3 | LAN behavior and spoofing tests define the minimum safe record |
| `ConstellationNode`, `ConstellationEdge`, `DesktopViewState` | Project local domain truth for the desktop GUI and accessible list | Phase 5 | Presenter types should follow the working GUI, not enter the core early |
| Rendezvous and relay DTOs, `ServicePolicy` | Bound mailbox, rate, retention, route, and relay behavior | Phase 10 | Hosted APIs are not guessed before direct desktop and Android transfer work |
| Android bridge DTOs | Validate intents, content URIs, permissions, and lifecycle events | Phase 8 and Phase 9 | Native Android behavior determines the smallest safe bridge |
| iOS bridge DTOs | Validate share-extension input, security-scoped URLs, Keychain access, notifications, and lifecycle events | Post-v1 Phases 17 and 18 | Native iOS behavior determines the smallest safe bridge |

Do not create public repository, transport, or key-store traits up front.
Start with concrete accepted implementations. Extract a small consumer-owned
trait when a second implementation or a focused test boundary proves its use.

## Threat Model

This is a design model, not a claim that unimplemented software is safe.

### Important assets

- device keys, invitation secrets, and trust decisions;
- payloads and filenames;
- user approval;
- partial and completed transfer state;
- local aliases, history, config, and logs;
- hosted-service and release credentials;
- the promise that rendezvous and relay do not receive payload keys or store
  payload bytes.

### Main attackers and failures

- unknown or malicious LAN peers;
- remote code guessers and racers;
- malicious senders and recipients;
- peer-controlled text that forges consent UI or injects terminal commands;
- a compromised rendezvous or relay;
- a compromised website or download deployment;
- another local OS user or an accidental same-user IPC client;
- compromised dependencies, CI, signing, or package channels;
- crashes, lost networks, full disks, clock changes, sleep, and corrupt state.

Hyakume does not claim to resist arbitrary malicious code already running with
the current OS user's authority. IPC permissions, peer checks, and request
authorization prevent cross-user access and accidental calls. They become a
stronger boundary only where an OS sandbox supplies one.

### Required controls

| Boundary | Required control |
| --- | --- |
| Invitation | strong random secret, PAKE for short codes, expiry, atomic consumption, rate limits, verifier |
| Peer session | mutual authentication, replay protection, version negotiation, bounded frames, revocation of active and resumable work |
| Peer presentation | bounded safe text projections, escaped controls, explicit consent fields, redacted logs |
| Files | safe names, no-follow writes, spool quotas, private partials, qualified atomic no-clobber publish, separate provider export |
| Local IPC | per-user permissions, peer checks where available, request authorization |
| Local state | OS secret store or passphrase-encrypted key file chosen at init, no silent fallback, migrations, corruption handling |
| Rendezvous and relay | no payload storage, minimal metadata retention, redacted logs, quotas, endpoint encryption |
| Website | strict CSP, no third-party assets, immutable identified builds, deployment provenance, verified download links |
| Android bridge | narrow Rust FFI boundary with a typed Kotlin wrapper, intent allowlist, lifecycle tests |
| iOS bridge (post-v1) | narrow Rust FFI boundary with a typed Swift wrapper, share-extension validation, Keychain isolation, lifecycle tests |
| Release | locked graph, immutable actions, scoped credentials, checksums, provenance, native QA |

All attacker-influenced inputs, queues, concurrency, retries, and retained state
have enforced limits chosen and tested with the feature that needs them.
Reaching a limit returns a typed refusal, never silent truncation or false
completion.

## Required Invariants

- An invitation is consumed exactly once. A permanent invitation is consumed
  when the inviting device's person accepts the verifier, and trust needs both
  decisions; a temporary invitation is consumed when the sender approves one
  claimant, and every other claimant is rejected. Each invitation allows one
  comparison of verifier words.
- Trust exists only after both devices confirm the same visual verifier. A
  declared device name, local alias, or discovery record never creates trust.
- Reconnection authenticates with saved keys. No path re-derives trust from a
  name, address, code, or previous session.
- Revocation and final publication share one per-peer authorization order. Once
  revocation wins, no item becomes `Published`, no queued offer is accepted, and
  no earlier resume authority is honored, including after restart.
- An item already committed by atomic publication stays published. Revocation
  cancels remaining work and never revives cancelled work.
- A batch outcome is derived from item outcomes. `Completed` requires every item
  published, and the aggregate outcome never disagrees with the published files.
- No received file appears under its final name before authentication, complete
  transfer, and digest verification pass.
- Publication happens only into a destination qualified for atomic no-clobber
  creation. Hyakume refuses an unqualified destination before acceptance and
  never silently substitutes another one.
- Export copies an already published file. It never changes the transfer's
  publication outcome and never overwrites an existing document.
- A sealed spool is immutable. Its exact length and digest are fixed at EOF,
  resume reads only from that spool, and no path rereads standard input under an
  existing transfer identifier.
- Every offer carries an exact byte length. V1 has no unknown-length offer.
- Original peer-supplied bytes and their safe display projection are separate
  values. Validating an output filename never makes its display name safe.
- No peer-controlled text reaches a terminal, consent screen, notification, or
  log without a bounded projection with visibly escaped controls.
- The endpoint protocol never gives rendezvous or relay services payload keys.
  Hyakume-operated services do not persist payload traffic; a compromised
  service's recording and availability risks follow the relay fault model.
- The Constellation Graph is a projection of local trust and session state. It
  is never an authority for trust or peer relationships.
- Private key material stays behind an opaque handle or protected wrapper. It is
  never copied as plain bytes through domain structs, IPC, or presenter types.
- Payloads, clipboard contents, complete invitations, and private keys never
  enter logs, metrics, crash reports, or diagnostics.
- Loss, corruption, or reset of local identity produces a new device identity.
  It never restores old trust or old resume authority.
- Every attacker-influenced input, queue, retry, concurrency level, and retained
  value has an enforced limit. Reaching a limit returns a typed refusal, never
  silent truncation or false completion.
- Every long-lived task has one owner, a cancellation path, and a retained
  handle until its result is observed or the process ends. Graceful shutdown
  follows the Shutdown contract; deadline expiry is a failed drain, never
  evidence of cancellation. Domain values hold no `JoinHandle`, socket,
  database handle, or GUI handle.
- Expected failure uses typed errors. Malformed peer input, I/O failure, full
  disks, permission denial, and timeouts never reach `panic!`, `unwrap()`, or
  `expect()` in core, protocol, service, or transfer code.

## Fault Model

These outcomes apply [Transfer states](#transfer-states),
[Payloads](#payloads), and [Shutdown](#shutdown). Fault rows and phase checklists
must not introduce different lifecycle rules.

| Fault | Required result |
| --- | --- |
| A new claim uses an expired or consumed invitation, or loses the approval race | Refuse that claim with a typed outcome; it creates no trust or authorized session |
| Verifier mismatch or user declines verification | No `TrustRecord` is written and the invitation cannot be reused |
| Native invitation URI has no handler or dispatch reports failure | Report unavailable when detectable; offer in-app entry and a separate installation link, with no web fallback |
| Native invitation URI reaches the wrong app | The handler may learn the secret; dispatch proves no identity, independent claimant verification remains required, and suspected interception requires a fresh invitation |
| Peer offers an unsupported protocol version | Negotiated down or rejected; never partial semantics |
| Peer text carries terminal controls or a forged prompt | Bounded escaped projection; terminal state and consent UI unchanged |
| Offer exceeds a declared limit | Typed refusal before acceptance; nothing is written |
| Receiver rejects the offer | No partial file, no directory creation, no state beyond the recorded outcome |
| Network drops mid-transfer | Item becomes `Paused`; partial bytes stay private; resume needs authenticated chunk state |
| Sender restarts mid-transfer | Preserve a valid sealed spool and manifest for authorized resume; the receiver publishes only verified complete items |
| Receiver restarts mid-transfer | Preserve valid authorized resumable partials; remove invalid, expired, or orphaned ones under the cleanup policy; no false `Published` |
| Digest verification fails | Item fails, cleanup follows the Payloads policy, and nothing is published |
| Destination cannot provide atomic no-clobber publication | Typed refusal before acceptance, or an offer of qualified app-owned storage |
| A file already exists at the output name | No overwrite; the item ends unsuccessfully with a typed outcome |
| Disk full or quota exceeded during receive or spooling | Bounded refusal, cleanup, and a typed error; no partial publication |
| Revocation during transfer | Publication barrier engages; already committed items remain published |
| Revocation during publication commit | The declared ordering decides the outcome, and it survives restart |
| One item of a batch fails | Retain published items and derive progress or terminal outcome from all item states under Transfer states |
| Export fails or the provider cannot guarantee no-clobber creation | `Received; export failed`; the published local file is kept and partial external output is reported |
| Local identity missing or corrupt | Fail closed with an explicit local reset path that requires fresh pairing |
| Key store locked, unreachable, or absent | Report `locked` and wait for `hyakume unlock`; an unresponsive Secret Service reads as locked after a short timeout; init refuses when no store is set up; key material is never stored unprotected |
| Rendezvous or relay unavailable | LAN and direct paths continue; typed unavailable result for internet operation |
| Relay is compromised or observes traffic | Payloads stay encrypted; the relay can observe metadata, record ciphertext, or deny service; replayed or modified traffic cannot create authorization or false completion |
| Another local OS user opens the IPC socket | Rejected by per-user permissions and peer checks before any request runs |
| Clock moves backward | Expiry decisions stay conservative; no invitation or session lifetime increases |
| GUI closes during accepted work | The desktop service keeps the transfer; the GUI reattaches to the same state |
| Mobile host suspends or the process is killed | Lifecycle recovery reports the real state; no false completion in a notification |
| Graceful shutdown begins | Refuse new offers, pause or resolve accepted work, and drain owned tasks under the shutdown policy; partials stay private |
| Shutdown drain deadline expires or blocking I/O stalls | Report failed graceful shutdown and use the host termination policy; do not claim tasks joined, free resources still in use, or invent completion |

## Initial Resource Budget

Phase 1 records the first estimates and Phase 4 measures real memory before
these laptop values are accepted or revised. A mobile host may lower any value;
neither configuration nor a peer may raise a compiled maximum.

| Resource | Initial limit |
| --- | ---: |
| Trusted devices per installation | 256 |
| Concurrent authenticated sessions | 32 |
| Concurrent active transfers per session | 4 |
| Concurrent active transfers per installation | 16 |
| Nonterminal transfers per installation, including offered and paused work | 64 |
| Items per batch | 256 |
| Active items per transfer | 1 |
| Batch total size | 16 GiB |
| Single file size | 16 GiB |
| Text payload | 1 MiB |
| URL payload | 8 KiB |
| Clipboard payload | 1 MiB |
| Standard-input spool per transfer | 2 GiB |
| Aggregate spool storage | 8 GiB |
| Aggregate private receive partials, including paused work and pending cleanup | 32 GiB |
| Cleanup retry window for terminal or orphaned temporary data | 24 h |
| Offer metadata bytes | 64 KiB |
| Retained peer display name | 1 KiB |
| Rendered name width | 120 columns |
| Text preview shown before consent | 4 KiB |
| Protocol frame | 1 MiB |
| Transfer chunk | 1 MiB |
| In-flight chunks per item | 8 |
| Application payload buffers per installation | 128 MiB |
| Claimants per temporary invitation | 8 |
| Human-code attempts per invitation | 5 |
| Human-code lifetime | 120 s |
| QR and native invitation lifetime | 15 min |
| Pairing attempts per peer per minute | 10 |
| Retained discovery records | 512 |
| Discovery announcements per minute | 12 |
| Session handshake deadline | 30 s |
| Idle session timeout | 10 min |
| Resume window | 7 days |
| IPC request bytes | 1 MiB |
| IPC clients per service | 8 |
| Constellation nodes rendered | 512 |
| Log line bytes | 4 KiB |
| Retained log | 64 MiB |
| Desktop service resident memory | 256 MiB |
| Graceful shutdown drain deadline | 20 s |

Process one item at a time per transfer. Sixteen active items with eight 1 MiB
chunks each require at most 128 MiB for one resident copy of each chunk. Enforce
the application buffer budget across sending, receiving, verification, and any
additional copies; acquire capacity before allocation and apply backpressure.
The other 128 MiB of the desktop service budget must cover transport buffers,
retransmissions, frame decoding, metadata, task stacks, storage caches, and the
runtime. Phase 1 estimates and Phase 4 measurements must account for those costs
and lower concurrency or buffer windows when necessary.

Paused work releases active transfer, active-item, and buffer capacity but
retains its nonterminal transfer slot and disk reservation. Reserve temporary
storage before acceptance and count pending cleanup against the same quota.
Quota exhaustion refuses new work without evicting authorized resumable content
silently. Mobile hosts declare lower limits where needed and measure the full
host process, including its UI and native bridge, on the smallest supported device.

## Rust Shape

Start with one package:

```text
hyakume/
|-- Cargo.toml
|-- src/
|   |-- lib.rs          # domain and application modules
|   `-- main.rs         # first CLI and process owner
```

The first implementation has one real consumer. Keep module boundaries concrete
without freezing cross-crate APIs.

Split code when the next phase creates a real boundary:

- extract `hyakume-core` and `hyakume-protocol` when a second application or
  service needs the shared domain and versioned wire contract;
- extract application, network, storage, platform, and presenter crates when
  they have distinct ownership or native dependencies;
- add `hyakume-server` when hosted operation begins;
- add `hyakume-mobile-ffi` with the Android application in Phase 8, after its
  first native consumer defines the boundary;
- build the static Astro website in the separate `hyakume-front` repository in
  Phase 12, using pnpm and Cloudflare Pages;
- add the native Swift application after v1 in Phase 17 and reuse the mobile Rust
  boundary only where its Android-proven contract also fits iOS;
- add `xtask` only when native packaging work is clearer in Rust than duplicated
  shell scripts.

Do not create placeholder crates, `Manager`, `Service`, `Repository`, or
platform traits. A second real implementation earns an abstraction.

### Ownership

- One desktop service owns identity, state, networking, invitations, and active
  transfers for the current OS user.
- CLI and the GPUI application call it through authenticated local IPC. GPUI
  owns presentation state, windows, input, and rendering. It does not own trust,
  networking, or transfer state.
- The native Kotlin application, and the post-v1 Swift application, use the
  shared Rust core through a narrow FFI boundary. Each mobile host owns its
  platform lifecycle, permissions, secure-store adapter, notifications, and file
  handles. Neither pretends to have an unrestricted desktop daemon.
- The separate `hyakume-front` website owns the landing page, release notes,
  documentation, and verified download links. Astro generates static HTML,
  pnpm manages the website dependencies, and Cloudflare Pages hosts the output.
  The website owns no transfer, retention, or peer-trust policy.
- Every long-lived Tokio task follows the task ownership and shutdown contracts
  below. Blocking filesystem, database, and key-store jobs have bounded
  admission and queues; their concurrency limit does not bound I/O duration.
- Core and protocol code return typed values and errors. Presenters own text.
- Project-owned core and protocol code forbid `unsafe`. Native FFI keeps any
  required `unsafe` small, documented, and natively tested.

### Rust error handling and invariants

- Validate external input and operational failure with typed errors. Use
  checked arithmetic and fallible conversions for byte totals, offsets, chunk
  counts, and reservations. Use fixed-width protocol fields and `usize` for
  in-memory indexing after validated conversion.
- Use release-active assertions for internal programmer-error invariants whose
  violation requires stopping the process. Required validation, state changes,
  cleanup, and memory safety must never depend on `debug_assert!` or assertion
  side effects.
- Supervisors observe worker results, including Tokio task panics. An invariant
  panic is fatal to the owning process; do not continue transfers using possibly
  inconsistent state. Recover through the normal startup path. No panic may
  unwind across FFI.
- Assert relationships between compiled limits, and test that admission,
  cancellation, pause, resume, and cleanup preserve aggregate resource budgets.

### Shutdown

Close admission to new offers and background jobs, notify owned tasks, and
resolve or pause accepted work within the drain deadline in the resource budget.
Every task handle stays owned until its result is observed or the process ends.
Started blocking I/O may outlive cancellation; dropping a handle or timing out
does not stop it.

If the drain deadline expires, report failed graceful shutdown. The desktop
service requests unsuccessful process termination and reconciles durable state
on restart. A mobile host follows its platform termination policy and must not
free Rust or native resources still reachable by running jobs. It cannot reuse
that core instance until all jobs have stopped. Each host qualifies this overrun
path in its own phase. The deadline bounds graceful waiting, not OS termination
or completion of a stalled system call. Never report unjoined tasks as joined
or interrupted work as complete.

### Planned stack

| Need | Planned choice |
| --- | --- |
| Language | Rust 1.99.0 initially, edition 2024, one package until a real workspace boundary exists |
| Async | Tokio with required features only |
| P2P transport | Iroh, confirmed; Phase 3 pins a supported version and validates its configuration and integration |
| Human-code pairing | maintained PAKE selected in Phase 1 |
| Content consistency | BLAKE3 digest bound to authenticated transfer metadata; the accepted session or AEAD design supplies security integrity |
| CLI | clap |
| Desktop GUI | GPUI for Linux and Windows in v1; macOS follows after v1 |
| Desktop GUI gate | Phase 5 validates Linux graph/list/accessibility; Phase 7 qualifies Windows; post-v1 Phase 16 qualifies macOS |
| Android UI | native Kotlin with Jetpack Compose, introduced in Phase 8 |
| iOS UI (post-v1) | native Swift with SwiftUI, introduced in Phase 17 |
| Mobile core bridge | narrow Rust FFI contract selected and pinned in Phase 8; validated commands and bounded immutable events cross the boundary |
| Website | Static Astro in `hyakume-front`, pnpm, Cloudflare Pages at the acquired `hyakume.org` domain; introduced in Phase 12 |
| Hosted APIs | Axum + Tower where the Iroh integration needs them |
| Persistence | SQLite through rusqlite with bundled SQLite, chosen in Phase 2 ([decision record](docs/local-service.md)) |
| Key storage | Linux Secret Service or passphrase-encrypted key file in Phase 2; Windows Credential Manager / DPAPI in Phase 7; Android Keystore in Phase 8 |
| Config and DTOs | serde, JSON, TOML |
| Errors | thiserror in libraries, anyhow at executable boundaries |
| Logging | tracing with bounded redacted sinks |
| Release | native GitHub Actions; add packaging tools only when a qualified artifact needs them |

Monitoring starts with local redacted diagnostics. When hosted rendezvous or
relay services need monitoring, use self-hosted Uptime Kuma on independent
infrastructure and self-hosted Prometheus/Grafana when metrics need dashboards.
Add self-hosted trace or log storage only for a demonstrated need. Paid managed
monitoring is a later option when capacity, availability, or maintenance costs
justify migration. If centralized error tracking is introduced, use Sentry Cloud
directly, never self-host Sentry; keep the existing consent and secret/payload
exclusion rules. None of these services is required for offline LAN operation.

Add dependencies when used. Commit `Cargo.lock`. Pin Rust 1.99.0 initially in
`rust-toolchain.toml`, review later stable and patch upgrades intentionally, and
use `--locked` for CI and release Cargo commands that resolve the graph. Do not
promise an MSRV during early `0.x` development.

`rust-toolchain.toml` is the Rust source of truth. It records the channel,
profile, and required components such as `rustfmt` and Clippy. Pin external
mobile, packaging, and release tools in the phase that first requires them.

`mise.toml` reads that toolchain file and defines local development tasks.
The Linux CI runs `mise run ci`: formatting, Clippy, tests, and a build, in that
order. The separate `check` task is available locally; Clippy covers compilation
in CI. Graph-resolving tasks use `--locked`. CI pins mise and action commits,
uses read-only permissions, and cancels superseded runs. Add caching when real
dependencies justify it.

## Roadmap

### How to use this roadmap

- The phase checkbox is the progress board.
- Check a task only when its behavior and focused tests are complete.
- Check a phase only when every item under **Done when** is true.
- Add platform CI, crates, applications, documents, and packaging in the phase
  that first needs them.
- Plan for one primary developer. Do not open several platform phases in
  parallel to make the schedule appear shorter.
- Linux, Windows, and Android remain required for v1, in that order. macOS and
  iOS/iPadOS begin after v1 publication and their own continuation decision.
  Support is earned one platform at a time.
- Phases 6, 7, 9, 11, 16, and 18 end with a written continuation decision before
  the next large platform commitment. The decision may continue or hold for
  redesign. The Phase 1 design review may replace the selected PAKE. GPUI and
  Iroh remain fixed; a missing mandatory capability requires integration or
  upstream work within the affected dependency and a passing recheck. Removing a
  required v1 platform requires a recorded decision and roadmap amendment.
- Record feasibility decisions using the format and fixed-dependency rules in
  Start Here.
- Tag internal milestones only when useful. Completing a phase does not require
  publishing a release.
- Track progress only in `PROJECT.md`. The README explains the project simply
  and does not track progress.

### Progress Board

#### V1

- [x] **Phase 1** - Deterministic core and loopback protocol
- [x] **Phase 2** - CLI, local service, IPC, and safe file pipeline
- [ ] **Phase 3** - LAN discovery and permanent pairing
- [ ] **Phase 4** - Reliable LAN transfer and clipboard
- [ ] **Phase 5** - Linux desktop GUI alpha
- [ ] **Phase 6** - Linux desktop beta
- [ ] **Phase 7** - Windows desktop beta
- [ ] **Phase 8** - Android alpha
- [ ] **Phase 9** - Android lifecycle beta
- [ ] **Phase 10** - Internet rendezvous and relay alpha
- [ ] **Phase 11** - Self-hosting and reliable online operation
- [ ] **Phase 12** - Project website and verified downloads
- [ ] **Phase 13** - Compatibility freeze and product polish
- [ ] **Phase 14** - Security and release qualification
- [ ] **Phase 15** - Release candidates and v1 publication

#### After v1

- [ ] **Phase 16** - macOS desktop beta
- [ ] **Phase 17** - iOS alpha
- [ ] **Phase 18** - iOS lifecycle beta and release qualification

### Phase 1 - Deterministic core and loopback protocol

#### Build now

- [x] Model device identity, trust, invitation, offer, per-item and batch
  transfer outcomes, cancellation, and revocation as explicit Rust states.
- [x] Model revocation across active sessions, queued offers, paused work, and
  resume authority, including its ordering against final publication.
- [x] Define bounded safe peer-text projections and consent fields. Test terminal
  escapes, forged multiline prompts, bidi controls, and oversized previews.
- [x] Define bounded versioned control DTOs and error codes.
- [x] Evaluate maintained balanced PAKE choices (SPAKE2, CPace), identity
  design, session construction, attempt limiting, maintenance, and licenses.
  Record how the authenticated exchange binds long-term identity and visual
  verification, including the verifier that identifies an individual temporary
  claim.
  Record the exact Iroh version and API evidence the identity design relies on,
  so the design is not proved against an assumed transport capability.
- [x] Implement the accepted pairing and session primitives through maintained
  libraries.
- [x] Transfer text and a small generated file between two loopback endpoints.
- [x] Publish non-secret protocol and cryptographic test vectors.
- [x] Add unit and property tests for legal transitions, batch aggregation,
  replay, expiry, limits, version negotiation, key separation, and binding of
  content digests to authenticated transfer metadata.
- [x] Exercise aggregate admission and buffer limits, checked arithmetic at
  boundary values, and the fatal worker-invariant policy.
- [x] Add narrow fuzz targets for invitation and frame decoding.
- [x] Obtain an external design review of identity, pairing, key separation,
  invitation consumption, protocol states, and the published test vectors.
- [x] Record the accepted core choices.

#### Done when

- [x] Two isolated endpoints pair, authenticate, transfer, verify, and revoke on
  loopback.
- [x] Duplicate, reordered, replayed, oversized, truncated, and unknown-version
  messages fail with defined outcomes.
- [x] No item-state combination can produce a false `Completed` batch outcome.
- [x] Revocation prevents later authorization and publication; items committed
  before revocation remain published and cannot revive cancelled work.
- [x] Hostile peer text cannot alter terminal state or impersonate approval UI.
- [x] No test sleeps to wait for state or shares mutable fixtures. State
  machines read time only from an injected clock; tests about network
  deadlines let real time pass, with wide margins.
- [x] No custom cryptographic primitive is proposed.
- [x] Every core choice has a verdict. No unaccepted design-review finding or
  core release blocker enters persistence or LAN pairing work.

### Phase 2 - CLI, local service, IPC, and safe file pipeline

#### Build now

- [x] Add one per-user desktop service and authenticated local IPC.
- [x] Add CLI commands for service status, receive, send, cancel, and inspect.
- [x] Implement safe names, private partial files, disk reservation, no-clobber
  publication, integrity verification, cleanup, and bounded streaming.
- [x] Qualify atomic publication and crash reconciliation for supported receive
  locations. Refuse unsupported destinations before acceptance.
- [x] Spool stdin and other accepted unknown-length sources under byte, time,
  and disk limits before offering exact-size immutable content. Persist bounded
  spool retention for resume and recover or clean it after restart. Apply the
  same accounting to receive partials, paused work, and failed cleanup.
- [x] Apply safe display projections at every CLI, consent, and log output path.
- [x] Choose SQLite or atomic files from the actual migration, corruption,
  concurrency, and recovery requirements of this phase.
- [x] Persist identity, trust, and transfer state with migrations. Invitations
  stay in memory: a token names a transport key that is new every run, so no
  saved invitation could be reached after a restart. Persist them when
  discovery lets a token outlive its endpoint.
- [x] Fail closed on missing or corrupt identity material and provide an explicit
  local reset path that requires fresh pairing.
- [x] Store the device key and pair secrets in the Secret Service or a
  passphrase-encrypted key file, chosen once at `hyakume init` under Local key
  storage. Refuse when neither is set up.
- [x] Add `hyakume unlock` and `hyakume lock`, and report the store and its
  locked state in `hyakume status`. Read passphrases from the terminal without
  echo, and bound every Secret Service call with a timeout.
- [x] Own service, IPC, filesystem, and transfer tasks under the Shutdown contract.
- [x] Qualify the shutdown overrun path with stalled blocking work and restart
  recovery. Verify that deadline expiry never reports successful cancellation
  or releases resources still in use.
- [x] Add real filesystem, persistence, IPC, and exact-binary tests with
  disposable state.

#### Done when

- [x] Two local service instances exchange file, text, URL, stdin, and a file
  batch through the public CLI.
- [x] Rejection, cancellation, full disk, permission denial, existing files,
  mid-batch failure, crashes, and restart leave no false completion or unsafe
  output. Published items and the aggregate batch outcome always agree.
- [x] Revocation during transfer and final publication obeys the declared
  ordering across restart and blocks old resume authority.
- [x] Empty, bounded, oversized, interrupted, and timed-out stdin input has the
  declared offer and cleanup outcome. Sealed spools retain their exact identity
  across restart and are released under the retention policy.
- [x] Unsupported publication destinations receive a typed refusal, and hostile
  peer text cannot inject terminal controls or forge a consent prompt.
- [x] Over SSH and on a headless machine, a device initializes with a
  passphrase-encrypted key file, unlocks after a restart, and transfers. A
  copied state directory reveals no key material without the passphrase.
- [x] A locked, unresponsive, or absent key store gives a typed refusal or a
  `locked` status, never a hang and never an unprotected key on disk.
- [x] Packaged CLI artifacts pass a clean-host smoke test.

### Phase 3 - LAN discovery and permanent pairing

#### Build now

- [ ] Add bounded LAN discovery without exposing raw OS hostnames or a globally
  stable device identity. Use rotating or pair-specific discovery identifiers.
- [ ] Add QR and human-code pairing with expiry, one-use enforcement, attempt
  limits, and visual verification.
- [ ] Validate native invitation URIs and QR input. Test absent URI handlers
  and browser handling without an HTTP fallback or website disclosure. Test
  in-app entry and a competing handler that receives the token; dispatch must
  never count as peer verification. Use the native invitation fault model for
  unavailable, intercepted, and cancelled invitations.
- [ ] Save declared names and private aliases separately.
- [ ] Add reconnect, disconnect, remove, revoke, and fresh re-pair behavior.
- [ ] Pin the supported Iroh version and configuration. Record metadata
  exposure and how production builds avoid unintended public relay or discovery
  infrastructure. Do not accept standard mDNS publication of a durable endpoint
  ID as private discovery.
- [ ] Test spoofed discovery, duplicate names, invitation guessing/racing,
  replay, ambient identity correlation, stale trust, and revoked peers on an
  isolated LAN. Capture ambient discovery traffic and prove that it exposes
  neither raw hostnames nor a stable device identity.
- [ ] Record the first Linux cohort, recurring problem, current workaround,
  pairing/verifier success and confusion thresholds before the trial.
- [ ] Run a trial with representative users. Verify permanent trust, visual
  verification, receive consent, revocation, and failure states. Test explicit
  approval on personal trusted devices. Record confusion and stop or redesign
  conditions rather than explaining around them.

#### Done when

- [ ] Two desktop devices pair and reconnect on a LAN without hosted services.
- [ ] Names never grant trust and consumed invitations cannot be reused.
- [ ] An untrusted LAN observer cannot derive a stable Hyakume device identity
  from ambient discovery, including through the actual transport-secret
  loading and storage contract.
- [ ] Representative users complete the trust and transfer flow, or the roadmap
  records `replace`, `descope`, or `stop` and the required change.
- [ ] Removal blocks future authentication, invalidates active authorization,
  and cancels outstanding work under the publication ordering. Fresh pairing
  does not revive old offers or resumable transfers.

### Phase 4 - Reliable LAN transfer and clipboard

#### Build now

- [ ] Add resumable chunk transfer with authenticated state and final digest
  verification.
- [ ] Support large files, bounded batches, text, URL, stdin, and explicit
  clipboard sending.
- [ ] Make direct route, progress, pause, retry, failure, and cleanup visible.
- [ ] Handle sender mutation, disconnect, restart, duplicate chunks, corruption,
  route changes, and storage failures.
- [ ] Test clipboard access only through an explicit focused action.
- [ ] Run focused production-equivalent measurements with synthetic small,
  large, many-file, interrupted, and resumed transfers. Record throughput, time
  to visible progress, cancel and resume latency, peak memory, disk use, errors,
  payload hashes, commit, toolchain, build profile, target, features, machine,
  network conditions, sample count, independent runs, and run-to-run variation.
  Validate authentication and received bytes before accepting a timing.

#### Done when

- [ ] A large generated file survives interruption and both endpoint restarts,
  then publishes once with the correct digest.
- [ ] Interrupted stdin transfers resume from the sealed sender spool after
  restart. Missing or corrupt spools refuse resume without substituting bytes.
- [ ] Revocation while paused or resuming cancels the work and prevents old
  resume authority from surviving restart or fresh pairing.
- [ ] No incomplete, rejected, or corrupted payload appears complete.
- [ ] LAN transfer works with internet and `hyakume.org` unavailable.
- [ ] The first supported Linux CLI release satisfies the contract declared at
  the start of this plan.
- [ ] Observe recurring transfer use and record a continuation decision for
  Linux desktop and Linux-to-Windows benefit, maintenance/support capacity, and
  the next platform cost.
- [ ] The Phase 4 measurements expose no architecture-blocking resource or
  latency failure. A failure stops the roadmap for redesign before desktop GUI work.

### Phase 5 - Linux desktop GUI alpha

#### Build now

- [ ] Before production GUI work, spike GPUI on Linux Wayland/X11 with the actual
  graph/list projection, focus, scaling, drag/drop, shutdown, keyboard operation,
  and Orca accessibility. Pin accepted versions, budget required integration or
  upstream work, and pass the gate within GPUI before continuing.
- [ ] Add the GPUI desktop presenter over the existing local service through
  authenticated typed IPC.
- [ ] Implement the Constellation Graph, list alternative, pairing, incoming
  approval, transfer progress, errors, settings, and saved-eye management.
- [ ] Keep graph nodes, edges, focus, selection, and progress in GPUI presenter
  state projected from the service. UI actions emit commands and never mutate
  trust or transfer state directly.
- [ ] Add keyboard focus, high contrast, non-color cues, reduced motion, scaling,
  and resize behavior from the first GUI release.
- [ ] Render only locally known device and session relationships.
- [ ] Use disposable geometric placeholders until approved assets arrive.
- [ ] Add deterministic presenter tests and a small set of reviewed visual
  states.

#### Done when

- [ ] Pair, send, approve, reject, cancel, resume, remove, and inspect work from
  the GUI without changing core behavior.
- [ ] The graph and list expose the same local truth.
- [ ] Hostile names and previews cannot impersonate consent controls in either
  view. Native invitation dispatch never falls back to a secret-bearing web URL.
- [ ] The app remains usable without color, motion, or a mouse.

### Phase 6 - Linux desktop beta

#### Build now

- [ ] Complete Linux service ownership, Unix IPC, secure storage, file dialogs,
  clipboard, notifications, drag/drop, startup, and shutdown.
- [ ] Qualify current Wayland and X11 environments on the declared Linux support
  matrix.
- [ ] Build immutable Linux artifacts for supported architectures.
- [ ] Add AUR `hyakume-bin` metadata only after those artifacts pass.
- [ ] Run native install, pair, transfer, resume, update, and uninstall QA.

#### Done when

- [ ] Linux is a complete supported sender and receiver.
- [ ] Known compositor, filesystem, key-store, and notification limits are
  documented.
- [ ] No known or unaccepted release-blocking Linux defect remains.
- [ ] A written continuation decision approves Windows implementation
  or holds the roadmap for redesign.

### Phase 7 - Windows desktop beta

#### Build now

- [ ] Qualify GPUI graph/list rendering, input, scaling, drag/drop, shutdown,
  keyboard access, and Narrator on Windows before claiming support. Record
  dependency/upstream work and its continuation cost.
- [ ] Implement and test Windows named-pipe, credential-store, filesystem,
  notification, drag/drop, startup, and packaging behavior.
- [ ] Add Windows CI when its implementation begins and run the shared protocol
  and native integration checks there.
- [ ] Run the complete desktop journeys as same-OS and representative cross-OS
  pairs, including Linux-to-Windows before Android implementation begins.
- [ ] Publish a Scoop manifest only after immutable Windows artifacts pass.
- [ ] Qualify Unicode, reserved names, permissions, sleep, restart, update, and
  uninstall natively.

#### Done when

- [ ] Linux and Windows both act as complete senders and receivers.
- [ ] The same protocol vectors and product rules pass on both desktop systems.
- [ ] Direct downloads, Scoop, and AUR resolve to qualified immutable artifacts.
- [ ] Observe recurring cross-desktop use and record a continuation decision for
  Android benefit, maintenance/support capacity, and native device/build access.

### Phase 8 - Android alpha

#### Build now

- [ ] Create a native Android application in Kotlin with Jetpack Compose. Keep
  the UI and platform lifecycle in Kotlin while reusing the shared Rust core and
  protocol.
- [ ] Select and pin the narrow Rust FFI mechanism after proving release
  packaging, typed errors, cancellation, shutdown, and bounded immutable event
  delivery on representative real devices. Validate lengths, identifiers, and
  state before foreign input reaches the core. Do not expose the Rust object
  graph or per-chunk callbacks across the boundary.
- [ ] Add secure key storage, QR scanning, Files/content-URI access, share target,
  notifications, and explicit receive approval.
- [ ] Receive into qualified atomic storage and add explicit provider export
  with separate status, no-clobber creation, retained local content on failure,
  and visible partial-export cleanup outcomes. Apply safe peer-text projections
  to consent screens, notifications, and errors.
- [ ] Keep generated or manual JNI and Kotlin bridge code small, typed, and
  reviewed. Validate content URIs and permissions before they enter Rust.
- [ ] Exercise received-file discovery, provider export, local deletion, and
  uninstall with users; distinguish app-owned received data from exported copies.
- [ ] Support foreground pairing, send, receive, cancel, and resume.
- [ ] Add Android protocol vectors, integration tests where practical, and real
  device QA.

#### Done when

- [ ] Android pairs and transfers with Linux and Windows over LAN.
- [ ] Share-target and content-URI flows preserve permissions and cleanup.
- [ ] A provider without atomic publication cannot weaken the receive contract;
  failed export leaves a verified local file and an explicit export failure.
- [ ] Unsupported background behavior is stated plainly.

### Phase 9 - Android lifecycle beta

#### Build now

- [ ] Handle backgrounding, process death, reboot, doze, network change, storage
  pressure, permission revocation, notification denial, and app update.
- [ ] Add the accepted foreground-service or app-must-be-open contract.
- [ ] Qualify the shutdown overrun policy without freeing resources still used
  by Rust or native jobs.
- [ ] Resume authenticated transfers without trusting stale partial state.
- [ ] Qualify current supported Android API levels and representative vendors.
- [ ] Run accessibility, Unicode, battery, idle, upgrade, rollback, uninstall,
  and reinstall journeys.
- [ ] Measure foreground and accepted background transfer battery use, steady
  and peak memory, startup, time to visible progress, cancel and resume latency,
  idle work, and recovery after process death on representative real devices.

#### Done when

- [ ] Android has an honest, repeatable lifecycle contract.
- [ ] No lifecycle path publishes unverified content or loses trust state
  silently.
- [ ] Linux, Windows, and Android remain protocol-compatible.
- [ ] Android measurements satisfy the declared lifecycle and resource contract,
  and a written continuation decision approves internet rendezvous and relay
  work or holds the roadmap for redesign.

### Phase 10 - Internet rendezvous and relay alpha

#### Build now

- [ ] Add first-party rendezvous, invitation mailbox, discovery, and encrypted
  relay services with bounded APIs and state.
- [ ] Keep endpoint encryption and identity independent from service trust.
- [ ] Add direct connection attempts, relay fallback, route reporting, and safe
  migration between routes.
- [ ] Add native temporary transfers. The sender remains online, approves one
  claimant after independent verifier comparison, consumes the invitation
  atomically, and offers bounded metadata for recipient approval. Test an
  unintended claimant with the same invitation secret and a spoofed name. The
  approved session may resume, but it never creates permanent trust.
- [ ] Stream relayed payloads without storing them.
- [ ] Add quotas, rate limits, minimal retention, redacted logs, abuse controls,
  timeouts, and service cleanup.
- [ ] Set a declared operating budget and service limits before public hosting.
  Record how anonymous bandwidth exhaustion is bounded and when the service
  refuses new work.
- [ ] Assign relay maintenance, abuse/support ownership, signing/channel access,
  and a bandwidth-cost ceiling with refusal behavior before operating the service.
- [ ] Test malicious service responses, downgrade, substitution, replay, outage,
  packet loss, latency, and unavailable DNS in isolated staging.
- [ ] Measure direct and relayed prototypes with the same authenticated payloads.
  Record route selection, throughput, time to visible progress, memory, errors,
  retries, and queue growth across a controlled concurrency sweep without
  turning the results into public claims.
- [ ] Publish an initial privacy and metadata statement that names which endpoint
  identifiers, IP addresses, timing, approximate sizes, and availability the
  service can correlate and for how long.

#### Done when

- [ ] Trusted desktop and Android devices transfer directly when possible and
  through encrypted relay when needed.
- [ ] One approved native temporary recipient completes or resumes one transfer;
  competing claimants are rejected and the session is forgotten afterward.
- [ ] The service fails a known-answer payload decryption check by construction.
- [ ] The relay retains no payload bytes after a transfer ends.
- [ ] The UI reports the route without claiming anonymity.
- [ ] Relay measurements reveal no architecture-blocking collapse or unbounded
  queue growth at the declared prototype load.
- [ ] The declared budget, quotas, and refusal behavior make public operation an
  accepted risk.

### Phase 11 - Self-hosting and reliable online operation

#### Build now

- [ ] Package the rendezvous and relay services with clear configuration and
  secret handling.
- [ ] Support a custom service URL without changing client trust rules.
- [ ] Add migrations, backup, restore, rotation, health, graceful shutdown,
  rollback, quotas, and operational logs.
- [ ] Run controlled staging soak, fault, recovery, and capacity checks with
  synthetic traffic. Sweep a declared concurrency or arrival rate and record
  offered and completed work, p50/p95/p99 when sample counts support them,
  errors, timeouts, retries, dropped work, queue depth, load-generator health,
  and the first saturation point.
- [ ] Write concise self-hosting, upgrade, privacy, abuse, and recovery guides.

#### Done when

- [ ] A clean self-hosted deployment supports pairing and relayed transfer.
- [ ] Backup, restore, upgrade, and rollback have been executed.
- [ ] Resource ceilings and failure behavior are known for the declared service
  size.
- [ ] A written continuation decision accepts the service and self-hosting
  burden and either approves website and v1 release work or holds the roadmap for
  redesign.

### Phase 12 - Project website and verified downloads

#### Build now

- [ ] Verify control, renewal, recovery, and registrar security for
  `hyakume.org` without storing secrets or personal records in the repository.
- [x] Select static Astro in the separate `hyakume-front` repository, pnpm, and
  Cloudflare Pages. The site publishes project information and downloads;
  it has no application server or transfer endpoint.
- [ ] Pin Astro, Node.js, and pnpm versions when website implementation starts.
  Record pnpm in `package.json`, commit `pnpm-lock.yaml`, and use
  `pnpm install --frozen-lockfile` for CI dependency installation.
- [ ] Build the landing page, release notes, download, documentation, security,
  privacy, self-hosting, and error pages with Astro's static output. Publish
  `vincent@sshmoi.com` as the public contact email.
- [ ] Escape untrusted values with Astro's template handling and prohibit raw
  markup except for reviewed constants. Validate URLs used in links and browser
  input used by any local interaction.
- [ ] Configure Cloudflare Pages to build `hyakume-front` with `pnpm build` and
  publish `dist`. Connect the acquired `hyakume.org` domain and verify DNS, HTTPS,
  preview deployments, production deployment, and rollback.
- [ ] Add browser JavaScript only for named interactions with useful failure
  behavior. The static site requires no runtime handlers or server-sent events.
- [ ] Self-host and pin browser assets. Keep ordinary links and readable HTML as
  the baseline when JavaScript is unavailable.
- [ ] Publish an immutable website build identity and show checksums and
  provenance for downloadable artifacts.
- [ ] Keep invitation secrets and payloads out of website requests, HTTP and
  HTTPS URLs, referrers, logs, analytics, and third-party requests. Native
  `hyakume` invitation URIs follow the separate secret-bearing format; the site
  never parses them, redirects them, or receives their token. Installation links
  are separate and secret-free, including when the native app is absent.
- [ ] Add strict CSP, safe response headers, and no third-party assets.
- [ ] Test Chrome, Firefox, and Edge on the declared Linux, Windows, and Android
  support matrix. Safari qualification belongs to post-v1 Apple platform work.
- [ ] Test XSS, malicious links and redirects, modified or stale deployment,
  cache behavior, download integrity, service outage, and incompatible browsers
  in owned environments.
- [ ] Publish verified desktop downloads and manual OS selection.

#### Done when

- [ ] The website is not a transfer endpoint and never receives invitation
  secrets or payloads.
- [ ] Public pages make no unapproved third-party request.
- [ ] Pages remain readable and navigable without JavaScript. Every declared
  browser interaction has a clear fallback when unavailable.
- [ ] The static build deploys to Cloudflare Pages, `hyakume.org` resolves over
  HTTPS, and the public contact link uses `vincent@sshmoi.com`.
- [ ] Every public download resolves to a qualified artifact and checksum.
- [ ] The declared browsers can read the project, documentation, security,
  privacy, self-hosting, download, and error pages.

### Phase 13 - Compatibility freeze and product polish

#### Build now

- [ ] Freeze v1 protocol, invitation, config, state, service API, and CLI JSON
  contracts.
- [ ] Define supported old/new version windows, migrations, downgrade refusal,
  and extension rules.
- [ ] Recheck material product, package, repository, and trademark collisions,
  and record whether specialist legal review is needed before public branding.
- [ ] Commission and finish the human-made Meme, eye system, brand, and icons,
  including their source, rights, licenses, manifests, platform exports, and
  reduced-motion guidance. Keep the detailed art brief and rights agreement
  outside this engineering roadmap.
- [ ] Complete Linux/Windows GUI, website, Android, docs, error text,
  accessibility, and cross-platform polish.
- [ ] Remove dead features, rejected dependencies, unused abstractions, and
  redundant tests.
- [ ] Freeze the supported Linux, Windows, browser, Android, architecture, and
  filesystem matrices. Apple platforms are outside the v1 support matrix.

#### Done when

- [ ] Every public v1 contract is versioned and exercised by the clients that
  use it.
- [ ] All supported surfaces show the same consent, route, trust, and failure
  truth.
- [ ] No placeholder or unlicensed public asset remains.

### Phase 14 - Security and release qualification

#### Build now

- [ ] Freeze source, dependencies, tools, service config, and candidate scope.
- [ ] Complete a threat-model-driven review of invitations, PAKE, identities,
  protocol, relay, website and downloads, Linux/Windows IPC, files, clipboard,
  the Android bridge, storage, service abuse, CI, and release publication.
- [ ] Obtain an independent implementation review of the cryptography and
  protocol before v1.
- [ ] Run bounded fuzz campaigns for implemented parsers and state machines,
  then turn useful failures into deterministic tests.
- [ ] Run dependency, license, secret, workflow, SBOM, provenance, and artifact
  reviews.
- [ ] Run the focused unit, integration, protocol, exact-binary, native desktop,
  website, Android, service, migration, and recovery suites.
- [ ] Exercise interruption, full disk, permission loss, sleep, process death,
  relay outage, downgrade, revocation, expired invitations, corrupt state,
  uninstall, reinstall, backup, and rollback.
- [ ] With the v1 feature set and candidate scope frozen, run one repeatable
  release-qualification benchmark suite against the exact Linux, Windows,
  Android, and service artifacts. Measure direct and relayed transfer
  throughput, time to visible progress, cancel and resume responsiveness,
  memory, idle CPU, Android battery impact, startup, binary size, relay
  capacity, queue behavior, and declared resource ceilings.
- [ ] Run service load only in an isolated authorized environment. Sweep the
  controlled concurrency or arrival rate and report target and achieved work,
  p50/p95/p99 with their sample counts when supported, errors, timeouts, retries,
  dropped work, load-generator health, and saturation instead of one favorable
  throughput number.
- [ ] Use synthetic small, large, many-file, interrupted, direct, and relayed
  workloads. Preserve artifact and payload hashes, release profile, target,
  machine/device and network conditions, commands, raw results, errors, and
  ordinary run-to-run variation. Validate the received digest and protocol state
  before accepting any timing.

#### Done when

- [ ] No unresolved critical or high security issue remains.
- [ ] No known or unaccepted release-blocking defect remains.
- [ ] Exact Linux, Windows, Android, website, and service artifacts have recorded
  digests, provenance, and QA evidence.
- [ ] The exact candidates have a readable late benchmark report with correct
  transfers, bounded resources, known saturation behavior, and no unaccepted
  release-blocking performance collapse on the supported matrix.
- [ ] Recovery and rollback were executed, not only reviewed.

### Phase 15 - Release candidates and v1 publication

#### Build now

- [ ] Build immutable release candidates from reviewed tags and `Cargo.lock`.
- [ ] Sign where the qualified Linux, Windows, or Android distribution path
  requires it. Apple signing and notarization are post-v1 work.
- [ ] Smoke-test exact direct-download, Scoop, AUR, Android, web, and service
  artifacts.
- [ ] Publish package metadata only after the referenced assets pass.
- [ ] Run final cross-platform pairing, transfer, temporary-eye, self-host,
  update, rollback, and uninstall journeys.
- [ ] Record the security scope, QA verdict, known issues, residual risks, and
  separate ship decision for each candidate.
- [ ] Publish `1.0.0` without rebuilding or replacing qualified artifacts.

#### Done when

- [ ] Linux, Windows, and Android satisfy the declared support
  contract as complete senders and receivers.
- [ ] Website, public service, and self-hosted paths satisfy their declared
  support contracts.
- [ ] Every package and download resolves to the recorded immutable artifact and
  checksum.
- [ ] Upgrade, rollback, support, and security-response ownership are active.

### Post-v1 work

Begin only after Phase 15 publishes v1 and a written continuation decision
accepts Apple platform development, device access, maintenance, and distribution
costs. Qualify each added platform against the shipped compatibility contract.
Extend that contract only through its negotiated, versioned extension rules.

### Phase 16 - macOS desktop beta

#### Build now

- [ ] Qualify GPUI graph/list rendering, input, scaling, drag/drop, shutdown,
  keyboard access, and VoiceOver on native macOS before claiming support.
  Record dependency/upstream work and its continuation cost.
- [ ] Implement and test macOS IPC, Keychain, filesystem, notifications,
  clipboard, app lifecycle, startup, and shutdown.
- [ ] Add macOS CI, native packaging, and same-OS and cross-OS journeys with
  Linux, Windows, and Android.
- [ ] Select and qualify direct-download and Homebrew distribution, including
  signing, notarization, installation behavior, and any required membership
  budget. Publish package metadata only after immutable native artifacts pass.
- [ ] Qualify Unicode, permissions, sleep, restart, update, rollback, and
  uninstall natively.
- [ ] Review the added trust boundaries, measure the exact artifacts, and run
  native QA and release qualification before publishing macOS support.

#### Done when

- [ ] macOS is a complete sender and receiver with the already supported systems.
- [ ] The same protocol vectors and product rules pass without breaking the
  shipped Linux, Windows, or Android clients.
- [ ] Direct downloads and Homebrew resolve to qualified immutable artifacts
  with recorded digests, provenance, and native QA evidence.
- [ ] No known or unaccepted release-blocking macOS defect remains.
- [ ] A written continuation decision approves iOS work or holds the roadmap
  for redesign.

### Phase 17 - iOS alpha

#### Build now

- [ ] Create a native iOS application in Swift with SwiftUI. Keep the UI and
  platform lifecycle in Swift while reusing the shared Rust core and protocol.
  First pass a disposable real-device/Linux-peer probe of Iroh, Keychain-backed
  secret storage, foreground transfer, atomic app-owned storage, Files export,
  and suspend/termination recovery. Begin with app-must-be-open behavior;
  qualify any supported background mode before adopting it.
- [ ] Reuse or adapt the Android-proven Rust FFI contract only where it fits
  Swift ownership and concurrency. Prove typed errors, cancellation, shutdown,
  bounded immutable event delivery, release packaging, and deallocation on real
  devices. Revalidate every foreign input before it reaches core state.
- [ ] Add Keychain-backed identity storage, QR scanning, Files integration,
  security-scoped URL handling, a share extension, notifications, and explicit
  receive approval.
- [ ] Qualify atomic local receive storage and separate Files-provider export,
  including no-clobber creation, partial export, cancellation, and retention of
  verified local content after failure. Apply safe peer-text projections to
  consent screens, notifications, and errors.
- [ ] Keep generated or manual Swift and C bridge code small, typed, documented,
  and natively reviewed. No panic, borrowed Rust reference, or unowned callback
  may cross the boundary.
- [ ] Exercise received-file discovery, export, local deletion, and uninstall;
  users must understand which app-owned and external copies remain.
- [ ] Support foreground pairing, send, receive, cancel, and resume.
- [ ] Add iOS CI, protocol vectors, integration tests where practical, and real
  iPhone and iPad QA.
- [ ] Select and budget the Apple distribution path, then build, sign, install,
  and update a release-profile artifact through it.

#### Done when

- [ ] iOS pairs and transfers with Linux, Windows, macOS, and Android.
- [ ] Share-extension, Files, security-scoped URL, and Keychain flows preserve
  permissions, secrets, and cleanup.
- [ ] Provider export never changes the verified receipt outcome or claims
  atomicity without qualification; failures and remaining partial output are
  visible.
- [ ] Unsupported background behavior is stated plainly.
- [ ] The signed iOS artifact installs and launches on supported real devices.

### Phase 18 - iOS lifecycle beta and release qualification

#### Build now

- [ ] Handle backgrounding, suspension, process termination, reboot, network
  change, storage pressure, permission revocation, notification denial, and app
  update.
- [ ] Define the accepted background-transfer or app-must-be-open contract.
- [ ] Qualify the shutdown overrun policy without freeing resources still used
  by Rust or native jobs.
- [ ] Resume authenticated transfers without trusting stale partial state.
- [ ] Qualify supported iOS and iPadOS versions and representative devices.
- [ ] Run VoiceOver, Dynamic Type, Unicode, battery, idle, upgrade, rollback,
  uninstall, reinstall, and share-extension journeys.
- [ ] Measure foreground and accepted background transfer battery use, steady
  and peak memory, startup, time to visible progress, cancel and resume latency,
  idle work, and recovery after process termination on real devices.
- [ ] Review the Swift bridge, Keychain, share extension, provider export, and
  lifecycle trust boundaries. Qualify exact signed artifacts, record digests
  and provenance, and smoke-test the distribution channel before publication.

#### Done when

- [ ] iOS has an honest, repeatable lifecycle contract.
- [ ] No lifecycle path publishes unverified content or loses trust state
  silently.
- [ ] Desktop, Android, and iOS remain protocol-compatible within the shipped
  old/new version windows.
- [ ] iOS measurements satisfy the declared lifecycle and resource contract,
  and a written continuation decision approves its release or holds the
  roadmap for redesign.
- [ ] No known or unaccepted release-blocking iOS defect remains, and the exact
  artifacts pass native security, QA, packaging, and distribution checks.

## Test Strategy

Testing follows risk, not the planned directory tree.
V1 coverage includes Linux, Windows, and Android. Add macOS and iOS coverage
when their post-v1 implementation begins, before claiming support.

### Layers

- **Unit tests:** trust and offer states, active-session revocation, publication
  ordering, safe display projections, spool and export states, limits, safe
  names, version decisions, config, and error mapping.
- **Property tests:** state-machine legality, frame round trips, chunk
  accounting, normalization, and idempotence.
- **Integration tests:** real filesystem, persistence, IPC, secure store,
  network, migrations, two endpoints, revocation/publication races, stdin spool
  recovery, unsupported destinations, and provider export failure.
- **Protocol vectors:** Linux, Windows, Android, service, and supported old/new
  versions; extend to Apple clients after v1.
- **End-to-end tests:** a small set of shipped CLI, GUI, website, Android, and
  package journeys; add Apple journeys with their post-v1 releases.
- **Fuzz tests:** invitation, link, frame, manifest, config, IPC, and service
  decoders, added when each boundary exists.
- **Exploratory QA:** lifecycle, interruption, network change, permissions,
  accessibility, upgrade, and recovery.

### Rules

- Test through public behavior.
- One test protects one named behavior.
- Make each high-value test fail once for the expected reason before accepting
  it. No permanent traceability register is required.
- Inject time, randomness, IDs, retries, and network faults.
- Wait for readiness or state changes, never a guessed sleep.
- Let every test own and clean its files, ports, processes, identities, and
  service state.
- Assert negative space: no trust, no publication, no clobber, no secret log,
  no stale state transition, no raw peer controls, no forged consent prompt,
  no post-revocation authorization, and no invitation secret sent to the website.
- Use real boundaries where a mock would hide the failure.
- Keep end-to-end suites small and focused on user journeys.
- Fix or delete flaky and redundant tests. Never retry them into green.

Build a deterministic network harness only as real transport failures demand
it. It may delay, drop, reorder, duplicate, corrupt, restart, and change routes
inside isolated test environments. It never contacts public Hyakume services in
ordinary tests.

## Security Review

When a phase adds or changes a trust boundary, check each meaningful risk for:

- asset and attacker;
- reachable path and impact;
- mitigation;
- verification;
- remaining risk.

Before the relevant feature ships, review:

1. invitation entropy, native URI handling without web fallback, PAKE, attempt
   limits, replay, verifier, and key separation;
2. peer authentication, version downgrade, resume, active-session revocation,
   publication ordering, and transfer state;
3. filenames, paths, peer-text rendering, consent spoofing, partial files,
   stdin spools, clobber, symlinks, quotas, separate exports, and cleanup;
4. local IPC identity and state-store permissions;
5. rendezvous, relay, metadata, abuse, retention, and service credentials;
6. website origin and deployment compromise, CSP, third-party requests,
   download integrity, and provenance;
7. Android intents, JNI, lifecycle, key storage, and content URIs;
8. dependencies, CI, signing, provenance, package channels, and rollback.

After v1, review macOS IPC, Keychain, native integration, packaging, and
distribution before shipping macOS support. Review iOS share extensions,
security-scoped URLs, native bridges, lifecycle, Keychain, and notification
handling before shipping iOS support.

Scanner output is a lead. It does not prove protocol or product safety. Security
reports name the exact version, scope, evidence, exclusions, and uncertainty.
They never claim that Hyakume as a whole is secure.

## QA and Platform Evidence

Each release-relevant run records the artifact digest, commit, dirty state,
toolchain, OS or device, architecture, filesystem, locale, service config,
synthetic identities and payloads, exact steps, expected result, actual result,
and cleanup.

The critical v1 journeys are:

1. first start without an account;
2. QR and human-code pairing;
3. trusted reconnect after restart;
4. send and receive every payload kind, including a partially completed batch;
5. reject, cancel, resume, and preserve an existing file;
6. remove, lose, reset, or suspect compromise of a local identity, revoke where
   required, and re-pair a device;
7. direct transfer and honest relay fallback;
8. temporary native transfer whose first approved claim consumes the invitation;
9. self-hosted service replacement;
10. Android share-target and lifecycle recovery;
11. compatible upgrade and safe incompatible downgrade;
12. install, launch, update, rollback, uninstall, and reinstall.

Include hostile peer names and previews, native invitation handling with no app
installed, revocation during transfer and publication, stdin spool recovery,
unsupported direct destinations, and failed provider exports in these journeys.

Before v1, Linux, Windows, and Android each act as sender and receiver. Use
representative same-system and cross-system pairs. Test supported browsers on
their declared Linux, Windows, and Android systems. A full combinatorial matrix
is not required unless failures show it has value.

Post-v1 macOS and iOS QA adds their native journeys, including iOS share-extension
and lifecycle recovery, and checks compatibility with already supported devices.
Their artifacts, signing, channels, and native QA do not block v1.

Use synthetic data in isolated local or staging environments. Preserve the
first intermittent failure. Do not retry it into green.

Verdicts are `PASS`, `PASS WITH KNOWN ISSUES`, `FAIL`, `BLOCKED`, or
`INCONCLUSIVE`. State `ship`, `hold`, or `no recommendation` separately.

## Measurement and Release Benchmark Policy

Hyakume requires focused feasibility measurements when the transfer core,
Android lifecycle, relay, and hosted service first become real. These Phase 4,
9, 10, and 11 measurements test architecture and resource bounds. Post-v1
Phases 16 and 18 measure the added macOS and iOS implementations before release.
They are not public performance claims. Each run uses production-equivalent
builds, verifies authentication and received bytes first, and records its
workload, commands, machine or device, network, errors, resource use, and ordinary
variation. An architecture-blocking result stops the next expensive phase for
redesign.
After a pilot establishes the noise floor, each gate declares its practical
decision threshold before the qualifying runs. Preserve machine-readable raw
samples, exact commands, independent run counts, and the written verdict in an
approved location. Do not move a threshold after seeing the result.

The v1 release-qualification benchmark belongs in Phase 14, after Phase 13
freezes the v1 contracts and supported matrix. It runs against the exact
Release-profile artifacts people will receive and includes the complete direct
and relay paths, Linux/Windows GUI and service boundaries, Android lifecycle,
and real packaging.
Post-v1 Apple releases qualify their own exact artifacts under the same policy.
Public performance statements come only from this evidence.

Do not invent throughput targets or optimize cryptographic and protocol code
from intuition. Retain an optimization only when transfers remain authenticated
and byte-correct and the same workload shows a useful gain beyond ordinary
variation. Keep a harness after its decision only if it remains reproducible on
the platforms where its result is used.

## Release and Packaging

The v1 release path stays ordinary:

1. Qualify one commit and create a release-candidate tag.
2. Build Linux, Windows, Android, web, and service artifacts from the
   locked graph.
3. Record contents, checksums, signatures, SBOM, and provenance.
4. Smoke-test the exact artifacts.
5. Publish passing artifacts unchanged.
6. Publish Scoop, AUR, and Android channel metadata that points to them.
7. Smoke-test public downloads, package channels, deployment, and rollback.

A failure creates a new candidate. Never replace a published artifact silently.
Apply the same qualification and immutable-artifact rules to post-v1 Apple
releases. Add Homebrew and iOS channels only with those releases.

Use two workflows to begin with: `ci.yml` and `release.yml`. Add another only
when a real platform requirement cannot fit clearly in those two.

## Documentation Policy

Do not create an empty mature `docs/` tree up front.

Keep behavioral definitions in their owning contract sections. Required
Invariants summarize them, the Fault Model applies them, and phase checklists
verify them. Reference those definitions when adding tasks; do not restate
different retention, aggregation, authorization, or shutdown rules.

Start with:

- `README.md` with a short explanation of the project, the tool setup, and the
  command that checks and builds it;
- this plan as `PROJECT.md`; keep phase numbers and progress checkboxes here,
  not in the README or other documents;
- `SECURITY.md` when the project is public;
- `LICENSE` with the Apache-2.0 text and the third-party notice policy;
- `docs/book.md`, which explains what was built, how it works, and why;
- short decision records in `docs/` for the accepted core choices.

Add documents when their subject exists:

- protocol and compatibility when a second client or service consumes them;
- safe consent rendering, publication guarantees, stdin preparation and resume,
  and native invitation handling when their CLI features appear; document
  separate provider export with each mobile application;
- privacy and service metadata in Phase 10;
- self-hosting and recovery in Phase 11;
- website and download help in Phase 12;
- platform support and installation as each native package ships;
- feasibility measurement methods and results when each measurement gate runs;
- final release and upgrade notes during qualification.

Use a short decision record only for a choice that is expensive to reverse,
such as transport identity, PAKE, storage, or invitation consumption. Source,
tests, manifests, and CI remain authoritative for implementation details.

## Brand and Art

Meme, the eye system, logo, wordmark, icons, and public illustrations are made
by a professional human artist. The engineering repository preserves editable
sources, exports, authorship, licenses, rights, attribution, platform variants,
and motion guidance.

Keep the commission brief separate from this roadmap. It should cover:

- mascot views, expressions, construction, and small-size forms;
- modular eye layers, poses, anchors, and allowed combinations;
- logo, wordmark, icons, palette, typography, and accessibility variants;
- GUI, graph, website, onboarding, transfer, error, and empty states;
- animation-ready layers, pivots, key poses, timing, and reduced motion;
- editable sources, export formats, third-party materials, rights, attribution,
  trademark policy, and permission before external AI processing.

Disposable geometric placeholders are allowed during development. They are not
the public identity.

## Planning References

These sources guide the phase that first uses each component. Decision records
and manifests pin exact accepted versions and preserve the evidence used at
that time.

- [Rust release history](https://doc.rust-lang.org/stable/releases.html)
- [Tokio blocking-task cancellation](https://docs.rs/tokio/latest/tokio/task/fn.spawn_blocking.html)
- [Iroh documentation](https://docs.iroh.computer/)
- [Iroh local address lookup](https://docs.iroh.computer/connecting/local-address-lookup)
- [GPUI](https://github.com/zed-industries/zed/tree/main/crates/gpui)
- [GPUI accessibility guide](https://github.com/zed-industries/zed/blob/main/crates/gpui/src/_accessibility.rs)
- [Astro](https://docs.astro.build/)
- [pnpm](https://pnpm.io/)
- [Cloudflare Pages build configuration](https://developers.cloudflare.com/pages/configuration/build-configuration/)
- [Cloudflare Pages custom domains](https://developers.cloudflare.com/pages/configuration/custom-domains/)
- [Jetpack Compose](https://developer.android.com/compose)
- [SwiftUI](https://developer.apple.com/xcode/swiftui/) (post-v1)
- [UniFFI user guide](https://mozilla.github.io/uniffi-rs/latest/)
- [SPAKE2, a balanced PAKE (RFC 9382)](https://www.rfc-editor.org/rfc/rfc9382.html)
- [CPace balanced PAKE (IRTF CFRG draft)](https://datatracker.ietf.org/doc/draft-irtf-cfrg-cpace/)
- [Freedesktop Secret Service API](https://specifications.freedesktop.org/secret-service-spec/latest/)
- [age file encryption format](https://age-encryption.org/v1)
- [Android Keystore](https://developer.android.com/privacy-and-security/keystore)
- [Windows UI Automation providers](https://learn.microsoft.com/windows/win32/winauto/uiauto-providersoverview)
- [Apple accessibility for custom views](https://developer.apple.com/documentation/appkit/nsaccessibilityprotocol) (post-v1)
- [Apple Keychain Services](https://developer.apple.com/documentation/security/keychain-services) (post-v1)
- [Apple app extensions](https://developer.apple.com/app-extensions/) (post-v1)
- [Apple custom URL schemes](https://developer.apple.com/documentation/xcode/defining-a-custom-url-scheme-for-your-app) (post-v1)
- [iOS background execution limits](https://developer.apple.com/forums/thread/685525) (post-v1)

## Honest Notes

Hyakume is ambitious because cross-device trust is the product. Linux, Windows,
and Android are required for v1, in that order. macOS and iOS follow after v1.
Each platform is qualified natively. A failed platform gate requires a recorded
implementation plan or roadmap decision within the fixed GPUI and Iroh
constraints.

The largest risks are short-code security, protocol compatibility, safe file
publication, background lifecycle, website and download integrity, relay
metadata and cost, and the number of supported platforms. Phase 1 tests the core
and Phase 3 proves private discovery before LAN pairing ships. GPUI and mobile
capabilities pass their own gates before the affected application/support work;
optional early probes expose risk without delaying the first Linux CLI release.

Iroh, GPUI, native Kotlin with Jetpack Compose, and the separate static Astro
website using pnpm and Cloudflare Pages are committed v1 implementation choices.
Native Swift with SwiftUI belongs to post-v1 iOS work. Their owning phases must
still prove the relevant transport, native integration, accessibility,
deployment, and release behavior.
GPUI and Iroh capability gaps require integration or upstream work within those
dependencies. Other failed mandatory checks require a recorded architecture
revision. SQLite, the mobile FFI mechanism, and every PAKE choice remain open
until the phase that needs them. A popular dependency is not a product guarantee.

V1 may arrive after many `0.x` releases. The number does not matter. The support
contract does.
