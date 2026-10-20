// peer_link as a module: addressing, fragmentation, entry validation, counters and lifecycle, over loopback UDP
// with the in-test host. Packet loss, delay and reordering are in peer_link_netns_test.cpp (private namespace).
#include <arpa/inet.h>
#include <netinet/in.h>
#include <sys/socket.h>
#include <unistd.h>
#include <xgc2/sdk/frame.hpp>
#include <xgc2/sdk/host.hpp>
#include <xgc2/sdk/test_host.hpp>

#include <chrono>
#include <string>
#include <thread>

#include "../common/frame_wire.hpp"
#include "../common/link_stats.h"
#include "check.hpp"

using namespace xgc2::sdk;
using namespace xgc2::sdk::test;
namespace wire = xgc2::sdk::wire;
using std::chrono::milliseconds;

#ifndef PEER_LINK_PATH
#error PEER_LINK_PATH must name the module library
#endif
#ifndef FRAME_TYPE
#define FRAME_TYPE xgc2_frame2k
#endif
using Frame = FRAME_TYPE;
constexpr uint32_t kCapacity = sizeof(Frame::data);

namespace {

int freePort() {
    int s = ::socket(AF_INET, SOCK_DGRAM, 0);
    sockaddr_in a{};
    a.sin_family = AF_INET;
    a.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
    ::bind(s, reinterpret_cast<sockaddr*>(&a), sizeof a);
    socklen_t n = sizeof a;
    ::getsockname(s, reinterpret_cast<sockaddr*>(&a), &n);
    ::close(s);
    return ntohs(a.sin_port);
}

std::string addr(int port) {
    return "127.0.0.1:" + std::to_string(port);
}

struct Node {
    Node(ModuleLibrary& library, uint16_t id, int port, const std::string& peers, const std::string& extra = "")
        : host(library.desc()), id(id), port(port) {
        const std::string config = "{\"self\":" + std::to_string(id) + ",\"bind\":\"" + addr(port) + "\",\"peers\":{" + peers +
                                   "},\"stats_period_ms\":0" + extra + "}";
        CHECK_EQ(host.create(config), XGC2_OK);
        CHECK_EQ(host.start(), XGC2_OK);
    }
    ~Node() {
        host.stop();
    }
    void send(uint16_t to, uint16_t kind, const std::vector<uint8_t>& data, int64_t t_tx = 0) {
        Frame frame;
        CHECK(fillFrame(frame, to, kind, data.data(), data.size(), t_tx));
        CHECK(host.push("tx", frame, 1));
        if (!live) host.setNow(1000);
        CHECK_EQ(host.step(XGC2_STEP_INPUT), XGC2_OK);
    }
    // The clock of the module runs (the reassembly timeout needs time to pass); steps no longer set it.
    void useLiveClock() {
        live = true;
        host.useMonotonicClock();
    }
    xgc2_link_stats stats() {
        if (!live) host.setNow(2000);
        host.step(XGC2_STEP_TIMER);
        auto all = host.takeAs<xgc2_link_stats>("stats");
        CHECK(!all.empty());
        return all.empty() ? xgc2_link_stats{} : all.back();
    }
    TestHost host;
    uint16_t id;
    int port;
    bool live{false};
};

std::vector<uint8_t> bytes(size_t n, uint8_t seed) {
    std::vector<uint8_t> v(n);
    for (size_t i = 0; i < n; ++i) {
        v[i] = static_cast<uint8_t>(seed + 3 * i);
    }
    return v;
}

void sendRaw(int to_port, const uint8_t* data, size_t size) {
    int s = ::socket(AF_INET, SOCK_DGRAM, 0);
    sockaddr_in a{};
    a.sin_family = AF_INET;
    a.sin_port = htons(static_cast<uint16_t>(to_port));
    a.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
    ::sendto(s, data, size, 0, reinterpret_cast<sockaddr*>(&a), sizeof a);
    ::close(s);
}

void unicastBroadcastAndFragments(ModuleLibrary& library) {
    const int pa = freePort(), pb = freePort(), pc = freePort();
    Node a(library, 1, pa, "\"2\":\"" + addr(pb) + "\",\"3\":\"" + addr(pc) + "\"");
    Node b(library, 2, pb, "\"1\":\"" + addr(pa) + "\",\"3\":\"" + addr(pc) + "\"");
    Node c(library, 3, pc, "\"1\":\"" + addr(pa) + "\",\"2\":\"" + addr(pb) + "\"");
    // A small unicast frame with the sender's own time.
    const auto beacon = bytes(37, 1);
    a.send(2, 7, beacon, 555);
    CHECK(b.host.waitOutputs("rx", 1, milliseconds(2000)));
    auto got = b.host.takeAs<Frame>("rx");
    CHECK_EQ(got.size(), 1u);
    CHECK_EQ(got[0].peer, 1);
    CHECK_EQ(got[0].kind, 7);
    CHECK_EQ(got[0].len, 37);
    CHECK_EQ(got[0].t_tx_ns, 555);
    CHECK_EQ(got[0].flags, 0);
    CHECK(std::equal(beacon.begin(), beacon.end(), got[0].data));
    CHECK(c.host.outputs("rx").empty());  // not addressed to C
    // Filled time: a zero t_tx_ns becomes the sender's host clock (1000 in the test).
    a.send(2, 8, beacon, 0);
    CHECK(b.host.waitOutputs("rx", 1, milliseconds(2000)));
    got = b.host.takeAs<Frame>("rx");
    CHECK_EQ(got[0].t_tx_ns, 1000);
    // A frame that needs two datagrams, and the largest the class carries.
    for (size_t len : {size_t(1400), size_t(1958), size_t(kCapacity)}) {
        const auto plan = bytes(len, 9);
        a.send(2, 9, plan, 1);
        CHECK(b.host.waitOutputs("rx", 1, milliseconds(2000)));
        got = b.host.takeAs<Frame>("rx");
        CHECK_EQ(got.size(), 1u);
        CHECK_EQ(got[0].len, len);
        CHECK(std::equal(plan.begin(), plan.end(), got[0].data));
    }
    // Broadcast: both peers, flagged.
    a.send(XGC2_FRAME_BROADCAST, 3, beacon, 1);
    CHECK(b.host.waitOutputs("rx", 1, milliseconds(2000)));
    CHECK(c.host.waitOutputs("rx", 1, milliseconds(2000)));
    CHECK_EQ(b.host.takeAs<Frame>("rx")[0].flags, XGC2_FRAME_FLAG_BROADCAST);
    CHECK_EQ(c.host.takeAs<Frame>("rx")[0].peer, 1);
    // Counters of the sender: 2 + 1 + (1 + 2 + n) datagrams, no error.
    const xgc2_link_stats s = a.stats();
    const unsigned big = wire::fragmentCount(kCapacity, 1400);
    CHECK_EQ(s.tx_frames, 1u + 1u + 3u + 2u);
    CHECK_EQ(s.tx_datagrams, 1u + 1u + 2u + 2u + big + 2u);
    CHECK_EQ(s.tx_invalid + s.tx_errors + s.tx_would_block, 0u);
    CHECK(s.tx_bytes > 0);
    const xgc2_link_stats r = b.stats();
    CHECK_EQ(r.rx_frames, 1u + 1u + 3u + 1u);
    CHECK_EQ(r.rx_malformed + r.rx_incomplete + r.rx_duplicates + r.rx_queue_full, 0u);
}

void invalidFramesAreRefusedAtTheEntry(ModuleLibrary& library) {
    const int pa = freePort(), pb = freePort();
    Node a(library, 1, pa, "\"2\":\"" + addr(pb) + "\"");
    a.send(9, 1, bytes(10, 1));   // unknown destination
    a.send(1, 1, bytes(10, 1));   // itself
    Frame frame;
    fillFrame(frame, 2, 1, "x", 1);
    frame.len = static_cast<uint16_t>(kCapacity + 1);  // a length no frame of the class can have
    CHECK(a.host.push("tx", frame, 1));
    CHECK_EQ(a.host.step(XGC2_STEP_INPUT), XGC2_OK);
    const xgc2_link_stats s = a.stats();
    CHECK_EQ(s.tx_invalid, 3u);
    CHECK_EQ(s.tx_datagrams, 0u);
}

void entryChecksOfTheReceiver(ModuleLibrary& library) {
    const int pa = freePort(), pb = freePort();
    Node b(library, 2, pb, "\"1\":\"" + addr(pa) + "\"", ",\"reassembly_timeout_ms\":50");
    b.useLiveClock();
    auto datagrams = [&](uint16_t src, uint16_t dst, uint32_t seq, size_t len, bool skip_second = false) {
        wire::Header h;
        h.src = src;
        h.dst = dst;
        h.frame_seq = seq;
        h.kind = 5;
        h.total_len = static_cast<uint16_t>(len);
        const auto data = bytes(len, 4);
        std::vector<std::vector<uint8_t>> out;
        wire::fragment(h, data.data(), 1400, [&](const uint8_t* d, size_t n) { out.emplace_back(d, d + n); });
        if (skip_second && out.size() > 1) {
            out.erase(out.begin() + 1);
        }
        return out;
    };
    const uint8_t junk[5] = {1, 2, 3, 4, 5};
    sendRaw(pb, junk, sizeof junk);
    for (auto& d : datagrams(7, 2, 0, 20)) sendRaw(pb, d.data(), d.size());   // not a configured peer
    for (auto& d : datagrams(1, 9, 1, 20)) sendRaw(pb, d.data(), d.size());   // for another endpoint
    for (auto& d : datagrams(1, 2, 2, 20)) sendRaw(pb, d.data(), d.size());   // fine
    CHECK(b.host.waitOutputs("rx", 1, milliseconds(2000)));
    // Two frames with a gap in the sequence, then a late one: loss estimate and reorder counter.
    for (auto& d : datagrams(1, 2, 6, 20)) sendRaw(pb, d.data(), d.size());
    for (auto& d : datagrams(1, 2, 4, 20)) sendRaw(pb, d.data(), d.size());
    CHECK(b.host.waitOutputs("rx", 3, milliseconds(2000)));
    // A frame whose second fragment never comes, and the same frame complete.
    for (auto& d : datagrams(1, 2, 10, 1958, true)) sendRaw(pb, d.data(), d.size());
    std::this_thread::sleep_for(milliseconds(200));  // the 50 ms timeout passes, the receiver notices when idle
    xgc2_link_stats s = b.stats();
    CHECK_EQ(s.rx_malformed, 1u);
    CHECK_EQ(s.rx_unknown_peer, 1u);
    CHECK_EQ(s.rx_misaddressed, 1u);
    CHECK_EQ(s.rx_frames, 3u);
    CHECK_EQ(s.rx_incomplete, 1u);
    CHECK_EQ(s.rx_reordered, 1u);
    CHECK_EQ(s.rx_seq_lost, 2u);  // frames 3 and 5 are missing; 4 arrived late and is no longer counted lost
    CHECK(b.host.outputs("rx").size() == 3u);  // the truncated frame was never delivered
    for (auto& d : datagrams(1, 2, 11, 1958)) sendRaw(pb, d.data(), d.size());
    CHECK(b.host.waitOutputs("rx", 4, milliseconds(2000)));
    CHECK_EQ(b.host.outputs("rx").back().as<Frame>().len, 1958);
}

void duplicatesAreDropped(ModuleLibrary& library) {
    const int pa = freePort(), pb = freePort();
    Node b(library, 2, pb, "\"1\":\"" + addr(pa) + "\"", ",\"reassembly_timeout_ms\":50");
    b.useLiveClock();
    auto split = [&](uint32_t seq, size_t len) {
        wire::Header h;
        h.src = 1;
        h.dst = 2;
        h.frame_seq = seq;
        h.total_len = static_cast<uint16_t>(len);
        const auto data = bytes(len, 4);
        std::vector<std::vector<uint8_t>> out;
        wire::fragment(h, data.data(), 1400, [&](const uint8_t* d, size_t n) { out.emplace_back(d, d + n); });
        return out;
    };
    // A single-datagram frame twice: one delivery.
    auto one = split(0, 30);
    sendRaw(pb, one[0].data(), one[0].size());
    sendRaw(pb, one[0].data(), one[0].size());
    CHECK(b.host.waitOutputs("rx", 1, milliseconds(2000)));
    // A two-fragment frame whose first fragment arrives twice, and again after completion: one delivery, and the
    // late repeat is a duplicate, not a frame that never completes.
    auto two = split(1, 1958);
    sendRaw(pb, two[0].data(), two[0].size());
    sendRaw(pb, two[0].data(), two[0].size());
    sendRaw(pb, two[1].data(), two[1].size());
    CHECK(b.host.waitOutputs("rx", 2, milliseconds(2000)));
    sendRaw(pb, two[0].data(), two[0].size());
    std::this_thread::sleep_for(milliseconds(200));
    const xgc2_link_stats s = b.stats();
    CHECK_EQ(s.rx_frames, 2u);
    CHECK_EQ(s.rx_duplicates, 3u);
    CHECK_EQ(s.rx_incomplete, 0u);
    CHECK_EQ(b.host.outputs("rx").size(), 2u);
}

void configurationIsValidated(ModuleLibrary& library) {
    const int p = freePort();
    const std::string bind = ",\"bind\":\"" + addr(p) + "\"";
    const std::vector<std::string> bad = {
        "{}",
        "{\"self\":65535" + bind + "}",
        "{\"self\":1,\"bind\":\"nonsense\"}",
        "{\"self\":1" + bind + ",\"peers\":{\"1\":\"127.0.0.1:9\"}}",       // peer id equal to self
        "{\"self\":1" + bind + ",\"peers\":{\"x\":\"127.0.0.1:9\"}}",
        "{\"self\":1" + bind + ",\"peers\":{\"2\":\"127.0.0.1\"}}",
        "{\"self\":1" + bind + ",\"datagram_bytes\":100}",
        "{\"self\":1" + bind + ",\"datagram_bytes\":9000}",
        "{\"self\":1" + bind + ",\"reassembly_timeout_ms\":0}",
        "{\"self\":1" + bind + ",\"reassembly_slots\":0}",
        "{\"self\":1" + bind + ",\"typo\":1}",
    };
    for (const std::string& config : bad) {
        TestHost host(library.desc());
        CHECK_EQ(host.create(config), XGC2_ERR_INVALID);
        CHECK(host.logged(kLogError, "Invalid configuration"));
    }
    // A datagram size that makes the largest frame need more than 8 fragments is refused up front.
    TestHost host(library.desc());
    CHECK_EQ(host.create("{\"self\":1" + bind + ",\"datagram_bytes\":512}"), kCapacity > 3000 ? XGC2_ERR_INVALID : XGC2_OK);
}

void lifecycleAndLiveConfiguration(ModuleLibrary& library) {
    const int pa = freePort(), pb = freePort();
    Node a(library, 1, pa, "\"2\":\"" + addr(pb) + "\"");
    // A live change of the peers is accepted, of the endpoint or the address is not.
    CHECK_EQ(a.host.configure("{\"self\":1,\"bind\":\"" + addr(pa) + "\",\"peers\":{\"2\":\"" + addr(pb) + "\",\"3\":\"127.0.0.1:9\"},\"stats_period_ms\":0}"), XGC2_OK);
    CHECK_EQ(a.host.configure("{\"self\":5,\"bind\":\"" + addr(pa) + "\",\"stats_period_ms\":0}"), XGC2_ERR_INVALID);
    CHECK_EQ(a.host.configure("{\"self\":1,\"bind\":\"" + addr(freePort()) + "\",\"stats_period_ms\":0}"), XGC2_ERR_INVALID);
    CHECK_EQ(a.host.periodNs(), 0);
    // Stop joins the receive thread and releases the port; start binds it again.
    CHECK_EQ(a.host.stop(), XGC2_OK);
    Node again(library, 4, pa, "");
    CHECK_EQ(again.host.stop(), XGC2_OK);
    CHECK_EQ(a.host.start(), XGC2_OK);
    // A second instance cannot take a port that is in use: start fails with a message, the host reports it.
    TestHost clash(library.desc());
    CHECK_EQ(clash.create("{\"self\":9,\"bind\":\"" + addr(pa) + "\"}"), XGC2_OK);
    CHECK_EQ(clash.start(), XGC2_ERR_INTERNAL);
    CHECK(clash.logged(kLogError, "bind"));
    // The stats period goes to the host timer.
    TestHost timed(library.desc());
    CHECK_EQ(timed.create("{\"self\":1,\"bind\":\"" + addr(freePort()) + "\",\"stats_period_ms\":250}"), XGC2_OK);
    CHECK_EQ(timed.start(), XGC2_OK);
    CHECK_EQ(timed.periodNs(), 250'000'000);
    CHECK_EQ(timed.stop(), XGC2_OK);
    CHECK_EQ(timed.periodNs(), 0);
}

}  // namespace

int main() {
    ModuleLibrary library(PEER_LINK_PATH);
    CHECK_EQ(std::string(library.desc()->name), "xgc2_peer_link");
    CHECK_EQ(library.desc()->port_count, 3u);
    CHECK_EQ(library.desc()->abi_minor, XGC2_MODULE_ABI_MINOR);
    unicastBroadcastAndFragments(library);
    invalidFramesAreRefusedAtTheEntry(library);
    entryChecksOfTheReceiver(library);
    duplicatesAreDropped(library);
    configurationIsValidated(library);
    lifecycleAndLiveConfiguration(library);
    TEST_MAIN_END();
}
