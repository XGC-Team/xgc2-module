#pragma once

// An in-test host for xgc2-module modules (tests only).
//
// It implements xgc2_host_api for one module instance without the real host: the test pushes input samples,
// sets the clock and calls step() itself, so a run is deterministic. It follows the rules of xgc2/module.h that
// matter to a module:
//  - read_latest, read_next and changed see the inputs as of the start of the step, and a view stays valid
//    until the step returns;
//  - write_begin hands out a slot filled with 0xA5 (a real slot holds stale data: a module must not rely on
//    zeroed memory), and a second write_begin before the commit returns NULL;
//  - a state input has one sample, an event input is a bounded FIFO that drops what does not fit;
//  - an output returns NULL from write_begin once its test-set capacity is used up (limitOutput), as a full
//    event queue does;
//  - wake, wake_at_ns, now_ns, phase_t0_ns, log, report and set_period_ns may be called from any thread, and the
//    write calls on a port flagged XGC2_PORT_ASYNC_WRITER too (the host serializes them here).
// Threading::kHomeWorker runs every lifecycle call and step of the instance on one fixed worker thread, as the
// host's default sticky affinity does; Threading::kCaller runs them on the thread of the test, which tests that
// are about threads (RotatingThreads) need. ModuleLibrary loads a module with dlopen, the way the host does.

#include <dlfcn.h>
#include <xgc2/module.h>

#include <atomic>
#include <chrono>
#include <condition_variable>
#include <cstdint>
#include <cstring>
#include <deque>
#include <functional>
#include <future>
#include <memory>
#include <mutex>
#include <set>
#include <stdexcept>
#include <string>
#include <thread>
#include <utility>
#include <vector>

namespace xgc2 {
namespace sdk {
namespace test {

// A module shared library loaded with dlopen.
class ModuleLibrary {
   public:
    explicit ModuleLibrary(const std::string& path) {
        handle_ = dlopen(path.c_str(), RTLD_NOW | RTLD_LOCAL);
        if (handle_ == nullptr) {
            throw std::runtime_error(std::string("dlopen: ") + dlerror());
        }
        auto entry =
            reinterpret_cast<xgc2_module_entry_fn>(dlsym(handle_, XGC2_MODULE_ENTRY_SYMBOL));
        if (entry == nullptr) {
            throw std::runtime_error("the library does not export xgc2_module_entry");
        }
        desc_ = entry();
    }
    ~ModuleLibrary() {
        if (handle_ != nullptr) {
            dlclose(handle_);
        }
    }
    ModuleLibrary(const ModuleLibrary&) = delete;
    ModuleLibrary& operator=(const ModuleLibrary&) = delete;

    const xgc2_module_desc* desc() const {
        return desc_;
    }

   private:
    void* handle_{nullptr};
    const xgc2_module_desc* desc_{nullptr};
};

struct Sample {
    Sample() = default;
    Sample(int64_t stamp, std::vector<uint8_t> payload) : stamp_ns(stamp), bytes(std::move(payload)) {}

    int64_t stamp_ns{0};
    std::vector<uint8_t> bytes;

    template <typename T>
    T as() const {
        if (bytes.size() != sizeof(T)) {
            throw std::runtime_error("payload size mismatch");
        }
        T value;
        std::memcpy(&value, bytes.data(), sizeof(T));
        return value;
    }
};

enum class Threading { kCaller, kHomeWorker };

class TestHost {
   public:
    explicit TestHost(const xgc2_module_desc* desc, Threading threading = Threading::kCaller)
        : desc_(desc), ports_(desc->port_count), threading_(threading) {
        api_.abi_major = XGC2_MODULE_ABI_MAJOR;
        api_.abi_minor = XGC2_MODULE_ABI_MINOR;
        api_.write_begin = &TestHost::writeBegin;
        api_.write_commit = &TestHost::writeCommit;
        api_.write_abort = &TestHost::writeAbort;
        api_.read_latest = &TestHost::readLatest;
        api_.read_next = &TestHost::readNext;
        api_.changed = &TestHost::changedPort;
        api_.now_ns = &TestHost::nowNs;
        api_.wake = &TestHost::wakeHost;
        api_.set_period_ns = &TestHost::setPeriod;
        api_.log = &TestHost::logMessage;
        api_.report = &TestHost::reportHealth;
        api_.wake_at_ns = &TestHost::wakeAt;
        api_.phase_t0_ns = &TestHost::phaseT0;
        uint32_t input_index = 0;
        for (uint32_t i = 0; i < desc->port_count; ++i) {
            ports_[i].desc = &desc->ports[i];
            if (desc->ports[i].direction == XGC2_PORT_IN) {
                ports_[i].input_index = input_index++;
            }
        }
        if (threading_ == Threading::kHomeWorker) {
            worker_ = std::thread([this] { workerLoop(); });
        }
    }
    ~TestHost() {
        destroy();
        if (worker_.joinable()) {
            {
                std::lock_guard<std::mutex> lock(worker_mutex_);
                worker_stopped_ = true;
            }
            worker_cv_.notify_one();
            worker_.join();
        }
    }
    TestHost(const TestHost&) = delete;
    TestHost& operator=(const TestHost&) = delete;

    const xgc2_module_desc* desc() const {
        return desc_;
    }
    const xgc2_host_api* api() const {
        return &api_;
    }
    // The table the module gets; a test may lower abi_minor to see how a module treats an older host.
    xgc2_host_api* mutableApi() {
        return &api_;
    }
    uint32_t port(const std::string& name) const {
        for (uint32_t i = 0; i < ports_.size(); ++i) {
            if (name == ports_[i].desc->name) {
                return i;
            }
        }
        throw std::runtime_error("no port " + name);
    }

    // ---- clock ----
    // The clock the module sees: the test's (setNow), the host's own CLOCK_MONOTONIC (`steady`) or CLOCK_REALTIME
    // (`utc`, what a module that talks to ROS needs).
    void setNow(int64_t ns) {
        clock_.store(kManual);
        now_ns_.store(ns);
    }
    void useMonotonicClock() {
        clock_.store(kMonotonic);
    }
    void useUtcClock() {
        clock_.store(kUtc);
    }
    int64_t now() const {
        switch (clock_.load()) {
            case kMonotonic:
                return monotonicNs();
            case kUtc:
                return utcNs();
            default:
                return now_ns_.load();
        }
    }
    static int64_t monotonicNs() {
        return std::chrono::duration_cast<std::chrono::nanoseconds>(
                   std::chrono::steady_clock::now().time_since_epoch())
            .count();
    }
    static int64_t utcNs() {
        return std::chrono::duration_cast<std::chrono::nanoseconds>(
                   std::chrono::system_clock::now().time_since_epoch())
            .count();
    }
    // What phase_t0_ns() answers (the origin of the host's timer grid; 0 by default).
    void setPhaseT0(int64_t ns) {
        phase_t0_ns_.store(ns);
    }

    // ---- lifecycle ----
    xgc2_status create(const std::string& json) {
        config_ = json;
        const xgc2_config config{config_.c_str(), config_.size()};
        return onWorker([&] { return desc_->create(&api_, this, &config, &instance_); });
    }
    xgc2_status configure(const std::string& json) {
        if (instance_ == nullptr) {
            return XGC2_ERR_STATE;
        }
        config_ = json;
        const xgc2_config config{config_.c_str(), config_.size()};
        return onWorker([&] { return desc_->configure(instance_, &config); });
    }
    xgc2_status start() {
        return instance_ == nullptr ? XGC2_ERR_STATE
                                    : onWorker([&] { return desc_->start(instance_); });
    }
    xgc2_status stop() {
        return instance_ == nullptr ? XGC2_ERR_STATE
                                    : onWorker([&] { return desc_->stop(instance_); });
    }
    void destroy() {
        if (instance_ != nullptr) {
            onWorker([&] { desc_->destroy(instance_); });
            instance_ = nullptr;
        }
    }
    bool created() const {
        return instance_ != nullptr;
    }
    xgc2_instance* instance() const {
        return instance_;
    }
    // The threads the module was called on. The host keeps an instance on one thread for its life (affinity
    // "sticky"), and a test that is not about threads makes every call from one.
    size_t callingThreads() const {
        std::lock_guard<std::mutex> lock(mutex_);
        return calling_threads_.size();
    }
    size_t workerThreadsUsed() const {
        return callingThreads();
    }
    // The worker of Threading::kHomeWorker (a default id for kCaller).
    std::thread::id homeWorkerThread() const {
        return worker_.get_id();
    }

    // One step at the current clock. changed_inputs is derived from the samples pushed since the previous step.
    xgc2_status step(uint32_t reasons = XGC2_STEP_TIMER) {
        if (instance_ == nullptr) {
            return XGC2_ERR_STATE;
        }
        xgc2_step_ctx ctx{};
        ctx.now_ns = now();
        ctx.step_index = step_index_++;
        ctx.reasons = reasons;
        {
            std::lock_guard<std::mutex> lock(mutex_);
            for (Port& p : ports_) {
                if (p.desc->direction != XGC2_PORT_IN) {
                    continue;
                }
                p.changed_in_step = p.dirty;
                p.dirty = false;
                if (p.changed_in_step) {
                    ctx.changed_inputs |= uint64_t{1} << p.input_index;
                }
                p.step_latest = p.latest;
                p.step_sequence = p.sequence;
            }
            in_step_ = true;
        }
        const xgc2_status status = onWorker([&] { return desc_->step(instance_, &ctx); });
        std::lock_guard<std::mutex> lock(mutex_);
        in_step_ = false;
        for (Port& p : ports_) {
            p.held.clear();
        }
        return status;
    }

    // ---- inputs ----
    // False when an event input is full (the sample is dropped, like the real host).
    bool push(uint32_t index, const void* data, uint32_t size, int64_t stamp_ns) {
        Port& p = ports_.at(index);
        if (p.desc->direction != XGC2_PORT_IN || size != p.desc->size) {
            throw std::runtime_error(std::string("bad push to ") + p.desc->name);
        }
        Sample sample;
        sample.stamp_ns = stamp_ns;
        sample.bytes.assign(static_cast<const uint8_t*>(data),
                            static_cast<const uint8_t*>(data) + size);
        std::lock_guard<std::mutex> lock(mutex_);
        if (p.desc->kind == XGC2_PORT_STATE) {
            p.latest = std::move(sample);
            p.has_latest = true;
            p.dirty = true;
            ++p.sequence;
            return true;
        }
        if (p.events.size() >= p.desc->queue_depth) {
            ++p.dropped;
            return false;
        }
        p.events.push_back(std::move(sample));
        p.dirty = true;
        ++p.sequence;
        return true;
    }
    template <typename T>
    bool push(const std::string& name, const T& value, int64_t stamp_ns) {
        return push(port(name), &value, sizeof(T), stamp_ns);
    }
    uint64_t dropped(const std::string& name) const {
        std::lock_guard<std::mutex> lock(mutex_);
        return ports_.at(port(name)).dropped;
    }

    // ---- outputs ----
    // Everything committed to the port since the last clearOutputs() or take().
    std::vector<Sample> outputs(const std::string& name) const {
        std::lock_guard<std::mutex> lock(mutex_);
        return ports_.at(port(name)).committed;
    }
    template <typename T>
    std::vector<T> outputsAs(const std::string& name) const {
        std::vector<T> result;
        for (const Sample& sample : outputs(name)) {
            result.push_back(sample.as<T>());
        }
        return result;
    }
    // The samples committed since the last take(), removed from the record.
    std::vector<Sample> take(const std::string& name) {
        std::lock_guard<std::mutex> lock(mutex_);
        Port& p = ports_.at(port(name));
        std::vector<Sample> samples;
        samples.swap(p.committed);
        return samples;
    }
    template <typename T>
    std::vector<T> takeAs(const std::string& name) {
        std::vector<T> result;
        for (const Sample& sample : take(name)) {
            result.push_back(sample.as<T>());
        }
        return result;
    }
    // Waits until the port holds at least `count` committed samples (for ports written by module threads).
    bool waitOutputs(const std::string& name, size_t count, std::chrono::milliseconds timeout) {
        const uint32_t index = port(name);
        std::unique_lock<std::mutex> lock(mutex_);
        return commit_cv_.wait_for(lock, timeout,
                                   [&] { return ports_[index].committed.size() >= count; });
    }
    void clearOutputs() {
        std::lock_guard<std::mutex> lock(mutex_);
        for (Port& p : ports_) {
            p.committed.clear();
        }
    }
    // The output accepts `count` more samples; after that write_begin returns NULL (counted in
    // refusedWrites()), like a full event queue.
    void limitOutput(const std::string& name, size_t count) {
        std::lock_guard<std::mutex> lock(mutex_);
        Port& p = ports_.at(port(name));
        p.write_limit = p.committed.size() + count;
    }
    uint64_t refusedWrites(const std::string& name) const {
        std::lock_guard<std::mutex> lock(mutex_);
        return ports_.at(port(name)).refused;
    }
    // Slots handed out and not yet committed or aborted (a module must not leak one).
    size_t openSlots() const {
        std::lock_guard<std::mutex> lock(mutex_);
        size_t open = 0;
        for (const Port& p : ports_) {
            open += p.writing ? 1U : 0U;
        }
        return open;
    }

    // ---- observation ----
    uint64_t wakeCount() const {
        return wakes_.load();
    }
    // The thread that called wake() last.
    std::thread::id lastWakeThread() const {
        std::lock_guard<std::mutex> lock(wake_mutex_);
        return wake_thread_;
    }
    // Waits until wakeCount() exceeds `seen`; false on timeout.
    bool waitWake(uint64_t seen, std::chrono::milliseconds timeout) {
        std::unique_lock<std::mutex> lock(wake_mutex_);
        return wake_cv_.wait_for(lock, timeout, [&] { return wakes_.load() > seen; });
    }
    int64_t periodNs() const {
        return period_ns_.load();
    }
    // Every wake_at_ns request in order (negative = cancel); the last one is the pending deadline.
    std::vector<int64_t> deadlineRequests() const {
        std::lock_guard<std::mutex> lock(mutex_);
        return deadlines_;
    }
    // The pending deadline (-1 without one): the last request.
    int64_t pendingDeadline() const {
        std::lock_guard<std::mutex> lock(mutex_);
        return deadlines_.empty() ? -1 : deadlines_.back();
    }
    // Waits until a deadline is pending; false on timeout.
    bool waitDeadline(std::chrono::milliseconds timeout) {
        std::unique_lock<std::mutex> lock(mutex_);
        return deadline_cv_.wait_for(lock, timeout,
                                     [&] { return !deadlines_.empty() && deadlines_.back() >= 0; });
    }
    std::vector<std::pair<int, std::string>> logs() const {
        std::lock_guard<std::mutex> lock(mutex_);
        return logs_;
    }
    bool logged(int level, const std::string& fragment) const {
        std::lock_guard<std::mutex> lock(mutex_);
        for (const auto& entry : logs_) {
            if (entry.first == level && entry.second.find(fragment) != std::string::npos) {
                return true;
            }
        }
        return false;
    }
    std::vector<std::pair<int, std::string>> reports() const {
        std::lock_guard<std::mutex> lock(mutex_);
        return reports_;
    }
    std::string lastReportDetail() const {
        std::lock_guard<std::mutex> lock(mutex_);
        return reports_.empty() ? std::string() : reports_.back().second;
    }

   private:
    struct Port {
        const xgc2_port_desc* desc{nullptr};
        uint32_t input_index{0};
        // inputs
        Sample latest;
        Sample step_latest;
        bool has_latest{false};
        bool dirty{false};
        bool changed_in_step{false};
        uint64_t sequence{0};
        uint64_t step_sequence{0};
        std::deque<Sample> events;  // pending event samples, the front is next
        std::deque<Sample> held;    // event samples read in the current step: their views stay valid
        uint64_t read_sequence{0};
        uint64_t dropped{0};
        // outputs
        std::vector<uint8_t> slot;
        bool writing{false};
        std::vector<Sample> committed;
        size_t write_limit{SIZE_MAX};
        uint64_t refused{0};
    };

    static TestHost& self(void* ctx) {
        return *static_cast<TestHost*>(ctx);
    }

    static void* writeBegin(void* ctx, uint32_t index) {
        TestHost& h = self(ctx);
        std::lock_guard<std::mutex> lock(h.mutex_);
        if (index >= h.ports_.size()) {
            return nullptr;
        }
        Port& p = h.ports_[index];
        if (p.desc->direction != XGC2_PORT_OUT || p.writing) {
            return nullptr;
        }
        if (p.committed.size() >= p.write_limit) {
            ++p.refused;
            return nullptr;
        }
        p.slot.assign(p.desc->size, 0xA5);
        p.writing = true;
        return p.slot.data();
    }
    static xgc2_status writeCommit(void* ctx, uint32_t index, int64_t stamp_ns) {
        TestHost& h = self(ctx);
        {
            std::lock_guard<std::mutex> lock(h.mutex_);
            if (index >= h.ports_.size() || !h.ports_[index].writing) {
                return XGC2_ERR_STATE;
            }
            Port& p = h.ports_[index];
            p.writing = false;
            p.committed.emplace_back(stamp_ns, p.slot);
        }
        h.commit_cv_.notify_all();
        return XGC2_OK;
    }
    static void writeAbort(void* ctx, uint32_t index) {
        TestHost& h = self(ctx);
        std::lock_guard<std::mutex> lock(h.mutex_);
        if (index < h.ports_.size()) {
            h.ports_[index].writing = false;
        }
    }
    static xgc2_status readLatest(void* ctx, uint32_t index, xgc2_sample_view* out) {
        TestHost& h = self(ctx);
        std::lock_guard<std::mutex> lock(h.mutex_);
        if (!h.in_step_ || index >= h.ports_.size()) {
            return XGC2_ERR_STATE;
        }
        Port& p = h.ports_[index];
        if (p.desc->direction != XGC2_PORT_IN || p.desc->kind != XGC2_PORT_STATE) {
            return XGC2_ERR_INVALID;
        }
        if (!p.has_latest) {
            return XGC2_ERR_NODATA;
        }
        out->data = p.step_latest.bytes.data();
        out->size = static_cast<uint32_t>(p.step_latest.bytes.size());
        out->seq = p.step_sequence;
        out->stamp_ns = p.step_latest.stamp_ns;
        return XGC2_OK;
    }
    static xgc2_status readNext(void* ctx, uint32_t index, xgc2_sample_view* out) {
        TestHost& h = self(ctx);
        std::lock_guard<std::mutex> lock(h.mutex_);
        if (!h.in_step_ || index >= h.ports_.size()) {
            return XGC2_ERR_STATE;
        }
        Port& p = h.ports_[index];
        if (p.desc->direction != XGC2_PORT_IN || p.desc->kind != XGC2_PORT_EVENT) {
            return XGC2_ERR_INVALID;
        }
        if (p.events.empty()) {
            return XGC2_ERR_NODATA;
        }
        // The view stays valid until the end of the step: keep the sample until then.
        p.held.push_back(std::move(p.events.front()));
        p.events.pop_front();
        const Sample& sample = p.held.back();
        out->data = sample.bytes.data();
        out->size = static_cast<uint32_t>(sample.bytes.size());
        out->seq = ++p.read_sequence;
        out->stamp_ns = sample.stamp_ns;
        return XGC2_OK;
    }
    static int changedPort(void* ctx, uint32_t index) {
        TestHost& h = self(ctx);
        std::lock_guard<std::mutex> lock(h.mutex_);
        if (!h.in_step_ || index >= h.ports_.size()) {
            return 0;
        }
        return h.ports_[index].changed_in_step ? 1 : 0;
    }
    static int64_t nowNs(void* ctx) {
        return self(ctx).now();
    }
    static void wakeHost(void* ctx) {
        TestHost& h = self(ctx);
        {
            std::lock_guard<std::mutex> lock(h.wake_mutex_);
            h.wake_thread_ = std::this_thread::get_id();
            h.wakes_.fetch_add(1);
        }
        h.wake_cv_.notify_all();
    }
    static void wakeAt(void* ctx, int64_t deadline_ns) {
        TestHost& h = self(ctx);
        {
            std::lock_guard<std::mutex> lock(h.mutex_);
            h.deadlines_.push_back(deadline_ns);
        }
        h.deadline_cv_.notify_all();
    }
    static int64_t phaseT0(void* ctx) {
        return self(ctx).phase_t0_ns_.load();
    }
    static void setPeriod(void* ctx, int64_t period_ns) {
        self(ctx).period_ns_.store(period_ns);
    }
    static void logMessage(void* ctx, int level, const char* message) {
        TestHost& h = self(ctx);
        std::lock_guard<std::mutex> lock(h.mutex_);
        h.logs_.emplace_back(level, message == nullptr ? "" : message);
    }
    static void reportHealth(void* ctx, int health, const char* detail) {
        TestHost& h = self(ctx);
        std::lock_guard<std::mutex> lock(h.mutex_);
        h.reports_.emplace_back(health, detail == nullptr ? "" : detail);
    }

    void noteCallingThread() {
        std::lock_guard<std::mutex> lock(mutex_);
        calling_threads_.insert(std::this_thread::get_id());
    }

    // Runs a module call on the thread the threading mode names and waits for it.
    template <typename F>
    auto onWorker(F&& call) -> decltype(call()) {
        if (threading_ == Threading::kCaller) {
            noteCallingThread();
            return call();
        }
        auto task = std::make_shared<std::packaged_task<decltype(call())()>>(std::forward<F>(call));
        auto result = task->get_future();
        {
            std::lock_guard<std::mutex> lock(worker_mutex_);
            worker_call_ = [this, task] {
                noteCallingThread();
                (*task)();
            };
        }
        worker_cv_.notify_one();
        return result.get();
    }

    void workerLoop() {
        std::unique_lock<std::mutex> lock(worker_mutex_);
        for (;;) {
            worker_cv_.wait(lock, [&] { return worker_stopped_ || worker_call_; });
            if (worker_stopped_) {
                return;
            }
            std::function<void()> call;
            call.swap(worker_call_);
            lock.unlock();
            call();
            lock.lock();
        }
    }

    const xgc2_module_desc* desc_;
    xgc2_host_api api_{};
    xgc2_instance* instance_{nullptr};
    std::vector<Port> ports_;
    Threading threading_;
    std::string config_;
    std::atomic<int64_t> now_ns_{0};
    enum ClockKind { kManual, kMonotonic, kUtc };
    std::atomic<int> clock_{kManual};
    std::atomic<int64_t> period_ns_{0};
    std::atomic<int64_t> phase_t0_ns_{0};
    std::atomic<uint64_t> wakes_{0};
    uint64_t step_index_{0};
    bool in_step_{false};
    mutable std::mutex mutex_;
    mutable std::mutex wake_mutex_;
    std::condition_variable wake_cv_;
    std::condition_variable commit_cv_;
    std::condition_variable deadline_cv_;
    std::thread::id wake_thread_;
    std::vector<int64_t> deadlines_;
    std::vector<std::pair<int, std::string>> logs_;
    std::vector<std::pair<int, std::string>> reports_;
    std::set<std::thread::id> calling_threads_;
    std::thread worker_;
    std::mutex worker_mutex_;
    std::condition_variable worker_cv_;
    std::function<void()> worker_call_;
    bool worker_stopped_{false};
};

// Runs jobs on a ring of threads that all live as long as the object: the next job goes to the next thread, so
// that two calls of one instance never share a thread id, as when the host's workers take an instance's calls in
// turn.
class RotatingThreads {
   public:
    explicit RotatingThreads(size_t count) : slots_(count) {
        for (size_t i = 0; i < count; ++i) {
            threads_.emplace_back([this, i] { loop(slots_[i]); });
        }
    }
    ~RotatingThreads() {
        for (Slot& slot : slots_) {
            {
                std::lock_guard<std::mutex> lock(slot.mutex);
                slot.quit = true;
            }
            slot.cv.notify_one();
        }
        for (std::thread& thread : threads_) {
            thread.join();
        }
    }
    RotatingThreads(const RotatingThreads&) = delete;
    RotatingThreads& operator=(const RotatingThreads&) = delete;

    void run(const std::function<void()>& job) {
        Slot& slot = slots_[next_++ % slots_.size()];
        std::unique_lock<std::mutex> lock(slot.mutex);
        slot.job = &job;
        slot.cv.notify_one();
        slot.done.wait(lock, [&] { return slot.job == nullptr; });
    }

   private:
    struct Slot {
        std::mutex mutex;
        std::condition_variable cv;
        std::condition_variable done;
        const std::function<void()>* job{nullptr};
        bool quit{false};
    };
    void loop(Slot& slot) {
        std::unique_lock<std::mutex> lock(slot.mutex);
        for (;;) {
            slot.cv.wait(lock, [&] { return slot.quit || slot.job != nullptr; });
            if (slot.job == nullptr) {
                return;
            }
            const std::function<void()>* job = slot.job;
            lock.unlock();
            (*job)();
            lock.lock();
            slot.job = nullptr;
            slot.done.notify_one();
        }
    }

    std::vector<Slot> slots_;
    std::vector<std::thread> threads_;
    size_t next_{0};
};

}  // namespace test
}  // namespace sdk
}  // namespace xgc2
