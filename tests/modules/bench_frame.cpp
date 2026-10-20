// Handoff benchmark module: role "producer" writes a frame of `bytes` at the period the host gives it, stamped with the
// monotonic clock; role "sink" reads the frames and keeps the latency (sink step time minus the stamp) of every one.
// The sink reports percentiles in its health detail. Used by benches/frame_handoff.rs to compare the module handoff of a
// 1-2 KB message with a ROS1 hop of the same size (program/C9/scratch/ros_hop_bench.cpp).
#include <time.h>

#include <algorithm>
#include <cstdint>
#include <cstring>
#include <vector>

#include <xgc2/frame.h>
#include <xgc2/sdk/frame.hpp>
#include <xgc2/sdk/host.hpp>

#include "common.h"

namespace {

using namespace xgc2::sdk;
using Frame = xgc2_frame2k;

const xgc2_port_desc kPorts[] = {
    port<Frame>("in", XGC2_PORT_IN, XGC2_PORT_EVENT, XGC2_FRAME2K_SCHEMA, 16),
    port<Frame>("out", XGC2_PORT_OUT, XGC2_PORT_EVENT, XGC2_FRAME2K_SCHEMA, 16),
};

int64_t monotonicNs() {
    timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return static_cast<int64_t>(ts.tv_sec) * 1000000000LL + ts.tv_nsec;
}

class Bench {
   public:
    explicit Bench(const Host& host) : host_(host) {}
    const Host& host() const {
        return host_;
    }
    void configure(const xgc2_config* config) {
        char role[16];
        cfg_string(config, "role", role, sizeof role);
        producer_ = std::strcmp(role, "producer") == 0;
        bytes_ = static_cast<uint16_t>(cfg_number(config, "bytes", 1500));
        period_ns_ = static_cast<int64_t>(cfg_number(config, "period_us", 2000) * 1000.0);
        if (bytes_ > sizeof(Frame::data)) {
            throw std::invalid_argument("bytes does not fit a frame");
        }
        latencies_.reserve(200000);
    }
    void start() {
        if (producer_) {
            host_.setPeriodNs(period_ns_);
        }
    }
    void step(const xgc2_step_ctx&) {
        if (producer_) {
            Slot<Frame> slot = host_.write<Frame>(1);
            if (!slot) {
                ++refused_;
                return;
            }
            slot->len = bytes_;
            slot->peer = 0;
            slot->kind = 1;
            slot->t_tx_ns = monotonicNs();
            std::memset(slot->data, static_cast<int>(sent_ & 0xff), bytes_);
            ++sent_;
            slot.commit(host_.nowNs());
            return;
        }
        for (;;) {
            Sample<Frame> sample = host_.next<Frame>(0);
            if (!sample) {
                break;
            }
            latencies_.push_back(monotonicNs() - sample->t_tx_ns);
            checksum_ += sample->data[0] + sample->len;
        }
        if (++steps_ % 64 == 0) {
            report();
        }
    }
    void stop() {
        if (!producer_) {
            report();
        }
    }

   private:
    void report() {
        std::vector<int64_t> sorted(latencies_);
        std::sort(sorted.begin(), sorted.end());
        auto at = [&](double p) -> long long {
            if (sorted.empty()) return 0;
            size_t rank = static_cast<size_t>(p * static_cast<double>(sorted.size()) + 0.999999);
            rank = std::min(std::max<size_t>(rank, 1), sorted.size());
            return sorted[rank - 1];
        };
        host_.report(kHealthOk, format("{\"count\":%zu,\"p50_ns\":%lld,\"p90_ns\":%lld,\"p99_ns\":%lld,\"p999_ns\":%lld,\"max_ns\":%lld,\"checksum\":%llu}",
                                       sorted.size(), at(0.5), at(0.9), at(0.99), at(0.999), sorted.empty() ? 0LL : (long long)sorted.back(),
                                       (unsigned long long)checksum_));
    }

    Host host_;
    bool producer_{false};
    uint16_t bytes_{1500};
    int64_t period_ns_{2000000};
    uint64_t sent_{0}, refused_{0}, steps_{0}, checksum_{0};
    std::vector<int64_t> latencies_;
};

const xgc2_module_desc kDesc = {XGC2_MODULE_ABI_MAJOR, XGC2_MODULE_ABI_MINOR, "test_bench_frame", "1.0.0", kPorts, 2,
                                &Lifecycle<Bench>::create, &Lifecycle<Bench>::configure, &Lifecycle<Bench>::start,
                                &Lifecycle<Bench>::step, &Lifecycle<Bench>::stop, &Lifecycle<Bench>::destroy};

}  // namespace

XGC2_MODULE_EXPORT const xgc2_module_desc* xgc2_module_entry(void) {
    return &kDesc;
}
