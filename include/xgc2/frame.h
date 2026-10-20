/*
 * xgc2/frame.h - the byte frame convention of xgc2-module (ABI minor 1).
 *
 * A frame is a fixed-size POD with a small header and a byte payload whose meaning belongs to
 * the algorithm that sends it. Ports that cross a process or host boundary (the shm and udp
 * bridge modules, peer_link) carry frames, so an algorithm-owned wire format passes through
 * unchanged: the platform never parses `data`. Inside a host an algorithm keeps its typed
 * ports; frames exist for the edges only.
 *
 * Three size classes cover the algorithms of this repository; each is a distinct payload schema
 * (the host checks schema id, size and alignment when it binds channels):
 *   xgc2.frame256.v1   256 bytes, 240 bytes of data (beacons, acknowledgements, small commands)
 *   xgc2.frame2k.v1    2048 bytes, 2032 bytes of data (plans up to N = 80 samples)
 *   xgc2.frame8k.v1    8192 bytes, 8176 bytes of data (plans with velocities, N = 80: 3904 bytes)
 *
 * Header, all fields native-endian inside a host, little-endian on a wire:
 *   peer     transmit: destination endpoint id, XGC2_FRAME_BROADCAST for every configured peer;
 *            receive: the sender's endpoint id.
 *   kind     message kind, algorithm-owned. Never interpreted by a bridge.
 *   len      valid bytes of `data`, at most the class capacity.
 *   flags    XGC2_FRAME_FLAG_*; zero when sent.
 *   t_tx_ns  time of the sender, nanoseconds of the session domain (utc or simulation time).
 *            Transmit: filled by the algorithm, or 0 to let the bridge use its clock at send.
 *            Receive: as sent. The sample stamp of the received frame is the receive time.
 */
#ifndef XGC2_FRAME_H
#define XGC2_FRAME_H

#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

#define XGC2_FRAME_BROADCAST 0xFFFFu
#define XGC2_FRAME_FLAG_BROADCAST 0x1u /* receive: the frame was addressed to every peer */
#define XGC2_FRAME_HEADER_BYTES 16u

#define XGC2_FRAME256_SCHEMA "xgc2.frame256.v1"
#define XGC2_FRAME256_DATA 240u
#define XGC2_FRAME2K_SCHEMA "xgc2.frame2k.v1"
#define XGC2_FRAME2K_DATA 2032u

#define XGC2_FRAME8K_SCHEMA "xgc2.frame8k.v1"
#define XGC2_FRAME8K_DATA 8176u

#define XGC2_FRAME_DECLARE(type, capacity)                                                         \
  typedef struct type {                                                                            \
    uint16_t peer;                                                                                 \
    uint16_t kind;                                                                                 \
    uint16_t len;                                                                                  \
    uint16_t flags;                                                                                \
    int64_t t_tx_ns;                                                                               \
    uint8_t data[capacity];                                                                        \
  } type

XGC2_FRAME_DECLARE(xgc2_frame256, XGC2_FRAME256_DATA);
XGC2_FRAME_DECLARE(xgc2_frame2k, XGC2_FRAME2K_DATA);
XGC2_FRAME_DECLARE(xgc2_frame8k, XGC2_FRAME8K_DATA);

#ifdef __cplusplus
static_assert(sizeof(xgc2_frame256) == 256, "frame256 layout");
static_assert(sizeof(xgc2_frame2k) == 2048, "frame2k layout");
static_assert(sizeof(xgc2_frame8k) == 8192, "frame8k layout");
#else
_Static_assert(sizeof(xgc2_frame256) == 256, "frame256 layout");
_Static_assert(sizeof(xgc2_frame2k) == 2048, "frame2k layout");
_Static_assert(sizeof(xgc2_frame8k) == 8192, "frame8k layout");
#endif

#ifdef __cplusplus
}
#endif

#endif /* XGC2_FRAME_H */
