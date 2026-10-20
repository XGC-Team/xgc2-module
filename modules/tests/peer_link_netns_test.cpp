// Packet-level behaviour of peer_link under loss, delay, reordering and duplication of the link, on the loopback
// interface of a PRIVATE network namespace (run through private-netns.py: nothing visible on the VM changes).
// Each scenario installs a netem qdisc on lo, runs a flow of frames from A to B and compares the counters of the
// two links with what the impairment implies. Not a performance measurement.
#include <sys/socket.h>
#include <unistd.h>
#include <xgc2/sdk/frame.hpp>
#include <xgc2/sdk/test_host.hpp>

#include <algorithm>
#include <chrono>
#include <cstdlib>
#include <string>
#include <thread>
#include <vector>

#include "../common/frame_wire.hpp"
#include "../common/link_stats.h"
#include "check.hpp"

using namespace xgc2::sdk;
using namespace xgc2::sdk::test;
using std::chrono::milliseconds;

#ifndef PEER_LINK_PATH
#error PEER_LINK_PATH must name the module library
#endif
#ifndef FRAME_TYPE
#define FRAME_TYPE xgc2_frame2k
#endif
using Frame = FRAME_TYPE;

namespace {

void tc(const std::string& arguments) {
    const std::string command = "tc " + arguments + " 2>&1";
    if (std::system(command.c_str()) != 0) {
        std::fprintf(stderr, "command failed: %s\n", command.c_str());
        std::exit(77);
    }
}
void netem(const std::string& options) {
    tc("qdisc replace dev lo root netem " + options);
}
void clearNetem() {
    if (std::system("tc qdisc del dev lo root 2>/dev/null") != 0) {
        // nothing installed: fine
    }
}

struct Pair {
    Pair(ModuleLibrary& library, int base) : a(library.desc()), b(library.desc()) {
        const std::string pa = "127.0.0.1:" + std::to_string(base), pb = "127.0.0.1:" + std::to_string(base + 1);
        CHECK_EQ(a.create("{\"self\":1,\"bind\":\"" + pa + "\",\"peers\":{\"2\":\"" + pb + "\"},\"stats_period_ms\":0}"), XGC2_OK);
        CHECK_EQ(b.create("{\"self\":2,\"bind\":\"" + pb + "\",\"peers\":{\"1\":\"" + pa + "\"},\"stats_period_ms\":0,\"reassembly_timeout_ms\":200}"),
                 XGC2_OK);
        a.useMonotonicClock();
        b.useMonotonicClock();
        CHECK_EQ(a.start(), XGC2_OK);
        CHECK_EQ(b.start(), XGC2_OK);
    }
    ~Pair() {
        a.stop();
        b.stop();
    }
    // Sends `count` frames of `len` bytes, one every `gap_us`; the payload carries its own number and a check byte.
    void flow(size_t count, size_t len, int gap_us) {
        for (size_t i = 0; i < count; ++i) {
            std::vector<uint8_t> data(len);
            for (size_t k = 0; k < len; ++k) {
                data[k] = static_cast<uint8_t>(i * 31 + k);
            }
            Frame frame;
            CHECK(fillFrame(frame, 2, static_cast<uint16_t>(i), data.data(), len));
            a.push("tx", frame, 1);
            a.step(XGC2_STEP_INPUT);
            std::this_thread::sleep_for(std::chrono::microseconds(gap_us));
        }
    }
    xgc2_link_stats stats(TestHost& h) {
        h.step(XGC2_STEP_TIMER);
        auto all = h.takeAs<xgc2_link_stats>("stats");
        return all.empty() ? xgc2_link_stats{} : all.back();
    }
    // Every delivered frame must be exactly the frame that was sent: never a partial or a mixed one.
    size_t verifyDelivered(size_t len) {
        size_t good = 0;
        for (const Sample& s : b.outputs("rx")) {
            const Frame f = s.as<Frame>();
            bool ok = f.len == len;
            for (size_t k = 0; ok && k < len; ++k) {
                ok = f.data[k] == static_cast<uint8_t>(f.kind * 31 + k);
            }
            CHECK(ok);
            good += ok ? 1U : 0U;
        }
        return good;
    }
    TestHost a, b;
};

void settle() {
    std::this_thread::sleep_for(milliseconds(600));
}

}  // namespace

int main() {
    // The namespace must be private: refuse to run anywhere else.
    if (std::getenv("EXPECTED_PARENT_NETNS") != nullptr) {
        char link[64] = {0};
        ssize_t n = ::readlink("/proc/self/ns/net", link, sizeof link - 1);
        if (n > 0 && std::string(link) == std::getenv("EXPECTED_PARENT_NETNS")) {
            std::fprintf(stderr, "refusing to shape the network of the parent namespace\n");
            return 78;
        }
    }
    tc("qdisc show dev lo");
    ModuleLibrary library(PEER_LINK_PATH);
    int port = 41000;

    {   // Ideal link: everything arrives, nothing is partial.
        clearNetem();
        Pair p(library, port += 2);
        p.flow(500, 1958, 500);
        settle();
        const auto tx = p.stats(p.a), rx = p.stats(p.b);
        CHECK_EQ(rx.rx_frames, 500u);
        CHECK_EQ(rx.rx_incomplete + rx.rx_malformed + rx.rx_duplicates, 0u);
        CHECK_EQ(tx.tx_datagrams, 1000u);
        CHECK_EQ(p.verifyDelivered(1958), 500u);
    }
    {   // 10 % loss: a two-fragment frame survives with 0.81, one fragment alone is dropped as a whole.
        netem("loss 10%");
        Pair p(library, port += 2);
        const size_t n = 3000;
        p.flow(n, 1958, 300);
        settle();
        const auto rx = p.stats(p.b);
        const double delivered = double(rx.rx_frames) / n;
        const double half = double(rx.rx_incomplete) / n;
        std::printf("loss 10%%, 2 datagrams per frame: delivered %.3f (0.81 expected), dropped as incomplete %.3f (0.18 expected)\n",
                    delivered, half);
        CHECK_NEAR(delivered, 0.81, 0.04);
        CHECK_NEAR(half, 0.18, 0.04);
        CHECK_EQ(p.verifyDelivered(1958), rx.rx_frames);   // never a partial frame
        CHECK_NEAR(double(rx.rx_seq_lost) / n, 0.19, 0.04);
        // A frame of one datagram survives with 0.9.
        Pair q(library, port += 2);
        q.flow(n, 37, 100);
        settle();
        const double single = double(q.stats(q.b).rx_frames) / n;
        std::printf("loss 10%%, 1 datagram per frame: delivered %.3f (0.90 expected)\n", single);
        CHECK_NEAR(single, 0.90, 0.03);
    }
    {   // Delay 50 ms +- 10 ms: everything arrives; the sample stamp minus the sender's time is the one-way delay.
        netem("delay 50ms 10ms");
        Pair p(library, port += 2);
        p.flow(200, 37, 2000);
        settle();
        CHECK_EQ(p.stats(p.b).rx_frames, 200u);
        std::vector<double> delays;
        for (const Sample& s : p.b.outputs("rx")) {
            delays.push_back((s.stamp_ns - s.as<Frame>().t_tx_ns) / 1e6);
        }
        std::sort(delays.begin(), delays.end());
        std::printf("delay 50ms 10ms: one-way p50 %.1f ms, min %.1f, max %.1f\n", delays[delays.size() / 2], delays.front(), delays.back());
        CHECK_NEAR(delays[delays.size() / 2], 50.0, 6.0);
        CHECK(delays.front() >= 38.0);
        CHECK(delays.back() <= 65.0);
    }
    {   // Reordering: nothing lost, some frames late, counted.
        netem("delay 20ms reorder 30% 50%");
        Pair p(library, port += 2);
        p.flow(400, 37, 3000);
        settle();
        const auto rx = p.stats(p.b);
        std::printf("reorder: %llu frames, %llu reordered, %llu still counted lost\n", (unsigned long long)rx.rx_frames,
                    (unsigned long long)rx.rx_reordered, (unsigned long long)rx.rx_seq_lost);
        CHECK_EQ(rx.rx_frames, 400u);
        CHECK(rx.rx_reordered > 0);
        CHECK_EQ(rx.rx_seq_lost, 0u);   // every late frame arrived; the estimate nets out to zero
    }
    {   // Duplication: repeated datagrams do not become repeated frames.
        netem("duplicate 25%");
        Pair p(library, port += 2);
        p.flow(400, 1958, 500);
        settle();
        const auto rx = p.stats(p.b);
        std::printf("duplicate 25%%: %llu frames delivered, %llu duplicate datagrams dropped\n", (unsigned long long)rx.rx_frames,
                    (unsigned long long)rx.rx_duplicates);
        CHECK_EQ(rx.rx_frames, 400u);
        CHECK(rx.rx_duplicates > 50);
        CHECK_EQ(p.verifyDelivered(1958), 400u);
    }
    {   // A blackhole, then recovery: the counters show the outage and the link needs no restart.
        clearNetem();
        Pair p(library, port += 2);
        p.flow(5, 37, 1000);
        settle();
        CHECK_EQ(p.stats(p.b).rx_frames, 5u);
        netem("loss 100%");
        p.flow(50, 37, 1000);
        settle();
        CHECK_EQ(p.stats(p.b).rx_frames, 5u);
        clearNetem();
        p.flow(50, 37, 1000);
        settle();
        const auto rx = p.stats(p.b);
        CHECK_EQ(rx.rx_frames, 55u);
        CHECK_EQ(rx.rx_seq_lost, 50u);   // the 50 frames of the outage are visible in the sequence numbers
    }
    clearNetem();
    TEST_MAIN_END();
}
