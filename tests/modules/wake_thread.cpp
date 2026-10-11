// A module that owns a thread. From that thread it publishes through an ASYNC_WRITER port and
// calls wake(), the way an NMPC solver or a ROS spinner hands results to the host. The thread
// is joined in stop(), as the ABI requires. C++ on purpose: it proves the header from C++.
#include <atomic>
#include <chrono>
#include <cstdio>
#include <cstring>
#include <thread>

#include "common.h"

namespace {

struct Instance {
  const xgc2_host_api* host = nullptr;
  void* ctx = nullptr;
  long interval_us = 1000;
  std::atomic<bool> running{false};
  std::thread worker;
  std::atomic<uint64_t> ticks{0};
  uint64_t steps = 0, wake_steps = 0, input_steps = 0, last_tick_seen = 0, tick_regressions = 0;
};

const xgc2_port_desc PORTS[] = {
    {"ticks", XGC2_PORT_OUT, XGC2_PORT_STATE, SAMPLE_SCHEMA, sizeof(test_sample), 8, 0, XGC2_PORT_ASYNC_WRITER},
};

void run(Instance* self) {
  while (self->running.load()) {
    std::this_thread::sleep_for(std::chrono::microseconds(self->interval_us));
    auto* sample = static_cast<test_sample*>(self->host->write_begin(self->ctx, 0));
    if (sample) {
      std::memset(sample, 0, sizeof *sample);
      sample->counter = self->ticks.fetch_add(1) + 1;
      sample->addr = reinterpret_cast<uintptr_t>(sample);
      self->host->write_commit(self->ctx, 0, self->host->now_ns(self->ctx));
    }
    self->host->wake(self->ctx);
  }
}

xgc2_status create(const xgc2_host_api* host, void* ctx, const xgc2_config* config, xgc2_instance** out) {
  auto* self = new Instance;
  self->host = host;
  self->ctx = ctx;
  self->interval_us = static_cast<long>(cfg_number(config, "interval_us", 1000));
  *out = reinterpret_cast<xgc2_instance*>(self);
  return XGC2_OK;
}

xgc2_status configure(xgc2_instance*, const xgc2_config*) { return XGC2_OK; }

xgc2_status start(xgc2_instance* handle) {
  auto* self = reinterpret_cast<Instance*>(handle);
  self->running = true;
  self->worker = std::thread(run, self);
  return XGC2_OK;
}

xgc2_status step(xgc2_instance* handle, const xgc2_step_ctx* step_ctx) {
  auto* self = reinterpret_cast<Instance*>(handle);
  self->steps++;
  if (step_ctx->reasons & XGC2_STEP_WAKE) self->wake_steps++;
  if (step_ctx->reasons & XGC2_STEP_INPUT) self->input_steps++;
  char detail[160];
  std::snprintf(detail, sizeof detail, "{\"steps\":%llu,\"wake_steps\":%llu,\"input_steps\":%llu,\"ticks\":%llu}",
                static_cast<unsigned long long>(self->steps), static_cast<unsigned long long>(self->wake_steps),
                static_cast<unsigned long long>(self->input_steps),
                static_cast<unsigned long long>(self->ticks.load()));
  self->host->report(self->ctx, XGC2_OK, detail);
  return XGC2_OK;
}

xgc2_status stop(xgc2_instance* handle) {
  auto* self = reinterpret_cast<Instance*>(handle);
  self->running = false;
  if (self->worker.joinable()) self->worker.join();
  return XGC2_OK;
}

void destroy(xgc2_instance* handle) { delete reinterpret_cast<Instance*>(handle); }

const xgc2_module_desc DESC = {XGC2_MODULE_ABI_MAJOR, XGC2_MODULE_ABI_MINOR, "test_wake_thread", "1.0.0", PORTS, 1,
                               create, configure, start, step, stop, destroy};

}  // namespace

TEST_EXPORT const xgc2_module_desc* xgc2_module_entry(void) { return &DESC; }
