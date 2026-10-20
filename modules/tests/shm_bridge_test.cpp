// shm_bridge: addressing, ordering, a full inbox, validation of the segment, and two real processes.
#include <sys/wait.h>
#include <unistd.h>
#include <xgc2/sdk/frame.hpp>
#include <xgc2/sdk/host.hpp>
#include <xgc2/sdk/test_host.hpp>

#include <chrono>
#include <string>
#include <thread>

#include "../common/link_stats.h"
#include "check.hpp"

using namespace xgc2::sdk;
using namespace xgc2::sdk::test;
using std::chrono::milliseconds;

#ifndef SHM_BRIDGE_PATH
#error SHM_BRIDGE_PATH must name the module library
#endif
#ifndef FRAME_TYPE
#define FRAME_TYPE xgc2_frame2k
#endif
using Frame = FRAME_TYPE;

namespace {

std::string config(const std::string& name, int self, int endpoints, bool owner, const std::string& extra = "") {
    return "{\"shm_name\":\"" + name + "\",\"self\":" + std::to_string(self) + ",\"endpoints\":" + std::to_string(endpoints) +
           ",\"owner\":" + (owner ? "true" : "false") + ",\"stats_period_ms\":0" + extra + "}";
}

std::string uniqueName(const char* tag) {
    return std::string("/xgc2-test-") + tag + "-" + std::to_string(::getpid());
}

struct Node {
    Node(ModuleLibrary& library, const std::string& json) : host(library.desc()) {
        CHECK_EQ(host.create(json), XGC2_OK);
        CHECK_EQ(host.start(), XGC2_OK);
    }
    ~Node() {
        host.stop();
    }
    bool send(uint16_t to, uint16_t kind, uint8_t fill, size_t len) {
        Frame frame;
        std::vector<uint8_t> data(len, fill);
        CHECK(fillFrame(frame, to, kind, data.data(), len));
        const bool pushed = host.push("tx", frame, 1);
        host.setNow(10);
        CHECK_EQ(host.step(XGC2_STEP_INPUT), XGC2_OK);
        return pushed;
    }
    xgc2_link_stats stats() {
        host.step(XGC2_STEP_TIMER);
        auto all = host.takeAs<xgc2_link_stats>("stats");
        return all.empty() ? xgc2_link_stats{} : all.back();
    }
    TestHost host;
};

void addressingAndOrder(ModuleLibrary& library) {
    const std::string name = uniqueName("order");
    Node owner(library, config(name, 0, 3, true));
    Node one(library, config(name, 1, 3, false));
    Node two(library, config(name, 2, 3, false));
    for (int i = 0; i < 20; ++i) {
        CHECK(one.send(0, static_cast<uint16_t>(i), static_cast<uint8_t>(i), 10 + i));
    }
    CHECK(owner.host.waitOutputs("rx", 20, milliseconds(2000)));
    auto got = owner.host.takeAs<Frame>("rx");
    for (int i = 0; i < 20; ++i) {
        CHECK_EQ(got[i].kind, i);          // order of one sender is kept
        CHECK_EQ(got[i].peer, 1);          // the sender, not the destination
        CHECK_EQ(got[i].len, 10 + i);
        CHECK_EQ(got[i].data[0], i);
    }
    CHECK(two.host.outputs("rx").empty());
    // Broadcast reaches everyone but the sender.
    CHECK(owner.send(XGC2_FRAME_BROADCAST, 99, 7, 100));
    CHECK(one.host.waitOutputs("rx", 1, milliseconds(2000)));
    CHECK(two.host.waitOutputs("rx", 1, milliseconds(2000)));
    CHECK_EQ(two.host.takeAs<Frame>("rx")[0].flags, XGC2_FRAME_FLAG_BROADCAST);
    CHECK(owner.host.outputs("rx").empty());
    // Refusals at the entry: unknown endpoint, itself.
    owner.send(7, 1, 1, 1);
    owner.send(0, 1, 1, 1);
    CHECK_EQ(owner.stats().tx_invalid, 2u);
}

void aFullInboxDropsAndCounts(ModuleLibrary& library) {
    const std::string name = uniqueName("full");
    Node owner(library, config(name, 0, 2, true, ",\"slots\":4"));
    Node writer(library, config(name, 1, 2, false, ",\"slots\":4"));
    // The owner's receive thread drains as fast as it can; to fill the ring stop the owner's consumer first.
    owner.host.stop();
    for (int i = 0; i < 10; ++i) {
        writer.send(0, 1, static_cast<uint8_t>(i), 5);
    }
    const xgc2_link_stats s = writer.stats();
    CHECK_EQ(s.tx_datagrams, 4u);       // the ring holds 4
    CHECK_EQ(s.tx_would_block, 6u);     // the writer never waits
    CHECK_EQ(s.tx_frames, 4u);
}

void theSegmentIsValidated(ModuleLibrary& library) {
    const std::string name = uniqueName("geom");
    Node owner(library, config(name, 0, 3, true));
    // Another geometry than the owner's: refused at attach.
    TestHost other(library.desc());
    CHECK_EQ(other.create(config(name, 1, 2, false, ",\"attach_timeout_ms\":200")), XGC2_OK);
    CHECK_EQ(other.start(), XGC2_ERR_INTERNAL);
    // A segment nobody publishes: the attach times out with a message.
    TestHost lonely(library.desc());
    CHECK_EQ(lonely.create(config(uniqueName("none"), 1, 2, false, ",\"attach_timeout_ms\":100")), XGC2_OK);
    CHECK_EQ(lonely.start(), XGC2_ERR_INTERNAL);
    CHECK(lonely.logged(kLogError, "did not publish"));
    for (const std::string bad : {"{}", "{\"shm_name\":\"nope\",\"self\":0,\"endpoints\":2}",
                                  "{\"shm_name\":\"/a/b\",\"self\":0,\"endpoints\":2}",
                                  "{\"shm_name\":\"/ok\",\"self\":2,\"endpoints\":2}",
                                  "{\"shm_name\":\"/ok\",\"self\":0,\"endpoints\":2,\"slots\":6}",
                                  "{\"shm_name\":\"/ok\",\"self\":0,\"endpoints\":2,\"extra\":1}"}) {
        TestHost host(library.desc());
        CHECK_EQ(host.create(bad), XGC2_ERR_INVALID);
    }
}

void twoProcesses(ModuleLibrary& library) {
    const std::string name = uniqueName("fork");
    Node owner(library, config(name, 0, 2, true));
    const pid_t child = ::fork();
    if (child == 0) {
        // The other process attaches to the same segment and sends 50 frames back to the owner.
        ModuleLibrary child_library(SHM_BRIDGE_PATH);
        Node peer(child_library, config(name, 1, 2, false));
        for (int i = 0; i < 50; ++i) {
            peer.send(0, static_cast<uint16_t>(i), 0xAB, 1000);
        }
        std::this_thread::sleep_for(milliseconds(100));
        ::_exit(check::failures() == 0 ? 0 : 1);
    }
    CHECK(owner.host.waitOutputs("rx", 50, milliseconds(5000)));
    int status = 0;
    ::waitpid(child, &status, 0);
    CHECK(WIFEXITED(status) && WEXITSTATUS(status) == 0);
    auto got = owner.host.takeAs<Frame>("rx");
    for (size_t i = 0; i < got.size(); ++i) {
        CHECK_EQ(got[i].kind, i);
        CHECK_EQ(got[i].data[999], 0xAB);
    }
}

}  // namespace

int main() {
    ModuleLibrary library(SHM_BRIDGE_PATH);
    CHECK_EQ(std::string(library.desc()->name), "xgc2_shm_bridge");
    addressingAndOrder(library);
    aFullInboxDropsAndCounts(library);
    theSegmentIsValidated(library);
    twoProcesses(library);
    TEST_MAIN_END();
}
