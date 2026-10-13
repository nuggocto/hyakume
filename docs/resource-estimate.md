# Initial resource estimate

Status: estimate from the implemented limits, with one rough measurement.
Real measurements on the declared hardware come with reliable LAN transfer.
Recorded 2026-10-12.

## What bounds memory

| Consumer | Bound | Where |
| --- | --- | --- |
| Frames being read | each frame leases 2 × its payload (raw and decoded) from the buffer budget before allocating; at most 2 frames per connection (one read ahead) | `src/node.rs` `read_frame`, `FRAME_QUEUE` |
| Chunks being sent | 3 × (chunk + 64 bytes) leased per chunk (slice copy, payload, frame); chunks and staged bytes together leave `FRAME_HEADROOM` (2 MiB) free for reading frames | `send_session`, `Budget::lease_send_wait` |
| Staged received bytes | the whole accepted offer, leased at acceptance; staged bytes leave `MIN_BUFFER_BYTES` (5 MiB) free, room to send one largest chunk and read one largest frame at once, and an offer that cannot fit is refused before consent | `src/receive.rs`, `Budget::lease_held` |
| All of the above | **128 MiB** buffer budget, at least 5 MiB when lowered; readers and senders wait when it is full, so QUIC flow control pushes back on the peer | `src/limits.rs` `Budget` |
| QUIC windows | 1.25 MiB stream and connection receive windows and send window per connection; one stream per connection | `bind_endpoint` |
| Open connections | 40 in both directions: one per session (32, incoming and outgoing together) and 8 that hold no session, such as pairing exchanges and incoming connections still proving who they are. An incoming connection past the 8 is refused before the handshake | `Budget::open_connection`, `accept_loop` |
| Writes | a frame write gives up after the idle timeout, or at once when revocation or cancellation stops its transfer; the stream is then reset, so a peer that stops reading cannot hold a connection | `Wire::write_parts` |
| Time before proof | everything before the peer proves who it is (QUIC handshake, Hello, session proof or pairing steps, and closing) shares one 30 s deadline, however slowly the peer writes; only the person's answer gets longer | `Wire::cap`, `serve`, `Node::dial` |
| Idle sessions | a session ends after 10 minutes without reading or writing a frame of its own; other sessions' work does not reset it | `Wire::idle_until` |
| Questions for the person | 8 waiting at once; another offer is refused as busy at once, so no connection waits for room | `Shared::ask` |
| Model publisher store | received items stay in memory until taken: at most 64 MiB and 1,024 items together with accepted ones, so empty files count too | `src/state.rs` `Room` |
| Decoded peer text | names 1 KiB, previews 4 KiB, offers 64 KiB, 256 items | `src/limits.rs`, `src/protocol.rs` |
| Remembered offer IDs | the last 4,096 offered transfer IDs, 64 KiB, separate from the 256 transfers kept for status | `State::first_offer` |
| Projected peer text | at most 4 zero-width characters after each visible one, so output grows with the column limit only | `SafeLine::project` |

## Worst case for the future desktop service

| Part | MiB |
| --- | ---: |
| Buffer budget (frames, chunk copies, staging) | 128 |
| QUIC windows: 40 connections in both directions × (1.25 receive + 1.25 send) | 100 |
| Runtime, task stacks, state, decoding, logs (estimate) | 20 |
| **Total** | **248** of 256 |

The margin is small. Lower the QUIC windows or the number of unauthenticated
connections before raising anything else. These windows cap one connection
at about 1.25 MiB per round trip, which is enough on a LAN (a 1 ms round trip
allows over 1 GiB/s) but limits a 50 ms internet path to about 25 MiB/s. The
internet relay work should measure this.

## Proof-only memory

The loopback proof keeps two things in memory that the product will keep on
disk:

- the sender's whole payload, until the stdin spool and file reading arrive;
- the model publisher's received items, until the qualified file pipeline
  arrives.

These are outside the service estimate above.

## One measurement

Two nodes in one process on this Linux machine (32 cores, Rust 1.99.0,
release build), one file each time:

| Payload | Time | Peak RSS of the process |
| --- | ---: | ---: |
| 1 MiB | 0.06 s | 17.9 MiB |
| 32 MiB | 0.11 s | 86.2 MiB |

The 32 MiB run holds the sender's payload and the receiver's staged copy
(64 MiB) on top of an 18 MiB base, as expected from the proof-only items
above. The measurement program was a throwaway; the method is: bind two
`Node`s, pair them, send one generated file with default options, and read
`ru_maxrss` from `wait4`.
