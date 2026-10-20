#pragma once

// What the frame links (peer_link, shm_bridge) share: the port table, the counters, the endpoint table and the
// validation of the frames that cross the module boundary. A frame link is a module with three ports:
//   tx     event in   xgc2.frame<class>.v1  frames to send (peer = destination endpoint or broadcast)
//   rx     event out  xgc2.frame<class>.v1  frames received (peer = sender), written by the link's own thread
//   stats  state out  xgc2.link_stats.v1    the counters, once per stats period
// Only the link validates what comes from outside; the modules on either side trust typed ports.

#include <xgc2/frame.h>
#include <xgc2/sdk/host.hpp>
#include <xgc2/sdk/json_config.hpp>

#include <atomic>
#include <cstdint>
#include <string>
#include <vector>

#include "link_stats.h"

namespace xgc2 {
namespace sdk {
namespace frame_link {

#ifndef XGC2_LINK_FRAME_CLASS
#define XGC2_LINK_FRAME_CLASS 2k
#endif
#define XGC2_LINK_CAT2(a, b) a##b
#define XGC2_LINK_CAT(a, b) XGC2_LINK_CAT2(a, b)
#define XGC2_LINK_STR2(x) #x
#define XGC2_LINK_STR(x) XGC2_LINK_STR2(x)

using FrameType = XGC2_LINK_CAT(xgc2_frame, XGC2_LINK_FRAME_CLASS);
constexpr uint32_t kFrameCapacity = static_cast<uint32_t>(sizeof(FrameType::data));
constexpr const char* kFrameSchema = "xgc2.frame" XGC2_LINK_STR(XGC2_LINK_FRAME_CLASS) ".v1";

constexpr uint32_t kTx = 0;
constexpr uint32_t kRx = 1;
constexpr uint32_t kStats = 2;

// Event queue depths of the two frame ports: a burst of a whole period of broadcasts fits.
constexpr uint32_t kQueueDepth = 64;

inline const xgc2_port_desc* ports() {
    static const xgc2_port_desc table[] = {
        port<FrameType>("tx", XGC2_PORT_IN, XGC2_PORT_EVENT, kFrameSchema, kQueueDepth),
        port<FrameType>("rx", XGC2_PORT_OUT, XGC2_PORT_EVENT, kFrameSchema, kQueueDepth, XGC2_PORT_ASYNC_WRITER),
        port<xgc2_link_stats>("stats", XGC2_PORT_OUT, XGC2_PORT_STATE, XGC2_LINK_STATS_SCHEMA),
    };
    return table;
}
constexpr uint32_t kPortCount = 3;

// The counters, written by the step thread and the receive thread.
struct Counters {
    std::atomic<uint64_t> tx_frames{0}, tx_datagrams{0}, tx_bytes{0}, tx_invalid{0}, tx_would_block{0},
        tx_errors{0}, rx_datagrams{0}, rx_bytes{0}, rx_frames{0}, rx_malformed{0}, rx_unknown_peer{0},
        rx_misaddressed{0}, rx_duplicates{0}, rx_incomplete{0}, rx_queue_full{0}, rx_reordered{0},
        rx_seq_lost{0};
    std::atomic<int64_t> last_rx_ns{0};

    void snapshot(xgc2_link_stats& out) const {
        out.tx_frames = tx_frames.load();
        out.tx_datagrams = tx_datagrams.load();
        out.tx_bytes = tx_bytes.load();
        out.tx_invalid = tx_invalid.load();
        out.tx_would_block = tx_would_block.load();
        out.tx_errors = tx_errors.load();
        out.rx_datagrams = rx_datagrams.load();
        out.rx_bytes = rx_bytes.load();
        out.rx_frames = rx_frames.load();
        out.rx_malformed = rx_malformed.load();
        out.rx_unknown_peer = rx_unknown_peer.load();
        out.rx_misaddressed = rx_misaddressed.load();
        out.rx_duplicates = rx_duplicates.load();
        out.rx_incomplete = rx_incomplete.load();
        out.rx_queue_full = rx_queue_full.load();
        out.rx_reordered = rx_reordered.load();
        out.rx_seq_lost = rx_seq_lost.load();
        out.last_rx_ns = last_rx_ns.load();
    }
};

// Sequence bookkeeping of one sender over a window of the last 64 frame numbers: which frames the numbers say
// are missing, which arrived late, and which arrived twice (a duplicating link repeats datagrams).
class SeqTracker {
   public:
    // True when the frame number was already accepted (inside the window); such a frame is dropped.
    bool seen(uint32_t seq) const {
        if (!started_) {
            return false;
        }
        const int32_t delta = static_cast<int32_t>(seq - highest_);
        if (delta > 0) {
            return false;
        }
        const uint32_t age = static_cast<uint32_t>(-delta);
        return age < 64U && ((window_ >> age) & 1U) != 0U;
    }

    // Called once per accepted frame of the sender.
    void note(uint32_t seq, Counters& counters) {
        if (!started_) {
            started_ = true;
            highest_ = seq;
            window_ = 1U;
            return;
        }
        const int32_t delta = static_cast<int32_t>(seq - highest_);
        if (delta > 0) {
            counters.rx_seq_lost += static_cast<uint64_t>(delta - 1);
            window_ = delta >= 64 ? 0U : window_ << delta;
            window_ |= 1U;
            highest_ = seq;
            return;
        }
        // Late: it was counted as lost when a younger frame arrived.
        counters.rx_reordered += 1;
        const uint32_t age = static_cast<uint32_t>(-delta);
        if (age < 64U) {
            window_ |= uint64_t{1} << age;
            if (counters.rx_seq_lost.load() > 0) {
                counters.rx_seq_lost -= 1;
            }
        }
    }

   private:
    bool started_{false};
    uint32_t highest_{0};
    uint64_t window_{0};  // bit i: frame number highest_ - i was accepted
};

struct Peer {
    uint16_t id{0};
    std::string address;  // "host:port" for peer_link; unused by shm_bridge
};

// Entry validation of a frame taken from the tx port.
inline bool txFrameValid(const FrameType& frame, const std::vector<Peer>& peers, uint16_t self) {
    if (frame.len > kFrameCapacity) {
        return false;
    }
    if (frame.peer == XGC2_FRAME_BROADCAST) {
        return true;
    }
    if (frame.peer == self) {
        return false;
    }
    for (const Peer& peer : peers) {
        if (peer.id == frame.peer) {
            return true;
        }
    }
    return false;
}

// "self", "peers" of a link configuration; throws std::invalid_argument on a malformed table.
struct Endpoints {
    uint16_t self{0};
    std::vector<Peer> peers;
};

}  // namespace frame_link
}  // namespace sdk
}  // namespace xgc2
