// A module shaped like the TRO v2 per-UAV agent, to drive the host (ABI minor 1) and the frame links without TRO's code.
//
// What it copies from the real method (tro-dmpc-v2): a period with two communication windows on a clock shared by
// the hosts (W1 beacons, W2 plans), a pure schedule machine that is polled with the time and returns the next action
// and the next instant (ScheduleMachine::poll), a solver that runs on its own thread and wakes the instance when done,
// and a transport that is nothing but byte frames to and from the peers (Transport over peer_link frames). It copies
// none of its mathematics: the plan is a counter pattern of the size of an N = 50 plan.
//
// Per period k, relative to the grid origin T0 (host.phaseT0Ns()) and period P:
//    t = 0        beacon (kind 1, 37 bytes) broadcast
//    t = W1       the solver starts (takes `solve_ms`)
//    solver done  plan (kind 2, `plan_bytes`) to the partner (the next endpoint, cyclic)
//    t = W2       commit: plans that arrive later than this are window misses
// Configuration (flat JSON numbers): self, peers (endpoint ids 1..peers, self among them), period_ms, w1_ms, w2_ms,
// solve_ms, plan_bytes.
#include <chrono>
#include <condition_variable>
#include <mutex>
#include <thread>

#include <xgc2/frame.h>
#include <xgc2/sdk/frame.hpp>
#include <xgc2/sdk/host.hpp>

#include "common.h"

namespace {

using namespace xgc2::sdk;

using Frame = xgc2_frame2k;
constexpr uint32_t kRx = 0, kTx = 1;
constexpr uint16_t kBeacon = 1, kPlan = 2;

const xgc2_port_desc kPorts[] = {
    port<Frame>("rx", XGC2_PORT_IN, XGC2_PORT_EVENT, XGC2_FRAME2K_SCHEMA, 64),
    port<Frame>("tx", XGC2_PORT_OUT, XGC2_PORT_EVENT, XGC2_FRAME2K_SCHEMA, 64),
};

struct Plan {
    int64_t period_index{-1};
    int64_t window_start_ns{0};
};

// The pure schedule: which action is due at `now` and when the next one is. No clock, no side effect.
struct ScheduleMachine {
    enum class Kind { kNone, kBeacon, kStartSolve, kCommit };
    struct Action {
        Kind kind;
        int64_t period_index;
        int64_t next_ns;  // the instant to be woken at next
    };
    int64_t t0_ns{0}, period_ns{100'000'000}, w1_ns{20'000'000}, w2_ns{60'000'000};
    int64_t beaconed{-1}, solving{-1}, committed{-1};

    // At most one action per call, in the order beacon, solver start, commit; a step that runs late catches up by
    // being polled again. Whole periods that passed unseen are skipped (the caller counts them).
    Action poll(int64_t now_ns) {
        if (now_ns < t0_ns) {
            return {Kind::kNone, -1, t0_ns};
        }
        const int64_t k = (now_ns - t0_ns) / period_ns;
        const int64_t start = t0_ns + k * period_ns;
        const int64_t t = now_ns - start;
        if (beaconed < k) {
            beaconed = k;
            return {Kind::kBeacon, k, start + w1_ns};
        }
        if (t >= w1_ns && solving < k) {
            solving = k;
            return {Kind::kStartSolve, k, start + w2_ns};
        }
        if (t >= w2_ns && committed < k) {
            committed = k;
            return {Kind::kCommit, k, start + period_ns};
        }
        return {Kind::kNone, k, t < w1_ns ? start + w1_ns : (t < w2_ns ? start + w2_ns : start + period_ns)};
    }
};

class Agent {
   public:
    explicit Agent(const Host& host) : host_(host) {}
    ~Agent() {
        stopSolver();
    }
    const Host& host() const {
        return host_;
    }

    void configure(const xgc2_config* config) {
        self_ = static_cast<uint16_t>(cfg_number(config, "self", 1));
        peers_ = static_cast<int>(cfg_number(config, "peers", 2));
        plan_bytes_ = static_cast<size_t>(cfg_number(config, "plan_bytes", 1238));
        solve_ns_ = static_cast<int64_t>(cfg_number(config, "solve_ms", 8) * 1e6);
        machine_.period_ns = static_cast<int64_t>(cfg_number(config, "period_ms", 100) * 1e6);
        machine_.w1_ns = static_cast<int64_t>(cfg_number(config, "w1_ms", 20) * 1e6);
        machine_.w2_ns = static_cast<int64_t>(cfg_number(config, "w2_ms", 60) * 1e6);
        if (plan_bytes_ > sizeof(Frame::data) || machine_.w1_ns >= machine_.w2_ns || machine_.w2_ns >= machine_.period_ns) {
            throw std::invalid_argument("windows must satisfy 0 < w1 < w2 < period and the plan must fit a frame");
        }
    }

    void start() {
        machine_.t0_ns = host_.phaseT0Ns();
        machine_ = ScheduleMachine{machine_.t0_ns, machine_.period_ns, machine_.w1_ns, machine_.w2_ns, -1, -1, -1};
        stop_ = false;
        solver_ = std::thread([this] { solverLoop(); });
        // The first wake is the next grid point; every later one is requested by the machine.
        const int64_t now = host_.nowNs();
        const int64_t first = machine_.t0_ns + ((now - machine_.t0_ns) / machine_.period_ns + 1) * machine_.period_ns;
        host_.wakeAt(first);
    }

    void step(const xgc2_step_ctx& ctx) {
        ++steps_;
        const int64_t now = ctx.now_ns;
        if ((ctx.reasons & XGC2_STEP_DEADLINE) != 0U && requested_ns_ > 0) {
            const int64_t late = now - requested_ns_;
            late_max_ns_ = late > late_max_ns_ ? late : late_max_ns_;
            late_sum_ns_ += late;
            ++deadline_steps_;
        }
        receive(now);
        bool solved = false;
        {
            std::lock_guard<std::mutex> lock(mutex_);
            solved = result_ready_;
            result_ready_ = false;
        }
        if (solved) {
            sendPlan(now);
        }
        // Drive the schedule until it has nothing due.
        for (int guard = 0; guard < 4; ++guard) {
            const ScheduleMachine::Action action = machine_.poll(now);
            if (action.kind == ScheduleMachine::Kind::kNone) {
                request(action.next_ns);
                break;
            }
            perform(action, now);
            request(action.next_ns);
        }
        if (steps_ % 10 == 0) {
            reportState();
        }
    }

    void stop() {
        stopSolver();
        reportState();
    }

   private:
    void request(int64_t at_ns) {
        requested_ns_ = at_ns;
        host_.wakeAt(at_ns);
    }

    void perform(const ScheduleMachine::Action& action, int64_t now) {
        using Kind = ScheduleMachine::Kind;
        switch (action.kind) {
            case Kind::kBeacon:
                send(XGC2_FRAME_BROADCAST, kBeacon, 37, now);
                ++beacons_sent_;
                break;
            case Kind::kStartSolve: {
                std::lock_guard<std::mutex> lock(mutex_);
                job_ = true;
                job_cv_.notify_one();
                ++solves_started_;
                break;
            }
            case Kind::kCommit:
                period_commit_ns_ = now;
                if (!plan_sent_for_period_) {
                    ++solver_late_;
                }
                plan_sent_for_period_ = false;
                break;
            case Kind::kNone:
                break;
        }
    }

    void sendPlan(int64_t now) {
        const uint16_t partner = static_cast<uint16_t>(self_ % peers_ + 1);
        send(partner, kPlan, plan_bytes_, now);
        ++plans_sent_;
        plan_sent_for_period_ = true;
    }

    void send(uint16_t to, uint16_t kind, size_t len, int64_t now) {
        Slot<Frame> slot = host_.write<Frame>(kTx);
        if (!slot) {
            ++tx_refused_;
            return;
        }
        slot->peer = to;
        slot->kind = kind;
        slot->len = static_cast<uint16_t>(len);
        slot->t_tx_ns = now;
        for (size_t i = 0; i < len; ++i) {
            slot->data[i] = static_cast<uint8_t>(self_ + i);
        }
        slot.commit(now);
    }

    void receive(int64_t now) {
        for (;;) {
            Sample<Frame> sample = host_.next<Frame>(kRx);
            if (!sample) {
                return;
            }
            const int64_t into_period = (now - machine_.t0_ns) % machine_.period_ns;
            if (sample->kind == kBeacon) {
                ++beacons_rx_;
            } else if (sample->kind == kPlan) {
                ++plans_rx_;
                // A plan is due before W2 ends; the sender's period starts at the same grid instant as ours.
                if (into_period >= machine_.w2_ns || into_period < machine_.w1_ns) {
                    ++window_misses_;
                }
                bool intact = sample->len > 0;
                for (size_t i = 0; i < sample->len; ++i) {
                    intact = intact && sample->data[i] == static_cast<uint8_t>(sample->peer + i);
                }
                if (!intact) {
                    ++corrupt_;
                }
            }
        }
    }

    void solverLoop() {
        std::unique_lock<std::mutex> lock(mutex_);
        for (;;) {
            job_cv_.wait(lock, [&] { return stop_ || job_; });
            if (stop_) {
                return;
            }
            job_ = false;
            lock.unlock();
            std::this_thread::sleep_for(std::chrono::nanoseconds(solve_ns_));  // the "optimization"
            lock.lock();
            result_ready_ = true;
            lock.unlock();
            host_.wake();  // from the solver thread: the instance steps and sends the plan
            lock.lock();
        }
    }

    void stopSolver() {
        {
            std::lock_guard<std::mutex> lock(mutex_);
            stop_ = true;
        }
        job_cv_.notify_all();
        if (solver_.joinable()) {
            solver_.join();
        }
    }

    void reportState() {
        host_.report(kHealthOk,
                     format("{\"steps\":%llu,\"deadline_steps\":%llu,\"late_max_us\":%lld,\"late_mean_us\":%lld,"
                            "\"beacons_sent\":%llu,\"beacons_rx\":%llu,\"plans_sent\":%llu,\"plans_rx\":%llu,"
                            "\"solves\":%llu,\"solver_late\":%llu,\"window_misses\":%llu,\"corrupt\":%llu,\"tx_refused\":%llu}",
                            (unsigned long long)steps_, (unsigned long long)deadline_steps_, (long long)(late_max_ns_ / 1000),
                            (long long)(deadline_steps_ ? late_sum_ns_ / (int64_t)deadline_steps_ / 1000 : 0),
                            (unsigned long long)beacons_sent_, (unsigned long long)beacons_rx_, (unsigned long long)plans_sent_,
                            (unsigned long long)plans_rx_, (unsigned long long)solves_started_, (unsigned long long)solver_late_,
                            (unsigned long long)window_misses_, (unsigned long long)corrupt_, (unsigned long long)tx_refused_));
    }

    Host host_;
    ScheduleMachine machine_;
    uint16_t self_{1};
    int peers_{2};
    size_t plan_bytes_{1238};
    int64_t solve_ns_{8'000'000};
    int64_t requested_ns_{0}, period_commit_ns_{0}, late_max_ns_{0}, late_sum_ns_{0};
    uint64_t steps_{0}, deadline_steps_{0}, beacons_sent_{0}, beacons_rx_{0}, plans_sent_{0}, plans_rx_{0}, solves_started_{0},
        solver_late_{0}, window_misses_{0}, corrupt_{0}, tx_refused_{0};
    bool plan_sent_for_period_{false};

    std::thread solver_;
    std::mutex mutex_;
    std::condition_variable job_cv_;
    bool job_{false}, stop_{false}, result_ready_{false};
};

const xgc2_module_desc kDesc = {XGC2_MODULE_ABI_MAJOR,
                                XGC2_MODULE_ABI_MINOR,
                                "test_tro_shaped",
                                "1.0.0",
                                kPorts,
                                2,
                                &Lifecycle<Agent>::create,
                                &Lifecycle<Agent>::configure,
                                &Lifecycle<Agent>::start,
                                &Lifecycle<Agent>::step,
                                &Lifecycle<Agent>::stop,
                                &Lifecycle<Agent>::destroy};

}  // namespace

XGC2_MODULE_EXPORT const xgc2_module_desc* xgc2_module_entry(void) {
    return &kDesc;
}
