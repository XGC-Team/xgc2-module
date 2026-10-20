/* ABI minor 1 probe: wake_at_ns, XGC2_STEP_DEADLINE, phase_t0_ns and the utc clock.
 *
 * Configuration (numbers, milliseconds of the host clock):
 *   deadline_after_ms  start() requests a deadline this far ahead (0: none)
 *   rearm_ms           every deadline step requests the next one this far ahead (0: stop)
 *   replace_after_ms   start() first requests a far deadline, then this one, which must replace it
 *   cancel             start() requests a deadline and cancels it at once
 *   past               start() requests a deadline 1 ms in the past
 * The detail reported by every step is a JSON object with the counters of the step reasons, the
 * smallest and largest lateness (step time minus requested instant) of the deadline steps, the
 * phase the host announces and the last clock reading. */
#include "common.h"

typedef struct instance {
  const xgc2_host_api* host;
  void* ctx;
  long deadline_after_ms, rearm_ms, replace_after_ms, cancel, past;
  int64_t requested_ns;
  uint64_t steps, deadline_steps, timer_steps, wake_steps, input_steps;
  int64_t min_late_ns, max_late_ns, last_now_ns;
} instance;

static const xgc2_port_desc PORTS[] = {
    {"in", XGC2_PORT_IN, XGC2_PORT_STATE, SAMPLE_SCHEMA, sizeof(test_sample), 8, 0, 0},
};

static xgc2_status create(const xgc2_host_api* host, void* ctx, const xgc2_config* config, xgc2_instance** out) {
  if (host->abi_minor < 1) return XGC2_ERR_INVALID; /* wake_at_ns needs minor 1 */
  instance* self = (instance*)calloc(1, sizeof *self);
  if (!self) return XGC2_ERR_INTERNAL;
  self->host = host;
  self->ctx = ctx;
  self->deadline_after_ms = (long)cfg_number(config, "deadline_after_ms", 0);
  self->rearm_ms = (long)cfg_number(config, "rearm_ms", 0);
  self->replace_after_ms = (long)cfg_number(config, "replace_after_ms", 0);
  self->cancel = (long)cfg_number(config, "cancel", 0);
  self->past = (long)cfg_number(config, "past", 0);
  self->min_late_ns = INT64_MAX;
  *out = (xgc2_instance*)self;
  return XGC2_OK;
}

static xgc2_status configure(xgc2_instance* handle, const xgc2_config* config) {
  (void)handle;
  (void)config;
  return XGC2_OK;
}

static void request(instance* self, int64_t at_ns) {
  self->requested_ns = at_ns;
  self->host->wake_at_ns(self->ctx, at_ns);
}

static xgc2_status start(xgc2_instance* handle) {
  instance* self = (instance*)handle;
  int64_t now = self->host->now_ns(self->ctx);
  if (self->replace_after_ms > 0) {
    self->host->wake_at_ns(self->ctx, now + 3600LL * 1000000000LL);
    request(self, now + self->replace_after_ms * 1000000LL);
  } else if (self->cancel) {
    self->host->wake_at_ns(self->ctx, now + 20 * 1000000LL);
    self->host->wake_at_ns(self->ctx, -1);
  } else if (self->past) {
    request(self, now - 1000000LL);
  } else if (self->deadline_after_ms > 0) {
    request(self, now + self->deadline_after_ms * 1000000LL);
  }
  return XGC2_OK;
}

static xgc2_status step(xgc2_instance* handle, const xgc2_step_ctx* step_ctx) {
  instance* self = (instance*)handle;
  self->steps++;
  self->last_now_ns = step_ctx->now_ns;
  if (step_ctx->reasons & XGC2_STEP_TIMER) self->timer_steps++;
  if (step_ctx->reasons & XGC2_STEP_WAKE) self->wake_steps++;
  if (step_ctx->reasons & XGC2_STEP_INPUT) self->input_steps++;
  if (step_ctx->reasons & XGC2_STEP_DEADLINE) {
    self->deadline_steps++;
    int64_t late = step_ctx->now_ns - self->requested_ns;
    if (late < self->min_late_ns) self->min_late_ns = late;
    if (late > self->max_late_ns) self->max_late_ns = late;
    if (self->rearm_ms > 0) request(self, self->host->now_ns(self->ctx) + self->rearm_ms * 1000000LL);
  }
  char detail[320];
  snprintf(detail, sizeof detail,
           "{\"steps\":%llu,\"deadline_steps\":%llu,\"timer_steps\":%llu,\"wake_steps\":%llu,\"input_steps\":%llu,"
           "\"min_late_ns\":%lld,\"max_late_ns\":%lld,\"phase_t0_ns\":%lld,\"last_now_ns\":%lld}",
           (unsigned long long)self->steps, (unsigned long long)self->deadline_steps,
           (unsigned long long)self->timer_steps, (unsigned long long)self->wake_steps,
           (unsigned long long)self->input_steps, (long long)(self->deadline_steps ? self->min_late_ns : 0),
           (long long)self->max_late_ns, (long long)self->host->phase_t0_ns(self->ctx), (long long)self->last_now_ns);
  self->host->report(self->ctx, XGC2_OK, detail);
  return XGC2_OK;
}

static xgc2_status stop(xgc2_instance* handle) {
  (void)handle;
  return XGC2_OK;
}

static void destroy(xgc2_instance* handle) { free(handle); }

static const xgc2_module_desc DESC = {XGC2_MODULE_ABI_MAJOR, XGC2_MODULE_ABI_MINOR, "test_deadline_probe", "1.0.0",
                                      PORTS, 1, create, configure, start, step, stop, destroy};

TEST_EXPORT const xgc2_module_desc* xgc2_module_entry(void) { return &DESC; }
