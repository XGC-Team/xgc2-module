#pragma once

// The C++ side of an xgc2-module module, header-only: the typed view of the host's function table, the
// descriptor helper, sample and slot views, small utilities and the trampolines that turn an instance class
// into the module's lifecycle functions. It is the one copy every module of the platform builds on; the
// JSON configuration is in json_config.hpp and the in-test host in test_host.hpp.
//
// A module is plain C++ behind the C ABI of <xgc2/module.h>. Nothing here allocates on the data path, throws
// across the ABI or links a library.

#include <xgc2/module.h>

#include <array>
#include <cstdarg>
#include <cstdint>
#include <cstdio>
#include <cstring>
#include <exception>
#include <memory>
#include <stdexcept>
#include <string>
#include <thread>
#include <type_traits>

#define XGC2_MODULE_EXPORT extern "C" __attribute__((visibility("default")))

namespace xgc2 {
namespace sdk {

constexpr int kLogDebug = 0;
constexpr int kLogInfo = 1;
constexpr int kLogWarn = 2;
constexpr int kLogError = 3;
constexpr int kHealthOk = 0;
constexpr int kHealthDegraded = 1;
constexpr int kHealthFailed = 2;

// A port descriptor of payload type T (size and alignment come from the type).
template <typename T>
constexpr xgc2_port_desc port(const char* name, xgc2_port_direction direction, xgc2_port_kind kind,
                              const char* schema_id, uint32_t queue_depth = 0U,
                              uint32_t flags = 0U) {
    return xgc2_port_desc{name,
                          static_cast<uint32_t>(direction),
                          static_cast<uint32_t>(kind),
                          schema_id,
                          static_cast<uint32_t>(sizeof(T)),
                          static_cast<uint32_t>(alignof(T)),
                          queue_depth,
                          flags};
}

// A sample read from an input: the payload (borrowed until the step returns), the producer's stamp
// and the channel's sequence number.
template <typename T>
struct Sample {
    const T* data{nullptr};
    int64_t stamp_ns{0};
    uint64_t seq{0};

    explicit operator bool() const {
        return data != nullptr;
    }
    const T* operator->() const {
        return data;
    }
    const T& operator*() const {
        return *data;
    }
};

class Host;

// An output slot to fill in place, zeroed first (a slot holds the data of an older sample). It is
// published by commit(); a slot that goes out of scope without a commit is handed back.
template <typename T>
class Slot {
   public:
    Slot(const Host& host, uint32_t port);
    ~Slot();
    Slot(const Slot&) = delete;
    Slot& operator=(const Slot&) = delete;
    // A slot is returned from Host::write(): it moves (the moved-from slot owns nothing).
    Slot(Slot&& other) noexcept : host_(other.host_), port_(other.port_), data_(other.data_) {
        other.data_ = nullptr;
    }

    // False when the port is being written or its event queue is full.
    explicit operator bool() const {
        return data_ != nullptr;
    }
    T* operator->() {
        return data_;
    }
    T& operator*() {
        return *data_;
    }
    bool commit(int64_t stamp_ns);

   private:
    const Host* host_;
    uint32_t port_;
    T* data_;
};

// The host's function table of one instance. Reads are valid inside step only and the pointers
// they return are borrowed until the step returns.
class Host {
   public:
    Host() = default;
    Host(const xgc2_host_api* api, void* context) : api_(api), context_(context) {}

    // The host clock in nanoseconds (see "Time" in xgc2/module.h).
    int64_t nowNs() const {
        return api_->now_ns(context_);
    }
    // A step with XGC2_STEP_WAKE; callable from any thread.
    void wake() const {
        api_->wake(context_);
    }
    // One step with XGC2_STEP_DEADLINE once the host clock reaches `deadline_ns`; replaces the previous
    // request, a negative value cancels. Minor 1.
    void wakeAt(int64_t deadline_ns) const {
        api_->wake_at_ns(context_, deadline_ns);
    }
    // Origin of the timer grid in the host clock, 0 when the clock has none. Minor 1.
    int64_t phaseT0Ns() const {
        return api_->phase_t0_ns(context_);
    }
    // 0 disables the period timer.
    void setPeriodNs(int64_t period_ns) const {
        api_->set_period_ns(context_, period_ns);
    }
    void log(int level, const std::string& message) const {
        api_->log(context_, level, message.c_str());
    }
    void report(int health, const std::string& detail) const {
        api_->report(context_, health, detail.c_str());
    }

    // The newest sample of a state input; empty without data (or of another size).
    template <typename T>
    Sample<T> latest(uint32_t port_index) const {
        xgc2_sample_view view{};
        if (api_->read_latest(context_, port_index, &view) != XGC2_OK || view.size != sizeof(T)) {
            return {};
        }
        return {static_cast<const T*>(view.data), view.stamp_ns, view.seq};
    }
    // The next event of an event input; empty when there is none.
    template <typename T>
    Sample<T> next(uint32_t port_index) const {
        xgc2_sample_view view{};
        if (api_->read_next(context_, port_index, &view) != XGC2_OK || view.size != sizeof(T)) {
            return {};
        }
        return {static_cast<const T*>(view.data), view.stamp_ns, view.seq};
    }
    template <typename T>
    Slot<T> write(uint32_t port_index) const {
        return Slot<T>(*this, port_index);
    }

    void* begin(uint32_t port_index) const {
        return api_->write_begin(context_, port_index);
    }
    bool commit(uint32_t port_index, int64_t stamp_ns) const {
        return api_->write_commit(context_, port_index, stamp_ns) == XGC2_OK;
    }
    void abort(uint32_t port_index) const {
        api_->write_abort(context_, port_index);
    }

   private:
    const xgc2_host_api* api_{nullptr};
    void* context_{nullptr};
};

template <typename T>
Slot<T>::Slot(const Host& host, uint32_t port)
    : host_(&host), port_(port), data_(static_cast<T*>(host.begin(port))) {
    static_assert(std::is_trivially_copyable<T>::value, "payloads are plain data");
    if (data_ != nullptr) {
        std::memset(static_cast<void*>(data_), 0, sizeof(T));
    }
}

template <typename T>
Slot<T>::~Slot() {
    if (data_ != nullptr) {
        host_->abort(port_);
    }
}

template <typename T>
bool Slot<T>::commit(int64_t stamp_ns) {
    if (data_ == nullptr) {
        return false;
    }
    data_ = nullptr;
    return host_->commit(port_, stamp_ns);
}

// The newest sequence number taken from each state input, to tell a new sample from the one the
// previous step already took.
class SeenSamples {
   public:
    template <typename T>
    bool fresh(uint32_t port, const Sample<T>& sample) {
        if (!sample) {
            return false;
        }
        Entry& entry = entries_.at(port);
        if (entry.seen && entry.seq == sample.seq) {
            return false;
        }
        entry.seen = true;
        entry.seq = sample.seq;
        return true;
    }
    void forget() {
        entries_ = {};
    }

   private:
    struct Entry {
        bool seen{false};
        uint64_t seq{0};
    };
    std::array<Entry, XGC2_MODULE_MAX_PORTS> entries_{};
};

// A host of the same major version and at least the minor version this module was built against.
inline bool compatibleHost(const xgc2_host_api* host) {
    return host != nullptr && host->abi_major == XGC2_MODULE_ABI_MAJOR &&
           host->abi_minor >= XGC2_MODULE_ABI_MINOR;
}

// At most one message per `period` seconds of the caller's clock, for warnings that can repeat at
// the rate of the inputs.
class LogThrottle {
   public:
    explicit LogThrottle(double period = 1.0) : period_(period) {}
    bool due(double now_sec) {
        if (now_sec >= last_ && now_sec - last_ < period_) {
            return false;
        }
        last_ = now_sec;
        return true;
    }

   private:
    double period_;
    double last_{-1.0e300};
};

// "text %d" into a std::string.
inline std::string format(const char* pattern, ...) __attribute__((format(printf, 1, 2)));
inline std::string format(const char* pattern, ...) {
    va_list args;
    va_start(args, pattern);
    va_list measure;
    va_copy(measure, args);
    const int length = std::vsnprintf(nullptr, 0, pattern, measure);
    va_end(measure);
    std::string text(length > 0 ? static_cast<size_t>(length) : 0U, '\0');
    if (length > 0) {
        std::vsnprintf(&text[0], text.size() + 1U, pattern, args);
    }
    va_end(args);
    return text;
}

// The thread that built the thread-affine state of an instance (a core whose state machine belongs to the
// thread that built it and refuses every other one stops working without a sound). The host keeps an
// instance on one thread for its life (manifest `affinity = "sticky"`, the default); an instance checks
// that it has been: bind() where the state is built, check() where it is used. A host that moved the
// instance would stop it with a message that says why, not leave it half alive.
class ThreadGuard {
   public:
    // The calling thread built the state (again).
    void bind() {
        owner_ = std::this_thread::get_id();
    }

    // Throws unless the calling thread is the one that built the state.
    void check(const char* call) const {
        if (std::this_thread::get_id() != owner_) {
            throw std::runtime_error(
                std::string(call) +
                " came from another thread than the one that built the state; the instance needs "
                "the host's thread affinity (affinity = \"sticky\" in the manifest)");
        }
    }

   private:
    std::thread::id owner_;
};

// The lifecycle functions of a module whose instances are objects of class Instance:
//   Instance(const Host&)
//   void configure(const xgc2_config*)    first and live; throws std::invalid_argument for a
//                                          configuration it refuses, and then keeps the old one
//   void start();  void step(const xgc2_step_ctx&);  void stop();
//   const Host& host() const;
// An exception never crosses into the host: a refused configuration is XGC2_ERR_INVALID, any other
// failure XGC2_ERR_INTERNAL (and a failed step is reported, which stops the instance).
template <typename Instance>
struct Lifecycle {
    static Instance& self(xgc2_instance* handle) {
        return *reinterpret_cast<Instance*>(handle);
    }

    static xgc2_status create(const xgc2_host_api* api, void* context, const xgc2_config* config,
                              xgc2_instance** out) {
        if (!compatibleHost(api) || out == nullptr) {
            return XGC2_ERR_INVALID;
        }
        const Host host(api, context);
        std::unique_ptr<Instance> instance;
        try {
            instance.reset(new Instance(host));
            instance->configure(config);
        } catch (const std::invalid_argument& error) {
            host.log(kLogError, std::string("Invalid configuration: ") + error.what());
            return XGC2_ERR_INVALID;
        } catch (const std::exception& error) {
            host.log(kLogError, std::string("Cannot create the instance: ") + error.what());
            return XGC2_ERR_INTERNAL;
        }
        *out = reinterpret_cast<xgc2_instance*>(instance.release());
        return XGC2_OK;
    }

    static xgc2_status configure(xgc2_instance* handle, const xgc2_config* config) {
        Instance& instance = self(handle);
        try {
            instance.configure(config);
        } catch (const std::invalid_argument& error) {
            instance.host().log(kLogError, std::string("Invalid configuration: ") + error.what());
            return XGC2_ERR_INVALID;
        } catch (const std::exception& error) {
            instance.host().log(kLogError, std::string("Configuration failed: ") + error.what());
            return XGC2_ERR_INTERNAL;
        }
        return XGC2_OK;
    }

    static xgc2_status start(xgc2_instance* handle) {
        Instance& instance = self(handle);
        try {
            instance.start();
        } catch (const std::exception& error) {
            instance.host().log(kLogError, std::string("Cannot start: ") + error.what());
            return XGC2_ERR_INTERNAL;
        }
        return XGC2_OK;
    }

    static xgc2_status step(xgc2_instance* handle, const xgc2_step_ctx* ctx) {
        Instance& instance = self(handle);
        try {
            instance.step(*ctx);
        } catch (const std::exception& error) {
            instance.host().report(kHealthFailed, std::string("step failed: ") + error.what());
            return XGC2_ERR_INTERNAL;
        }
        return XGC2_OK;
    }

    static xgc2_status stop(xgc2_instance* handle) {
        Instance& instance = self(handle);
        try {
            instance.stop();
        } catch (const std::exception& error) {
            instance.host().log(kLogError, std::string("Cannot stop: ") + error.what());
            return XGC2_ERR_INTERNAL;
        }
        return XGC2_OK;
    }

    static void destroy(xgc2_instance* handle) {
        delete &self(handle);
    }
};

}  // namespace sdk
}  // namespace xgc2
