# Initial resource estimate

Status: estimate from the implemented limits, with one rough measurement.
Real measurements on the declared hardware come with reliable LAN transfer.
Recorded 2026-10-12; updated 2026-10-13 for the local service and files on
disk, and 2026-10-14 for the real number of kept transfers.

## What bounds memory

| Consumer | Bound | Where |
| --- | --- | --- |
| Frames being read | each frame leases 2 × its payload (raw and decoded) from the buffer budget before allocating; at most 2 frames per connection (one read ahead) | `src/node.rs` `read_frame`, `FRAME_QUEUE` |
| Chunks being sent | 3 × (chunk + 64 bytes) leased per chunk (read buffer, payload, frame); chunks leave `FRAME_HEADROOM` (2 MiB) free for reading frames | `send_session`, `Budget::lease_send_wait` |
| Received bytes | written to the item's private partial file as they arrive; a chunk's frame lease lasts until its write ends, so nothing grows with the file | `receive_session`, `write_partial` |
| All of the above | **128 MiB** buffer budget, at least 5 MiB when lowered; readers and senders wait when it is full, so QUIC flow control pushes back on the peer | `src/limits.rs` `Budget` |
| QUIC windows | 1.25 MiB stream and connection receive windows and send window per connection; one stream per connection | `bind_endpoint` |
| Open connections | 40 in both directions: one per session (32, incoming and outgoing together) and 8 that hold no session, such as pairing exchanges and incoming connections still proving who they are. An incoming connection past the 8 is refused before the handshake | `Budget::open_connection`, `accept_loop` |
| Writes | a frame write gives up after the idle timeout, or at once when revocation or cancellation stops its transfer; the stream is then reset, so a peer that stops reading cannot hold a connection | `Wire::write_parts` |
| Time before proof | everything before the peer proves who it is (QUIC handshake, Hello, session proof or pairing steps, and closing) shares one 30 s deadline, however slowly the peer writes; only the person's answer gets longer | `Wire::cap`, `serve`, `Node::dial` |
| Idle sessions | a session ends after 10 minutes without reading or writing a frame of its own; other sessions' work does not reset it | `Wire::idle_until` |
| Questions for the person | 8 waiting at once; another offer is refused as busy at once, so no connection waits for room | `Shared::ask` |
| Decoded peer text | names 1 KiB, previews 4 KiB, offers 64 KiB, 256 items | `src/limits.rs`, `src/protocol.rs` |
| Remembered offer IDs | the last 4,096 offered transfer IDs, 64 KiB, separate from the 256 transfers kept for status | `State::first_offer` |
| Projected peer text | at most 4 zero-width characters after each visible one, so output grows with the column limit only | `SafeLine::project` |
| Kept transfer records | up to 256 transfers, finished and unfinished together (at most 64 unfinished), each with its offer of at most 64 KiB of metadata: 16 MiB at worst, a few KiB each in practice | `State`, `TransferInfo` |
| Local clients | 8 clients, each reading at most one 1 MiB frame at a time and queueing at most 64 events | `src/ipc.rs`, `serve_client` |
| File preparation and blocking file work | 4 service jobs, at most a 1 MiB hashing buffer each; 4 file operations at once, whose data is already leased | `SERVICE_JOBS`, `FS_JOBS` |
| Unlocking a key file | scrypt at N = 2^17: 128 MiB for under a second, while the service is locked and no node runs | `secrets::KEY_FILE_WORK_FACTOR` |

## Worst case for the desktop service

| Part | MiB |
| --- | ---: |
| Buffer budget (frames, chunk copies) | 128 |
| QUIC windows: 40 connections in both directions × (1.25 receive + 1.25 send) | 100 |
| Kept transfer records at their worst | 16 |
| Local clients: 8 frames of 1 MiB | 8 |
| Preparation buffers | 4 |
| Runtime, task stacks, state, SQLite cache, decoding, logs (estimate) | 20 |
| **Total** | **276** of 256 |

The worst case is now 20 MiB over the budget. Reaching it takes a trusted
device sending 256 offers of maximum metadata while every other limit is
full at once, but the budget is meant to hold in the worst case. The levers,
in order: read the offers of finished transfers from the database instead of
keeping them in memory, which takes the 16 MiB back; then lower the QUIC
windows or the number of unauthenticated connections. The reliable-transfer
work measures real memory and must bring the total back under 256 MiB before
these values are accepted.

These windows cap one connection at about 1.25 MiB per round trip, which is
enough on a LAN (a 1 ms round trip allows over 1 GiB/s) but limits a 50 ms
internet path to about 25 MiB/s. The internet relay work should measure
this.

## On disk

| Consumer | Bound | Where |
| --- | --- | --- |
| Private partial files | 32 GiB together, paused work and pending deletions included; an offer that does not fit is refused before consent | `MAX_PARTIAL_TOTAL`, the file ledger |
| Sender spools | 2 GiB each, 8 GiB together, sealed or still being written | `MAX_SPOOL_BYTES`, `MAX_SPOOL_TOTAL` |
| Free space | every partial and spool write leaves 64 MiB free on its filesystem | `MIN_FREE_BYTES` |
| State database | at most 512 trust records, 4,096 remembered offer IDs, 256 transfers | `src/storage.rs` |

## Memory the loopback proof used to need

The loopback proof kept each sender's payload and each receiver's staged
copy in memory. Both are gone: senders read prepared files and sealed
spools, and receivers write partial files as bytes arrive. The measurement
below predates that change.

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
