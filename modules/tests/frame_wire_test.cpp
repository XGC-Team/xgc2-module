// The datagram format, the fragmentation and the bounded reassembly, and the wire budget of the TRO v2 messages
// (sizes from tro-dmpc-v2 codec.cpp: beacon 37 B, plan 998 / 1238 / 1958 B for N = 40 / 50 / 80).
#include <algorithm>
#include <random>
#include <vector>

#include "../common/frame_wire.hpp"
#include "check.hpp"

using namespace xgc2::sdk::wire;

namespace {

std::vector<uint8_t> pattern(size_t n, uint8_t seed) {
    std::vector<uint8_t> v(n);
    for (size_t i = 0; i < n; ++i) {
        v[i] = static_cast<uint8_t>(seed + i * 7);
    }
    return v;
}

Header frameHeader(size_t len, uint32_t seq) {
    Header h;
    h.src = 3;
    h.dst = 1;
    h.kind = 42;
    h.total_len = static_cast<uint16_t>(len);
    h.frame_seq = seq;
    h.t_tx_ns = 123456789012345LL;
    return h;
}

struct Datagram {
    std::vector<uint8_t> bytes;
};

std::vector<Datagram> split(const Header& h, const std::vector<uint8_t>& data, size_t datagram_bytes) {
    std::vector<Datagram> out;
    fragment(h, data.data(), datagram_bytes, [&](const uint8_t* d, size_t n) { out.push_back({{d, d + n}}); });
    return out;
}

void headerRoundTrip() {
    Header h = frameHeader(100, 0xFFFFFFF0u);
    h.flags = kFlagBroadcast;
    h.dst = kBroadcast;
    const auto data = pattern(100, 1);
    const auto d = split(h, data, 1400);
    CHECK_EQ(d.size(), 1u);
    CHECK_EQ(d[0].bytes.size(), kHeaderBytes + 100);
    Header p;
    CHECK(parseDatagram(d[0].bytes.data(), d[0].bytes.size(), p, 2032) == Parse::kOk);
    CHECK_EQ(p.src, 3);
    CHECK_EQ(p.dst, kBroadcast);
    CHECK_EQ(p.frame_seq, 0xFFFFFFF0u);
    CHECK_EQ(p.kind, 42);
    CHECK_EQ(p.total_len, 100);
    CHECK_EQ(p.t_tx_ns, 123456789012345LL);
    CHECK_EQ(p.flags & kFlagBroadcast, kFlagBroadcast);
    CHECK(std::equal(data.begin(), data.end(), d[0].bytes.begin() + kHeaderBytes));
}

void entryValidation() {
    const auto data = pattern(300, 9);
    auto d = split(frameHeader(300, 1), data, 1400)[0].bytes;
    Header h;
    CHECK(parseDatagram(d.data(), 10, h, 2032) == Parse::kShort);
    auto bad = d;
    bad[0] = 'Y';
    CHECK(parseDatagram(bad.data(), bad.size(), h, 2032) == Parse::kMagic);
    bad = d;
    bad[2] = 9;
    CHECK(parseDatagram(bad.data(), bad.size(), h, 2032) == Parse::kVersion);
    CHECK(parseDatagram(d.data(), d.size() - 1, h, 2032) == Parse::kGeometry);  // frag_len says more than there is
    CHECK(parseDatagram(d.data(), d.size(), h, 200) == Parse::kGeometry);      // frame larger than the receiver's class
    bad = d;
    bad[17] = 9;  // frag_cnt above the bound
    CHECK(parseDatagram(bad.data(), bad.size(), h, 2032) == Parse::kGeometry);
    bad = d;
    bad[16] = 1;  // frag_idx outside a single-fragment frame
    CHECK(parseDatagram(bad.data(), bad.size(), h, 2032) == Parse::kGeometry);
}

void wireBudgetOfTheTroMessages() {
    // Beacon: one datagram of header + 37 bytes.
    CHECK_EQ(fragmentCount(37, 1400), 1u);
    CHECK_EQ(wireBytes(37, 1400), 37 + kHeaderBytes);
    // Plans N = 40 and 50 fit one datagram; N = 80 (1958 B) needs two; every datagram stays below the path MTU.
    CHECK_EQ(fragmentCount(998, 1400), 1u);
    CHECK_EQ(fragmentCount(1238, 1400), 1u);
    CHECK_EQ(fragmentCount(1958, 1400), 2u);
    CHECK_EQ(wireBytes(1958, 1400), 1958 + 2 * kHeaderBytes);
    for (size_t len : {37u, 998u, 1238u, 1958u, 2032u}) {
        for (const Datagram& d : split(frameHeader(len, 5), pattern(len, 3), 1400)) {
            CHECK(d.bytes.size() <= 1400);
            CHECK(d.bytes.size() + 28 <= 1500);  // UDP + IPv4 header: the datagram fits an Ethernet MTU
        }
    }
    // Plan + velocities for N = 80 (3904 B) is a frame8k frame: three datagrams, still bounded.
    CHECK_EQ(fragmentCount(3904, 1400), 3u);
    CHECK_EQ(fragmentCount(8176, 1400), 6u);
    CHECK_EQ(fragmentCount(8176, 512), 0u);  // 17 fragments: refused, never sent
    // Worst-case egress of the TRO scenarios against the field budget of about 1 Mbit/s per vehicle
    // (static, from wire_budget.py): 10 UAVs, 9 partners, N = 80, period 0.1 s.
    const double period_s = 0.1;
    const size_t beacons = 9 * wireBytes(37, 1400);
    const size_t plans = 9 * wireBytes(1958, 1400);
    const double mbit = (beacons + plans) * 8.0 / period_s / 1e6;
    CHECK_NEAR(mbit, 1.497, 0.01);  // payload-only 1.436 plus the headers of the 9 + 18 datagrams
    CHECK(mbit < 2.0);
    // The 8-UAV, 3-partner navigation case (N = 50) stays well below the budget.
    const double navigation = (7 * wireBytes(37, 1400) + 3 * wireBytes(1238, 1400)) * 8.0 / period_s / 1e6;
    CHECK(navigation < 0.6);
}

void reassemblyInAnyOrder() {
    std::mt19937 random(7);
    for (size_t len : {1400u, 1958u, 2032u, 3000u, 8176u}) {
        const auto data = pattern(len, 5);
        const size_t capacity = std::max<size_t>(len, 2032);
        auto parts = split(frameHeader(len, 9), data, 1400);
        CHECK(parts.size() >= 2u);
        for (int round = 0; round < 20; ++round) {
            std::shuffle(parts.begin(), parts.end(), random);
            Reassembler r(4, capacity, 100'000'000);
            Reassembler::Frame done;
            for (size_t i = 0; i < parts.size(); ++i) {
                Header h;
                CHECK(parseDatagram(parts[i].bytes.data(), parts[i].bytes.size(), h, capacity) == Parse::kOk);
                done = r.add(h, parts[i].bytes.data() + kHeaderBytes, 1000 + static_cast<int64_t>(i));
                CHECK((done.data != nullptr) == (i + 1 == parts.size()));
            }
            CHECK(done.data != nullptr);
            CHECK_EQ(done.header.total_len, len);
            CHECK(std::equal(data.begin(), data.end(), done.data));
            CHECK_EQ(r.inFlight(), 0u);
        }
    }
}

void aMissingFragmentDropsTheWholeFrame() {
    const auto data = pattern(1958, 5);
    auto parts = split(frameHeader(1958, 9), data, 1400);
    Reassembler r(4, 2032, 100'000'000);
    Header h;
    parseDatagram(parts[0].bytes.data(), parts[0].bytes.size(), h, 2032);
    CHECK(r.add(h, parts[0].bytes.data() + kHeaderBytes, 0).data == nullptr);
    CHECK_EQ(r.inFlight(), 1u);
    // Not yet: 99 ms.
    r.expire(99'000'000);
    CHECK_EQ(r.inFlight(), 1u);
    CHECK_EQ(r.counters().dropped_incomplete, 0u);
    r.expire(100'000'000);
    CHECK_EQ(r.inFlight(), 0u);
    CHECK_EQ(r.counters().dropped_incomplete, 1u);
    // The late second fragment starts a new slot that never completes: no partial delivery, ever.
    Header h2;
    parseDatagram(parts[1].bytes.data(), parts[1].bytes.size(), h2, 2032);
    CHECK(r.add(h2, parts[1].bytes.data() + kHeaderBytes, 150'000'000).data == nullptr);
}

void duplicatesAndEviction() {
    const auto data = pattern(1958, 5);
    auto parts = split(frameHeader(1958, 1), data, 1400);
    Reassembler r(2, 2032, 1'000'000'000);
    Header h;
    parseDatagram(parts[0].bytes.data(), parts[0].bytes.size(), h, 2032);
    r.add(h, parts[0].bytes.data() + kHeaderBytes, 10);
    r.add(h, parts[0].bytes.data() + kHeaderBytes, 11);
    CHECK_EQ(r.counters().duplicates, 1u);
    // Two more frames in flight evict the oldest one (counted as dropped, no unbounded growth).
    for (uint32_t seq = 2; seq <= 3; ++seq) {
        auto other = split(frameHeader(1958, seq), data, 1400);
        Header oh;
        parseDatagram(other[0].bytes.data(), other[0].bytes.size(), oh, 2032);
        r.add(oh, other[0].bytes.data() + kHeaderBytes, 20 + seq);
    }
    CHECK_EQ(r.inFlight(), 2u);
    CHECK_EQ(r.counters().dropped_incomplete, 1u);
}

}  // namespace

int main() {
    headerRoundTrip();
    entryValidation();
    wireBudgetOfTheTroMessages();
    reassemblyInAnyOrder();
    aMissingFragmentDropsTheWholeFrame();
    duplicatesAndEviction();
    TEST_MAIN_END();
}
