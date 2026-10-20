/* The counters of a frame link (peer_link, shm_bridge): the payload of the module's `stats` port, schema
 * xgc2.link_stats.v1. Plain 64-bit counters, monotone since start; a dashboard differences them. */
#ifndef XGC2_LINK_STATS_H
#define XGC2_LINK_STATS_H

#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

#define XGC2_LINK_STATS_SCHEMA "xgc2.link_stats.v1"

typedef struct xgc2_link_stats {
  uint64_t tx_frames;        /* frames accepted from the tx port and sent (one per destination for broadcast) */
  uint64_t tx_datagrams;     /* datagrams (fragments) put on the link */
  uint64_t tx_bytes;         /* link-level payload bytes of those datagrams, headers included */
  uint64_t tx_invalid;       /* frames refused: unknown destination, length above the capacity, too many fragments */
  uint64_t tx_would_block;   /* datagrams the link did not take (socket buffer full, ring full) */
  uint64_t tx_errors;        /* send failures other than would-block */
  uint64_t rx_datagrams;     /* datagrams received */
  uint64_t rx_bytes;         /* link-level payload bytes received */
  uint64_t rx_frames;        /* frames completed and handed to the rx port */
  uint64_t rx_malformed;     /* datagrams refused by the entry checks (magic, version, geometry) */
  uint64_t rx_unknown_peer;  /* datagrams from an endpoint that is not a configured peer */
  uint64_t rx_misaddressed;  /* datagrams for another endpoint */
  uint64_t rx_duplicates;    /* fragments received twice */
  uint64_t rx_incomplete;    /* frames dropped as a whole because a fragment never arrived */
  uint64_t rx_queue_full;    /* frames dropped because the rx port was full */
  uint64_t rx_reordered;     /* frames that arrived after a younger frame of the same sender */
  uint64_t rx_seq_lost;      /* frames missing in the sequence numbers of a sender (loss estimate) */
  int64_t last_rx_ns;        /* host clock at the last completed frame, 0 before the first */
} xgc2_link_stats;           /* 144 bytes */

#ifdef __cplusplus
static_assert(sizeof(xgc2_link_stats) == 144, "link stats layout");
}
#endif

#endif
