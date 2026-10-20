# Frame links: `peer_link` and `shm_bridge`

Two modules of the host that carry the byte frames of `include/xgc2/frame.h` between entities. They exist because the host
has no network stack and typed ports never leave a process: whatever crosses a process or host boundary is a frame, and the
algorithm owns its bytes (the links never parse `data`). One link instance serves one entity; a topology adds one per mesh
member (see the platform's `xgc.topology/v1`).

Common ports (the same for both, in descriptor order):

| Port | Dir | Kind | Schema | Meaning |
|---|---|---|---|---|
| `tx` | in | event (64) | `xgc2.frame2k.v1` (`..8k..` in the 8k build) | frames to send; `peer` = destination endpoint, `0xFFFF` = every peer |
| `rx` | out | event (64), async writer | same | frames received; `peer` = sender, flags bit 0 = it was a broadcast; the sample stamp is the receive time |
| `stats` | out | state | `xgc2.link_stats.v1` | 18 monotone 64-bit counters (`modules/common/link_stats.h`), once per `stats_period_ms` |

Libraries: `libxgc2_peer_link.so`, `libxgc2_peer_link_8k.so`, `libxgc2_shm_bridge.so`, `libxgc2_shm_bridge_8k.so` (`modules/CMakeLists.txt`,
install `lib/xgc2/modules`). The frame class is fixed at build time (a port has a fixed size).

## `peer_link` (UDP)

```toml
[[instance]]
name = "radio_link"
module = "peer_link"
[instance.config]
self = 3                                  # this entity's endpoint id, 0..65534
bind = "0.0.0.0:47103"                    # local UDP address
peers = { "1" = "10.253.77.11:47101", "2" = "10.253.77.12:47102" }
datagram_bytes = 1400                     # UDP payload, 512..1472
reassembly_timeout_ms = 100               # a frame missing a fragment this long is dropped as a whole
reassembly_slots = 8                      # frames in flight per sender
stats_period_ms = 1000
[instance.bind]
tx = "dmpc.tx"
rx = "dmpc.rx"
stats = "radio_link.stats"
```

Datagram header (28 bytes, little-endian): `'X' 'G'`, version 1, flags, src, dst, frame_seq, kind, total_len, frag_idx, frag_cnt,
frag_len, t_tx_ns (layout in `modules/common/frame_wire.hpp`). A frame needs ceil(len / (datagram_bytes - 28)) datagrams, at
most 8; the transmit side refuses a frame that would need more. There is no retransmission and no partial delivery. Counters
(`xgc2_link_stats`): `tx_frames`, `tx_datagrams`, `tx_bytes`, `tx_invalid` (unknown destination, itself, length, too many
fragments), `tx_would_block`, `tx_errors`, `rx_datagrams`, `rx_bytes`, `rx_frames`, `rx_malformed` (magic, version,
geometry), `rx_unknown_peer`, `rx_misaddressed`, `rx_duplicates`, `rx_incomplete` (frames dropped for a missing fragment),
`rx_queue_full` (the `rx` port was full), `rx_reordered`, `rx_seq_lost` (loss estimated from sequence gaps of a sender over a 64-frame
window), `last_rx_ns`.

`self` and `bind` cannot change while the link runs (`configure` answers `XGC2_ERR_INVALID`); the peer table, the timeouts and
the stats period can. Health: `degraded` when send errors or full `rx` queues increased since the last report.

## `shm_bridge` (POSIX shared memory)

```toml
[instance.config]
shm_name = "/xgc2-s1-planners"            # one slash, then [A-Za-z0-9._-]
self = 0                                  # endpoint id = inbox index, 0..endpoints-1
endpoints = 4
slots = 64                                # per inbox, a power of two
owner = true                              # exactly one instance per segment: creates it (and removes a stale one), unlinks at stop
attach_timeout_ms = 2000                  # a non-owner waits this long for the owner
```

One inbox ring per endpoint in one segment; any process writes to any inbox, only the owner of an endpoint reads its inbox. A
writer never waits: a full inbox drops the frame and counts it (`tx_would_block`). Order of the frames of one sender to one
inbox is kept.

## Tests

`modules/tests/run.sh OUTDIR [--netns private-netns.py]` builds both frame classes and runs: the datagram format, fragmentation,
reassembly in any order, the wire budget of the TRO messages (`frame_wire_test`), `peer_link` over loopback (addressing,
fragments, broadcast, entry validation, duplicates, configuration, lifecycle) and `shm_bridge` (order, full inbox, geometry,
two processes). With `--netns` the packet-level test runs inside a private user/network namespace (loss, delay, reordering,
duplication, blackhole and recovery with `tc netem` on the namespace's loopback; nothing is created on the machine). The
host-level tests (`cargo test --test frame_links`) run two TRO-shaped agents in two hosts over each link.
