// shm_bridge: the frame link between module-host processes of one machine (the shared-memory edge of a topology).
// Same ports, addressing and counters as peer_link (see ../common/link_module.hpp); the transport is one POSIX
// shared-memory segment with one bounded inbox ring per endpoint. Any process writes to any inbox (multi-producer,
// per-slot sequence numbers), only the owner of an endpoint reads its inbox. A full inbox drops the frame and counts
// it: a writer never waits for a slow reader.
//
// Configuration (JSON):
//   "shm_name": "/xgc2-s1-planner"   POSIX shm object name (one slash, then [A-Za-z0-9._-])
//   "self": 0                        this endpoint's id, 0..endpoints-1
//   "endpoints": 4                   number of inboxes (1..64)
//   "slots": 64                      slots per inbox, a power of two (4..4096)
//   "owner": false                   true: this instance creates the segment (recreating a stale one) and unlinks it at stop
//   "attach_timeout_ms": 2000        a non-owner waits this long for the owner to publish the segment
//   "stats_period_ms": 1000

#include <fcntl.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <time.h>
#include <unistd.h>
#include <xgc2/frame.h>
#include <xgc2/module.h>
#include <xgc2/sdk/host.hpp>
#include <xgc2/sdk/json_config.hpp>

#include <algorithm>
#include <atomic>
#include <cctype>
#include <cerrno>
#include <chrono>
#include <cstring>
#include <map>
#include <memory>
#include <string>
#include <thread>

#include "../common/link_module.hpp"

namespace {

using namespace xgc2::sdk;
namespace fl = xgc2::sdk::frame_link;

constexpr uint32_t kMagic = 0x4D485358;  // 'XSHM'
constexpr uint32_t kVersion = 1;

struct SegmentHeader {
    uint32_t magic;
    uint32_t version;
    uint32_t endpoints;
    uint32_t slots;
    uint32_t slot_bytes;
    std::atomic<uint32_t> ready;
    uint8_t pad[40];
};
static_assert(sizeof(SegmentHeader) == 64, "segment header layout");

struct Slot64 {
    std::atomic<uint64_t> seq;
    uint16_t src;
    uint16_t flags;
    uint32_t pad;
    fl::FrameType frame;
};

struct RingHeader {
    alignas(64) std::atomic<uint64_t> tail;  // next ticket of the producers
    alignas(64) std::atomic<uint64_t> head;  // next slot of the consumer
};

size_t ringBytes(uint32_t slots) {
    return sizeof(RingHeader) + static_cast<size_t>(slots) * sizeof(Slot64);
}
size_t segmentBytes(uint32_t endpoints, uint32_t slots) {
    return sizeof(SegmentHeader) + static_cast<size_t>(endpoints) * ringBytes(slots);
}

struct Settings {
    std::string name;
    uint16_t self{0};
    uint32_t endpoints{0};
    uint32_t slots{64};
    bool owner{false};
    int64_t attach_timeout_ms{2000};
    int64_t stats_period_ns{1'000'000'000};
};

Settings readSettings(const xgc2_config* config) {
    JsonConfig json(config);
    Settings s;
    if (!json.get("shm_name", s.name) || s.name.size() < 2 || s.name.size() > 200 || s.name[0] != '/' ||
        s.name.find('/', 1) != std::string::npos) {
        throw std::invalid_argument("'shm_name' must be one slash and a name, such as /xgc2-s1-planner");
    }
    for (char c : s.name.substr(1)) {
        if (!(std::isalnum(static_cast<unsigned char>(c)) || c == '.' || c == '_' || c == '-')) {
            throw std::invalid_argument("'shm_name' has a character outside [A-Za-z0-9._-]");
        }
    }
    uint32_t self = 0;
    if (!json.get("self", self) || !json.get("endpoints", s.endpoints) || s.endpoints < 1 || s.endpoints > 64 ||
        self >= s.endpoints) {
        throw std::invalid_argument("'endpoints' must be 1..64 and 'self' one of them");
    }
    s.self = static_cast<uint16_t>(self);
    json.get("slots", s.slots);
    if (s.slots < 4 || s.slots > 4096 || (s.slots & (s.slots - 1U)) != 0U) {
        throw std::invalid_argument("'slots' must be a power of two, 4..4096");
    }
    json.get("owner", s.owner);
    uint32_t attach = 2000;
    json.get("attach_timeout_ms", attach);
    s.attach_timeout_ms = attach;
    uint32_t stats_ms = 1000;
    json.get("stats_period_ms", stats_ms);
    s.stats_period_ns = static_cast<int64_t>(stats_ms) * 1'000'000;
    json.rejectUnused();
    return s;
}

class Segment {
   public:
    ~Segment() {
        close();
    }

    // Owner: create (recreating a stale segment), initialize, publish. Others: wait for the owner.
    void open(const Settings& s) {
        settings_ = s;
        const size_t bytes = segmentBytes(s.endpoints, s.slots);
        if (s.owner) {
            ::shm_unlink(s.name.c_str());  // a stale segment of a crashed session
            fd_ = ::shm_open(s.name.c_str(), O_CREAT | O_EXCL | O_RDWR | O_CLOEXEC, 0600);
            if (fd_ < 0 || ::ftruncate(fd_, static_cast<off_t>(bytes)) != 0) {
                throw std::runtime_error("create " + s.name + ": " + std::strerror(errno));
            }
            map(bytes);
            SegmentHeader* h = header();
            h->magic = kMagic;
            h->version = kVersion;
            h->endpoints = s.endpoints;
            h->slots = s.slots;
            h->slot_bytes = static_cast<uint32_t>(sizeof(Slot64));
            for (uint32_t e = 0; e < s.endpoints; ++e) {
                RingHeader* r = ring(e);
                r->tail.store(0);
                r->head.store(0);
                for (uint32_t i = 0; i < s.slots; ++i) {
                    slot(e, i)->seq.store(i);
                }
            }
            h->ready.store(1, std::memory_order_release);
            created_ = true;
            return;
        }
        const auto deadline = std::chrono::steady_clock::now() + std::chrono::milliseconds(s.attach_timeout_ms);
        for (;;) {
            fd_ = ::shm_open(s.name.c_str(), O_RDWR | O_CLOEXEC, 0);
            struct stat st {};
            if (fd_ >= 0 && ::fstat(fd_, &st) == 0 && static_cast<size_t>(st.st_size) >= bytes) {
                map(bytes);
                if (header()->ready.load(std::memory_order_acquire) == 1U) {
                    break;
                }
                unmap();
            }
            if (fd_ >= 0) {
                ::close(fd_);
                fd_ = -1;
            }
            if (std::chrono::steady_clock::now() >= deadline) {
                throw std::runtime_error("the owner of " + s.name + " did not publish the segment in time");
            }
            std::this_thread::sleep_for(std::chrono::milliseconds(5));
        }
        // Entry validation of what another process wrote.
        const SegmentHeader* h = header();
        if (h->magic != kMagic || h->version != kVersion || h->endpoints != s.endpoints || h->slots != s.slots ||
            h->slot_bytes != sizeof(Slot64)) {
            close();
            throw std::runtime_error("the segment " + s.name + " has another geometry than the configuration");
        }
    }

    void close() {
        unmap();
        if (fd_ >= 0) {
            ::close(fd_);
            fd_ = -1;
        }
        if (created_) {
            ::shm_unlink(settings_.name.c_str());
            created_ = false;
        }
    }

    // False when the inbox of `to` is full.
    bool push(uint16_t to, uint16_t src, uint16_t flags, const fl::FrameType& frame) {
        RingHeader* r = ring(to);
        const uint32_t mask = settings_.slots - 1U;
        uint64_t pos = r->tail.load(std::memory_order_relaxed);
        Slot64* cell = nullptr;
        for (;;) {
            cell = slot(to, static_cast<uint32_t>(pos & mask));
            const uint64_t seq = cell->seq.load(std::memory_order_acquire);
            const int64_t diff = static_cast<int64_t>(seq) - static_cast<int64_t>(pos);
            if (diff == 0) {
                if (r->tail.compare_exchange_weak(pos, pos + 1, std::memory_order_relaxed)) {
                    break;
                }
            } else if (diff < 0) {
                return false;
            } else {
                pos = r->tail.load(std::memory_order_relaxed);
            }
        }
        cell->src = src;
        cell->flags = flags;
        const size_t used = XGC2_FRAME_HEADER_BYTES + frame.len;
        std::memcpy(&cell->frame, &frame, used);
        cell->seq.store(pos + 1, std::memory_order_release);
        return true;
    }

    // The oldest frame of this endpoint's inbox, or false when it is empty. `out` gets header and data.
    bool pop(uint16_t& src, uint16_t& flags, fl::FrameType& out) {
        RingHeader* r = ring(settings_.self);
        const uint32_t mask = settings_.slots - 1U;
        const uint64_t pos = r->head.load(std::memory_order_relaxed);
        Slot64* cell = slot(settings_.self, static_cast<uint32_t>(pos & mask));
        const uint64_t seq = cell->seq.load(std::memory_order_acquire);
        if (static_cast<int64_t>(seq) - static_cast<int64_t>(pos + 1) != 0) {
            return false;
        }
        src = cell->src;
        flags = cell->flags;
        const size_t used = XGC2_FRAME_HEADER_BYTES + std::min<size_t>(cell->frame.len, fl::kFrameCapacity);
        std::memcpy(&out, &cell->frame, used);
        cell->seq.store(pos + mask + 1U, std::memory_order_release);
        r->head.store(pos + 1, std::memory_order_relaxed);
        return true;
    }

   private:
    void map(size_t bytes) {
        void* p = ::mmap(nullptr, bytes, PROT_READ | PROT_WRITE, MAP_SHARED, fd_, 0);
        if (p == MAP_FAILED) {
            throw std::runtime_error(std::string("mmap: ") + std::strerror(errno));
        }
        base_ = static_cast<uint8_t*>(p);
        bytes_ = bytes;
    }
    void unmap() {
        if (base_ != nullptr) {
            ::munmap(base_, bytes_);
            base_ = nullptr;
        }
    }
    SegmentHeader* header() const {
        return reinterpret_cast<SegmentHeader*>(base_);
    }
    RingHeader* ring(uint32_t e) const {
        return reinterpret_cast<RingHeader*>(base_ + sizeof(SegmentHeader) + e * ringBytes(settings_.slots));
    }
    Slot64* slot(uint32_t e, uint32_t i) const {
        return reinterpret_cast<Slot64*>(reinterpret_cast<uint8_t*>(ring(e)) + sizeof(RingHeader)) + i;
    }

    Settings settings_;
    int fd_{-1};
    uint8_t* base_{nullptr};
    size_t bytes_{0};
    bool created_{false};
};

class ShmBridge {
   public:
    explicit ShmBridge(const Host& host) : host_(host) {}
    ~ShmBridge() {
        stop();
    }

    const Host& host() const {
        return host_;
    }

    void configure(const xgc2_config* config) {
        Settings next = readSettings(config);
        if (running_.load() && (next.name != settings_.name || next.self != settings_.self ||
                                next.endpoints != settings_.endpoints || next.slots != settings_.slots)) {
            throw std::invalid_argument("the segment and the endpoint cannot change while the link runs");
        }
        settings_ = next;
        if (running_.load()) {
            host_.setPeriodNs(settings_.stats_period_ns);
        }
    }

    void start() {
        segment_.open(settings_);
        stop_.store(false);
        running_.store(true);
        receiver_ = std::thread([this] { receiveLoop(); });
        host_.setPeriodNs(settings_.stats_period_ns);
    }

    void step(const xgc2_step_ctx& ctx) {
        if ((ctx.reasons & XGC2_STEP_INPUT) != 0U) {
            sendPending();
        }
        if ((ctx.reasons & XGC2_STEP_TIMER) != 0U) {
            publishStats(ctx.now_ns);
        }
    }

    void stop() {
        stop_.store(true);
        if (receiver_.joinable()) {
            receiver_.join();
        }
        if (running_.exchange(false)) {
            host_.setPeriodNs(0);
        }
        segment_.close();
    }

   private:
    void sendPending() {
        for (;;) {
            Sample<fl::FrameType> sample = host_.next<fl::FrameType>(fl::kTx);
            if (!sample) {
                return;
            }
            const fl::FrameType& frame = *sample;
            const bool broadcast = frame.peer == XGC2_FRAME_BROADCAST;
            if (frame.len > fl::kFrameCapacity || (!broadcast && (frame.peer >= settings_.endpoints || frame.peer == settings_.self))) {
                ++counters_.tx_invalid;
                continue;
            }
            const uint16_t flags = broadcast ? XGC2_FRAME_FLAG_BROADCAST : 0;
            bool all = true;
            for (uint32_t to = 0; to < settings_.endpoints; ++to) {
                if (to == settings_.self || (!broadcast && to != frame.peer)) {
                    continue;
                }
                if (segment_.push(static_cast<uint16_t>(to), settings_.self, flags, frame)) {
                    ++counters_.tx_datagrams;
                    counters_.tx_bytes += XGC2_FRAME_HEADER_BYTES + frame.len;
                } else {
                    ++counters_.tx_would_block;
                    all = false;
                }
            }
            if (all) {
                ++counters_.tx_frames;
            }
        }
    }

    void receiveLoop() {
        fl::FrameType frame;
        uint16_t src = 0;
        uint16_t flags = 0;
        int idle = 0;
        while (!stop_.load()) {
            if (!segment_.pop(src, flags, frame)) {
                // Spin briefly, then sleep: the latency of a handoff stays in microseconds under load and the
                // idle link costs almost nothing.
                if (++idle > 200) {
                    ::timespec ts{0, 100'000};
                    ::nanosleep(&ts, nullptr);
                }
                continue;
            }
            idle = 0;
            ++counters_.rx_datagrams;
            counters_.rx_bytes += XGC2_FRAME_HEADER_BYTES + frame.len;
            if (frame.len > fl::kFrameCapacity || src >= settings_.endpoints) {
                ++counters_.rx_malformed;
                continue;
            }
            Slot<fl::FrameType> out = host_.write<fl::FrameType>(fl::kRx);
            if (!out) {
                ++counters_.rx_queue_full;
                continue;
            }
            std::memcpy(&*out, &frame, XGC2_FRAME_HEADER_BYTES + frame.len);
            out->peer = src;
            out->flags = flags;
            const int64_t now = host_.nowNs();
            if (out.commit(now)) {
                ++counters_.rx_frames;
                counters_.last_rx_ns = now;
            }
        }
    }

    void publishStats(int64_t now_ns) {
        Slot<xgc2_link_stats> slot = host_.write<xgc2_link_stats>(fl::kStats);
        if (!slot) {
            return;
        }
        counters_.snapshot(*slot);
        slot.commit(now_ns);
    }

    Host host_;
    Settings settings_;
    Segment segment_;
    std::thread receiver_;
    std::atomic<bool> running_{false};
    std::atomic<bool> stop_{false};
    fl::Counters counters_;
};

const xgc2_module_desc kDesc = {XGC2_MODULE_ABI_MAJOR,
                                XGC2_MODULE_ABI_MINOR,
                                "xgc2_shm_bridge",
                                "1.0.0",
                                nullptr,
                                fl::kPortCount,
                                &Lifecycle<ShmBridge>::create,
                                &Lifecycle<ShmBridge>::configure,
                                &Lifecycle<ShmBridge>::start,
                                &Lifecycle<ShmBridge>::step,
                                &Lifecycle<ShmBridge>::stop,
                                &Lifecycle<ShmBridge>::destroy};

}  // namespace

XGC2_MODULE_EXPORT const xgc2_module_desc* xgc2_module_entry(void) {
    static xgc2_module_desc desc = kDesc;
    desc.ports = fl::ports();
    return &desc;
}
