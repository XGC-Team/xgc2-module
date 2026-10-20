// peer_link: the UDP frame link between the entities of an algorithm that runs on several hosts (the radio edge of
// a topology). Frames are the algorithm's own messages; the module adds the endpoint addressing and, for frames
// larger than one datagram, a bounded fragmentation (no TCP: a frame with a missing fragment is dropped as a whole
// and counted). It is a module of the host, not part of it: the host stays without a network stack.
//
// Configuration (JSON):
//   "self": 3                                  this entity's endpoint id (0..65534)
//   "bind": "0.0.0.0:47103"                    local UDP address
//   "peers": {"1": "10.0.0.11:47101", ...}     endpoint id -> UDP address of every other entity
//   "datagram_bytes": 1400                     UDP payload per datagram, 512..1472 (below the path MTU)
//   "reassembly_timeout_ms": 100               a frame missing a fragment this long is dropped
//   "reassembly_slots": 8                      frames in flight per sender
//   "stats_period_ms": 1000                    period of the stats port (0: only on request)
//
// Threads: the instance's step thread sends; one module thread receives and writes the rx port.

#include <arpa/inet.h>
#include <fcntl.h>
#include <netinet/in.h>
#include <poll.h>
#include <sys/socket.h>
#include <unistd.h>
#include <xgc2/frame.h>
#include <xgc2/module.h>
#include <xgc2/sdk/host.hpp>
#include <xgc2/sdk/json_config.hpp>

#include <atomic>
#include <cerrno>
#include <cstdlib>
#include <map>
#include <memory>
#include <mutex>
#include <thread>

#include "../common/frame_wire.hpp"
#include "../common/link_module.hpp"

namespace {

using namespace xgc2::sdk;
namespace fl = xgc2::sdk::frame_link;
namespace wire = xgc2::sdk::wire;

struct Address {
    sockaddr_in in{};
};

bool parseAddress(const std::string& text, Address& out) {
    const size_t colon = text.rfind(':');
    if (colon == std::string::npos) {
        return false;
    }
    const std::string host = text.substr(0, colon);
    char* end = nullptr;
    const long port = std::strtol(text.c_str() + colon + 1, &end, 10);
    if (end == text.c_str() + colon + 1 || *end != '\0' || port < 0 || port > 65535) {
        return false;
    }
    out.in = sockaddr_in{};
    out.in.sin_family = AF_INET;
    out.in.sin_port = htons(static_cast<uint16_t>(port));
    return inet_pton(AF_INET, host.c_str(), &out.in.sin_addr) == 1;
}

struct Settings {
    uint16_t self{0};
    Address bind;
    std::string bind_text;
    std::map<uint16_t, Address> peers;
    std::vector<fl::Peer> peer_list;
    size_t datagram_bytes{wire::kDefaultDatagramBytes};
    int64_t reassembly_timeout_ns{100'000'000};
    size_t reassembly_slots{8};
    int64_t stats_period_ns{1'000'000'000};
};

Settings readSettings(const xgc2_config* config) {
    JsonConfig json(config);
    Settings s;
    uint32_t self = 0;
    if (!json.get("self", self) || self >= wire::kBroadcast) {
        throw std::invalid_argument("'self' must be an endpoint id 0..65534");
    }
    s.self = static_cast<uint16_t>(self);
    if (!json.get("bind", s.bind_text) || !parseAddress(s.bind_text, s.bind)) {
        throw std::invalid_argument("'bind' must be an IPv4 address and port such as 0.0.0.0:47103");
    }
    for (const std::string& key : json.keys("peers")) {
        char* end = nullptr;
        const long id = std::strtol(key.c_str(), &end, 10);
        std::string text;
        Address address;
        if (key.empty() || *end != '\0' || id < 0 || id >= wire::kBroadcast || id == s.self) {
            throw std::invalid_argument("peer id '" + key + "' must be an endpoint id other than 'self'");
        }
        if (!json.get("peers/" + key, text) || !parseAddress(text, address) || ntohs(address.in.sin_port) == 0) {
            throw std::invalid_argument("peer '" + key + "' needs an IPv4 address and port");
        }
        s.peers[static_cast<uint16_t>(id)] = address;
        s.peer_list.push_back({static_cast<uint16_t>(id), text});
    }
    uint32_t datagram = wire::kDefaultDatagramBytes;
    json.get("datagram_bytes", datagram);
    if (datagram < wire::kMinDatagramBytes || datagram > wire::kMaxDatagramBytes) {
        throw std::invalid_argument("'datagram_bytes' must be 512..1472");
    }
    s.datagram_bytes = datagram;
    if (wire::fragmentCount(fl::kFrameCapacity, s.datagram_bytes) == 0) {
        throw std::invalid_argument("'datagram_bytes' is too small for the frame class (more than 8 fragments)");
    }
    uint32_t timeout_ms = 100;
    json.get("reassembly_timeout_ms", timeout_ms);
    if (timeout_ms == 0 || timeout_ms > 60000) {
        throw std::invalid_argument("'reassembly_timeout_ms' must be 1..60000");
    }
    s.reassembly_timeout_ns = static_cast<int64_t>(timeout_ms) * 1'000'000;
    uint32_t slots = 8;
    json.get("reassembly_slots", slots);
    if (slots == 0 || slots > 64) {
        throw std::invalid_argument("'reassembly_slots' must be 1..64");
    }
    s.reassembly_slots = slots;
    uint32_t stats_ms = 1000;
    json.get("stats_period_ms", stats_ms);
    s.stats_period_ns = static_cast<int64_t>(stats_ms) * 1'000'000;
    json.rejectUnused();
    return s;
}

class PeerLink {
   public:
    explicit PeerLink(const Host& host) : host_(host) {}
    ~PeerLink() {
        closeSocket();
    }

    const Host& host() const {
        return host_;
    }

    void configure(const xgc2_config* config) {
        auto next = std::make_shared<const Settings>(readSettings(config));
        std::lock_guard<std::mutex> lock(settings_mutex_);
        if (settings_ && running_.load() && (next->self != settings_->self || next->bind_text != settings_->bind_text)) {
            throw std::invalid_argument("'self' and 'bind' cannot change while the link runs");
        }
        settings_ = next;
        if (running_.load()) {
            host_.setPeriodNs(next->stats_period_ns);
        }
    }

    void start() {
        const std::shared_ptr<const Settings> settings_ptr = current();
        const Settings& settings = *settings_ptr;
        socket_ = ::socket(AF_INET, SOCK_DGRAM | SOCK_NONBLOCK | SOCK_CLOEXEC, 0);
        if (socket_ < 0) {
            throw std::runtime_error(std::string("socket: ") + std::strerror(errno));
        }
        const int buffer = 1 << 20;
        ::setsockopt(socket_, SOL_SOCKET, SO_RCVBUF, &buffer, sizeof buffer);
        ::setsockopt(socket_, SOL_SOCKET, SO_SNDBUF, &buffer, sizeof buffer);
        if (::bind(socket_, reinterpret_cast<const sockaddr*>(&settings.bind.in), sizeof settings.bind.in) != 0) {
            const std::string why = std::strerror(errno);
            closeSocket();
            throw std::runtime_error("bind " + settings.bind_text + ": " + why);
        }
        running_.store(true);
        stop_.store(false);
        receiver_ = std::thread([this] { receiveLoop(); });
        host_.setPeriodNs(settings.stats_period_ns);
    }

    void step(const xgc2_step_ctx& ctx) {
        if ((ctx.reasons & XGC2_STEP_INPUT) != 0U) {
            sendPending(ctx.now_ns);
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
        running_.store(false);
        host_.setPeriodNs(0);
        closeSocket();
    }

   private:
    std::shared_ptr<const Settings> current() {
        std::lock_guard<std::mutex> lock(settings_mutex_);
        return settings_;
    }

    void closeSocket() {
        if (socket_ >= 0) {
            ::close(socket_);
            socket_ = -1;
        }
    }

    void sendPending(int64_t now_ns) {
        const std::shared_ptr<const Settings> settings_ptr = current();
        const Settings& settings = *settings_ptr;
        for (;;) {
            Sample<fl::FrameType> sample = host_.next<fl::FrameType>(fl::kTx);
            if (!sample) {
                return;
            }
            const fl::FrameType& frame = *sample;
            if (!fl::txFrameValid(frame, settings.peer_list, settings.self)) {
                ++counters_.tx_invalid;
                continue;
            }
            wire::Header header;
            header.src = settings.self;
            header.kind = frame.kind;
            header.total_len = frame.len;
            header.t_tx_ns = frame.t_tx_ns != 0 ? frame.t_tx_ns : now_ns;
            header.frame_seq = next_seq_++;
            const bool broadcast = frame.peer == XGC2_FRAME_BROADCAST;
            header.flags = broadcast ? wire::kFlagBroadcast : 0;
            header.dst = frame.peer;
            if (wire::fragmentCount(frame.len, settings.datagram_bytes) == 0) {
                ++counters_.tx_invalid;
                continue;
            }
            for (const auto& entry : settings.peers) {
                if (!broadcast && entry.first != frame.peer) {
                    continue;
                }
                sendFrame(entry.second, header, frame.data, settings.datagram_bytes);
            }
        }
    }

    void sendFrame(const Address& to, const wire::Header& header, const uint8_t* data, size_t datagram_bytes) {
        bool sent_all = true;
        wire::fragment(header, data, datagram_bytes, [&](const uint8_t* datagram, size_t size) {
            const ssize_t n = ::sendto(socket_, datagram, size, 0, reinterpret_cast<const sockaddr*>(&to.in), sizeof to.in);
            if (n == static_cast<ssize_t>(size)) {
                ++counters_.tx_datagrams;
                counters_.tx_bytes += size;
            } else if (n < 0 && (errno == EAGAIN || errno == EWOULDBLOCK || errno == ENOBUFS)) {
                ++counters_.tx_would_block;
                sent_all = false;
            } else {
                ++counters_.tx_errors;
                sent_all = false;
            }
        });
        if (sent_all) {
            ++counters_.tx_frames;
        }
    }

    void publishStats(int64_t now_ns) {
        Slot<xgc2_link_stats> slot = host_.write<xgc2_link_stats>(fl::kStats);
        if (!slot) {
            return;
        }
        counters_.snapshot(*slot);
        slot.commit(now_ns);
        const uint64_t errors = counters_.tx_errors.load() + counters_.rx_queue_full.load();
        host_.report(errors > reported_errors_ ? kHealthDegraded : kHealthOk,
                     format("{\"tx_frames\":%llu,\"rx_frames\":%llu,\"rx_incomplete\":%llu,\"tx_would_block\":%llu}",
                            static_cast<unsigned long long>(counters_.tx_frames.load()),
                            static_cast<unsigned long long>(counters_.rx_frames.load()),
                            static_cast<unsigned long long>(counters_.rx_incomplete.load()),
                            static_cast<unsigned long long>(counters_.tx_would_block.load())));
        reported_errors_ = errors;
    }

    void receiveLoop() {
        const std::shared_ptr<const Settings> settings_ptr = current();
        const Settings& settings = *settings_ptr;
        std::map<uint16_t, wire::Reassembler> reassemblers;
        std::map<uint16_t, fl::SeqTracker> trackers;
        std::map<uint16_t, uint64_t> counted_incomplete;
        std::vector<uint8_t> buffer(2048);
        while (!stop_.load()) {
            pollfd fd{socket_, POLLIN, 0};
            const int ready = ::poll(&fd, 1, 10);
            const int64_t now_ns = host_.nowNs();
            if (ready > 0 && (fd.revents & POLLIN) != 0) {
                for (;;) {
                    const ssize_t n = ::recv(socket_, buffer.data(), buffer.size(), 0);
                    if (n < 0) {
                        break;
                    }
                    handleDatagram(settings, reassemblers, trackers, buffer.data(), static_cast<size_t>(n), now_ns);
                }
            }
            for (auto& entry : reassemblers) {
                entry.second.expire(now_ns);
                const uint64_t total = entry.second.counters().dropped_incomplete;
                uint64_t& seen = counted_incomplete[entry.first];
                counters_.rx_incomplete += total - seen;
                seen = total;
            }
        }
    }

    void handleDatagram(const Settings& settings, std::map<uint16_t, wire::Reassembler>& reassemblers,
                        std::map<uint16_t, fl::SeqTracker>& trackers, const uint8_t* data, size_t size,
                        int64_t now_ns) {
        ++counters_.rx_datagrams;
        counters_.rx_bytes += size;
        wire::Header header;
        if (wire::parseDatagram(data, size, header, fl::kFrameCapacity) != wire::Parse::kOk) {
            ++counters_.rx_malformed;
            return;
        }
        if (settings.peers.find(header.src) == settings.peers.end()) {
            ++counters_.rx_unknown_peer;
            return;
        }
        if (header.dst != settings.self && header.dst != wire::kBroadcast) {
            ++counters_.rx_misaddressed;
            return;
        }
        fl::SeqTracker& tracker = trackers[header.src];
        if (tracker.seen(header.frame_seq)) {
            ++counters_.rx_duplicates;  // a link that repeats datagrams must not repeat frames
            return;
        }
        const uint8_t* fragment = data + wire::kHeaderBytes;
        if (header.frag_cnt == 1) {
            deliver(header, fragment, tracker, now_ns);
            return;
        }
        auto found = reassemblers.find(header.src);
        if (found == reassemblers.end()) {
            found = reassemblers
                        .emplace(header.src, wire::Reassembler(settings.reassembly_slots, fl::kFrameCapacity,
                                                               settings.reassembly_timeout_ns))
                        .first;
        }
        const uint64_t dup_before = found->second.counters().duplicates;
        wire::Reassembler::Frame done = found->second.add(header, fragment, now_ns);
        counters_.rx_duplicates += found->second.counters().duplicates - dup_before;
        if (done.data != nullptr) {
            deliver(done.header, done.data, tracker, now_ns);
        }
    }

    void deliver(const wire::Header& header, const uint8_t* data, fl::SeqTracker& tracker, int64_t now_ns) {
        tracker.note(header.frame_seq, counters_);
        Slot<fl::FrameType> slot = host_.write<fl::FrameType>(fl::kRx);
        if (!slot) {
            ++counters_.rx_queue_full;
            return;
        }
        slot->peer = header.src;
        slot->kind = header.kind;
        slot->len = header.total_len;
        slot->flags = (header.flags & wire::kFlagBroadcast) != 0 ? XGC2_FRAME_FLAG_BROADCAST : 0;
        slot->t_tx_ns = header.t_tx_ns;
        if (header.total_len > 0) {
            std::memcpy(slot->data, data, header.total_len);
        }
        if (slot.commit(now_ns)) {
            ++counters_.rx_frames;
            counters_.last_rx_ns = now_ns;
        }
    }

    Host host_;
    std::shared_ptr<const Settings> settings_;
    std::mutex settings_mutex_;
    int socket_{-1};
    std::thread receiver_;
    std::atomic<bool> running_{false};
    std::atomic<bool> stop_{false};
    fl::Counters counters_;
    uint32_t next_seq_{0};
    uint64_t reported_errors_{0};
};

}  // namespace

namespace {
// Lifecycle<> calls configure() for the first configuration and for live ones.
const xgc2_module_desc kDesc = {XGC2_MODULE_ABI_MAJOR,
                                XGC2_MODULE_ABI_MINOR,
                                "xgc2_peer_link",
                                "1.0.0",
                                nullptr,
                                fl::kPortCount,
                                &Lifecycle<PeerLink>::create,
                                &Lifecycle<PeerLink>::configure,
                                &Lifecycle<PeerLink>::start,
                                &Lifecycle<PeerLink>::step,
                                &Lifecycle<PeerLink>::stop,
                                &Lifecycle<PeerLink>::destroy};
}  // namespace

XGC2_MODULE_EXPORT const xgc2_module_desc* xgc2_module_entry(void) {
    static xgc2_module_desc desc = kDesc;
    desc.ports = fl::ports();
    return &desc;
}
