#pragma once

// The datagram format of the frame links (peer_link) and the fragmentation of a frame into datagrams, with a
// bounded reassembler. Pure functions over byte buffers: no socket, no clock, no allocation after construction
// except the reassembly slots, so the whole format is testable without a network.
//
// Datagram, little-endian, 28 bytes of header (the payload follows):
//    0  2  magic 'X','G'
//    2  1  version (1)
//    3  1  flags (bit 0: the frame was sent to every peer)
//    4  2  src endpoint id
//    6  2  dst endpoint id (0xFFFF: broadcast)
//    8  4  frame_seq (per sender, +1 per frame; every fragment of a frame carries the same value)
//   12  2  kind (algorithm-owned, never interpreted)
//   14  2  total_len (data bytes of the whole frame)
//   16  1  frag_idx
//   17  1  frag_cnt (1..kMaxFragments)
//   18  2  frag_len (data bytes in this datagram)
//   20  8  t_tx_ns (time of the sender)
//
// A frame is dropped as a whole when one of its fragments is missing: there is no retransmission (the
// algorithms above are periodic and tolerate loss) and no partial delivery.

#include <cstddef>
#include <cstdint>
#include <cstring>
#include <vector>

namespace xgc2 {
namespace sdk {
namespace wire {

constexpr uint8_t kMagic0 = 'X';
constexpr uint8_t kMagic1 = 'G';
constexpr uint8_t kVersion = 1;
constexpr size_t kHeaderBytes = 28;
constexpr uint16_t kBroadcast = 0xFFFF;
constexpr uint8_t kFlagBroadcast = 0x1;
constexpr unsigned kMaxFragments = 8;
// A datagram carries at most this many bytes of UDP payload (header included): below the 1472 bytes an
// Ethernet MTU of 1500 leaves after IPv4 and UDP headers, so that no IP fragmentation ever happens.
constexpr size_t kDefaultDatagramBytes = 1400;
constexpr size_t kMinDatagramBytes = 512;
constexpr size_t kMaxDatagramBytes = 1472;

struct Header {
    uint8_t flags{0};
    uint16_t src{0};
    uint16_t dst{0};
    uint32_t frame_seq{0};
    uint16_t kind{0};
    uint16_t total_len{0};
    uint8_t frag_idx{0};
    uint8_t frag_cnt{1};
    uint16_t frag_len{0};
    int64_t t_tx_ns{0};
};

inline void put16(uint8_t* p, uint16_t v) {
    p[0] = static_cast<uint8_t>(v);
    p[1] = static_cast<uint8_t>(v >> 8);
}
inline void put32(uint8_t* p, uint32_t v) {
    for (int i = 0; i < 4; ++i) {
        p[i] = static_cast<uint8_t>(v >> (8 * i));
    }
}
inline void put64(uint8_t* p, uint64_t v) {
    for (int i = 0; i < 8; ++i) {
        p[i] = static_cast<uint8_t>(v >> (8 * i));
    }
}
inline uint16_t get16(const uint8_t* p) {
    return static_cast<uint16_t>(p[0] | (p[1] << 8));
}
inline uint32_t get32(const uint8_t* p) {
    return static_cast<uint32_t>(p[0]) | (static_cast<uint32_t>(p[1]) << 8) |
           (static_cast<uint32_t>(p[2]) << 16) | (static_cast<uint32_t>(p[3]) << 24);
}
inline uint64_t get64(const uint8_t* p) {
    uint64_t v = 0;
    for (int i = 7; i >= 0; --i) {
        v = (v << 8) | p[i];
    }
    return v;
}

// Bytes of frame data one datagram of `datagram_bytes` carries.
constexpr size_t fragmentData(size_t datagram_bytes) {
    return datagram_bytes - kHeaderBytes;
}

// Datagrams a frame of `len` data bytes needs; 0 when it would need more than kMaxFragments.
inline unsigned fragmentCount(size_t len, size_t datagram_bytes) {
    const size_t per = fragmentData(datagram_bytes);
    const size_t count = len == 0 ? 1 : (len + per - 1) / per;
    return count > kMaxFragments ? 0U : static_cast<unsigned>(count);
}

// Bytes on the UDP payload level for a frame of `len` data bytes: headers included.
inline size_t wireBytes(size_t len, size_t datagram_bytes) {
    const unsigned count = fragmentCount(len, datagram_bytes);
    return count == 0 ? 0 : len + count * kHeaderBytes;
}

inline void writeHeader(uint8_t* out, const Header& h) {
    out[0] = kMagic0;
    out[1] = kMagic1;
    out[2] = kVersion;
    out[3] = h.flags;
    put16(out + 4, h.src);
    put16(out + 6, h.dst);
    put32(out + 8, h.frame_seq);
    put16(out + 12, h.kind);
    put16(out + 14, h.total_len);
    out[16] = h.frag_idx;
    out[17] = h.frag_cnt;
    put16(out + 18, h.frag_len);
    put64(out + 20, static_cast<uint64_t>(h.t_tx_ns));
}

enum class Parse { kOk, kShort, kMagic, kVersion, kGeometry };

// Entry validation of one received datagram. kOk only when the header is consistent with the datagram.
inline Parse parseDatagram(const uint8_t* in, size_t size, Header& h, size_t frame_capacity) {
    if (size < kHeaderBytes) {
        return Parse::kShort;
    }
    if (in[0] != kMagic0 || in[1] != kMagic1) {
        return Parse::kMagic;
    }
    if (in[2] != kVersion) {
        return Parse::kVersion;
    }
    h.flags = in[3];
    h.src = get16(in + 4);
    h.dst = get16(in + 6);
    h.frame_seq = get32(in + 8);
    h.kind = get16(in + 12);
    h.total_len = get16(in + 14);
    h.frag_idx = in[16];
    h.frag_cnt = in[17];
    h.frag_len = get16(in + 18);
    h.t_tx_ns = static_cast<int64_t>(get64(in + 20));
    const bool ok = h.frag_cnt >= 1 && h.frag_cnt <= kMaxFragments && h.frag_idx < h.frag_cnt &&
                    h.total_len <= frame_capacity && size == kHeaderBytes + h.frag_len &&
                    h.frag_len <= h.total_len && (h.frag_cnt > 1 || h.frag_len == h.total_len) &&
                    (h.frag_cnt == 1 || h.frag_len > 0);
    return ok ? Parse::kOk : Parse::kGeometry;
}

// Splits a frame into datagrams and calls `emit(const uint8_t* datagram, size_t size)` for each, in order.
// Returns the number of datagrams, 0 when the frame is too large for kMaxFragments.
template <typename Emit>
unsigned fragment(const Header& frame, const uint8_t* data, size_t datagram_bytes, Emit&& emit) {
    const unsigned count = fragmentCount(frame.total_len, datagram_bytes);
    if (count == 0) {
        return 0;
    }
    const size_t per = fragmentData(datagram_bytes);
    uint8_t buffer[kMaxDatagramBytes];
    for (unsigned i = 0; i < count; ++i) {
        Header h = frame;
        h.frag_idx = static_cast<uint8_t>(i);
        h.frag_cnt = static_cast<uint8_t>(count);
        const size_t offset = i * per;
        h.frag_len = static_cast<uint16_t>(frame.total_len - offset < per ? frame.total_len - offset : per);
        writeHeader(buffer, h);
        if (h.frag_len > 0) {
            std::memcpy(buffer + kHeaderBytes, data + offset, h.frag_len);
        }
        emit(static_cast<const uint8_t*>(buffer), kHeaderBytes + h.frag_len);
    }
    return count;
}

// Bounded reassembly of the fragments of the frames from one sender. At most `slots` frames are in flight;
// a frame whose fragments do not all arrive within `timeout_ns` of its first one is dropped as a whole, and so
// is the oldest in-flight frame when a new one needs a slot.
class Reassembler {
   public:
    struct Frame {
        Header header;  // of fragment 0
        const uint8_t* data{nullptr};
    };
    struct Counters {
        uint64_t duplicates{0};
        uint64_t dropped_incomplete{0};  // frames dropped for a missing fragment
    };

    Reassembler(size_t slots, size_t frame_capacity, int64_t timeout_ns)
        : slots_(slots), capacity_(frame_capacity), timeout_ns_(timeout_ns) {
        for (Slot& s : slots_) {
            s.data.assign(frame_capacity, 0);
        }
    }

    // A single-fragment frame needs no slot; the caller handles it. Returns the completed frame (valid until
    // the next call), or a Frame with data == nullptr while fragments are missing or for a duplicate.
    Frame add(const Header& h, const uint8_t* fragment_data, int64_t now_ns) {
        expire(now_ns);
        Slot* slot = find(h);
        if (slot == nullptr) {
            slot = allocate();
            slot->used = true;
            slot->header = h;
            slot->first_ns = now_ns;
            slot->mask = 0;
        }
        const uint8_t bit = static_cast<uint8_t>(1U << h.frag_idx);
        if ((slot->mask & bit) != 0) {
            ++counters_.duplicates;
            return {};
        }
        // Every fragment but the last carries the same number of data bytes, so a fragment can be placed
        // whatever the arrival order.
        const size_t offset = static_cast<size_t>(h.frag_idx) * strideOf(h);
        if (offset + h.frag_len > capacity_) {
            slot->used = false;  // inconsistent geometry between the fragments of one frame
            ++counters_.dropped_incomplete;
            return {};
        }
        std::memcpy(slot->data.data() + offset, fragment_data, h.frag_len);
        slot->mask = static_cast<uint8_t>(slot->mask | bit);
        if (slot->mask == static_cast<uint8_t>((1U << h.frag_cnt) - 1U)) {
            slot->used = false;
            Frame done;
            done.header = slot->header;
            done.data = slot->data.data();
            return done;
        }
        return {};
    }

    // Drop the frames whose time is up. Called by add(); a receiver also calls it when idle.
    void expire(int64_t now_ns) {
        for (Slot& s : slots_) {
            if (s.used && now_ns - s.first_ns >= timeout_ns_) {
                s.used = false;
                ++counters_.dropped_incomplete;
            }
        }
    }

    Counters counters() const {
        return counters_;
    }
    size_t inFlight() const {
        size_t n = 0;
        for (const Slot& s : slots_) {
            n += s.used ? 1U : 0U;
        }
        return n;
    }

   private:
    struct Slot {
        bool used{false};
        Header header;
        int64_t first_ns{0};
        uint8_t mask{0};
        std::vector<uint8_t> data;
    };

    // Every fragment but the last carries the same number of data bytes; the last carries the rest.
    static size_t strideOf(const Header& h) {
        if (h.frag_cnt == 1) {
            return h.total_len;
        }
        if (h.frag_idx + 1U < h.frag_cnt) {
            return h.frag_len;
        }
        // The last fragment: total = (cnt - 1) * stride + frag_len.
        return (static_cast<size_t>(h.total_len) - h.frag_len) / (h.frag_cnt - 1U);
    }

    Slot* find(const Header& h) {
        for (Slot& s : slots_) {
            if (s.used && s.header.frame_seq == h.frame_seq && s.header.total_len == h.total_len &&
                s.header.frag_cnt == h.frag_cnt) {
                return &s;
            }
        }
        return nullptr;
    }
    Slot* allocate() {
        Slot* oldest = &slots_[0];
        for (Slot& s : slots_) {
            if (!s.used) {
                return &s;
            }
            if (s.first_ns < oldest->first_ns) {
                oldest = &s;
            }
        }
        ++counters_.dropped_incomplete;  // evicted before it completed
        return oldest;
    }

    std::vector<Slot> slots_;
    size_t capacity_;
    int64_t timeout_ns_;
    Counters counters_;
};

}  // namespace wire
}  // namespace sdk
}  // namespace xgc2
