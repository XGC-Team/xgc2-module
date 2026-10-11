/* Records OS thread identity for every lifecycle call, including destruction. */
#include "common.h"
#include <pthread.h>
#include <stdatomic.h>
#include <sys/syscall.h>
#include <unistd.h>

typedef struct instance {
  const xgc2_host_api* host;
  void* ctx;
  long home_tid, last_tid;
  uint64_t moves, steps;
  unsigned calls;
  int strict, wake_ms, thread_started;
  atomic_int quit;
  pthread_t thread;
  char trace[512];
} instance;

static const xgc2_port_desc PORTS[] = {
    {"in", XGC2_PORT_IN, XGC2_PORT_STATE, SAMPLE_SCHEMA, sizeof(test_sample), 8, 0, 0},
    {"out", XGC2_PORT_OUT, XGC2_PORT_STATE, SAMPLE_SCHEMA, sizeof(test_sample), 8, 0, 0},
};

static xgc2_status visit(instance* self, const char* call, unsigned bit) {
  long tid = syscall(SYS_gettid);
  if (self->trace[0]) {
    FILE* file = fopen(self->trace, "a");
    if (!file) { perror(self->trace); return XGC2_ERR_INTERNAL; }
    fprintf(file, "%s %ld\n", call, tid);
    fclose(file);
  }
  if (self->last_tid && tid != self->last_tid) self->moves++;
  self->last_tid = tid;
  self->calls |= bit;
  char detail[256];
  snprintf(detail, sizeof detail, "{\"tid\":%ld,\"moves\":%llu,\"calls\":%u,\"steps\":%llu}",
           tid, (unsigned long long)self->moves, self->calls, (unsigned long long)self->steps);
  self->host->report(self->ctx, 0, detail);
  if (self->strict && tid != self->home_tid) {
    fprintf(stderr, "thread probe %s moved from OS thread %ld to %ld\n", call, self->home_tid, tid);
    return XGC2_ERR_INTERNAL;
  }
  return XGC2_OK;
}

static void* waker(void* argument) {
  instance* self = (instance*)argument;
  while (!atomic_load(&self->quit)) {
    pause_us((long)self->wake_ms * 1000);
    if (!atomic_load(&self->quit)) self->host->wake(self->ctx);
  }
  return NULL;
}

static xgc2_status create(const xgc2_host_api* host, void* ctx, const xgc2_config* config, xgc2_instance** out) {
  instance* self = (instance*)calloc(1, sizeof *self);
  if (!self) return XGC2_ERR_INTERNAL;
  self->host = host;
  self->ctx = ctx;
  self->home_tid = syscall(SYS_gettid);
  self->strict = (int)cfg_number(config, "strict", 1);
  self->wake_ms = (int)cfg_number(config, "wake_ms", 0);
  atomic_init(&self->quit, 0);
  cfg_string(config, "trace", self->trace, sizeof self->trace);
  xgc2_status status = visit(self, "create", 1);
  if (status != XGC2_OK) { free(self); return status; }
  *out = (xgc2_instance*)self;
  return XGC2_OK;
}

static xgc2_status configure(xgc2_instance* handle, const xgc2_config* config) {
  instance* self = (instance*)handle;
  self->strict = (int)cfg_number(config, "strict", self->strict);
  return visit(self, "configure", 2);
}

static xgc2_status start(xgc2_instance* handle) {
  instance* self = (instance*)handle;
  xgc2_status status = visit(self, "start", 4);
  if (status != XGC2_OK) return status;
  if (self->wake_ms > 0) {
    atomic_store(&self->quit, 0);
    if (pthread_create(&self->thread, NULL, waker, self) != 0) return XGC2_ERR_INTERNAL;
    self->thread_started = 1;
  }
  return XGC2_OK;
}

static xgc2_status step(xgc2_instance* handle, const xgc2_step_ctx* ctx) {
  instance* self = (instance*)handle;
  self->steps++;
  xgc2_status status = visit(self, "step", 8);
  if (status != XGC2_OK) return status;
  xgc2_sample_view view;
  uint64_t counter = self->steps;
  if (self->host->read_latest(self->ctx, 0, &view) == XGC2_OK)
    counter = ((const test_sample*)view.data)->counter;
  test_sample* sample = (test_sample*)self->host->write_begin(self->ctx, 1);
  if (sample) {
    memset(sample, 0, sizeof *sample);
    sample->counter = counter;
    self->host->write_commit(self->ctx, 1, ctx->now_ns);
  }
  return XGC2_OK;
}

static xgc2_status stop(xgc2_instance* handle) {
  instance* self = (instance*)handle;
  if (self->thread_started) {
    atomic_store(&self->quit, 1);
    pthread_join(self->thread, NULL);
    self->thread_started = 0;
  }
  return visit(self, "stop", 16);
}

static void destroy(xgc2_instance* handle) {
  instance* self = (instance*)handle;
  if (visit(self, "destroy", 32) != XGC2_OK) abort();
  free(self);
}

static const xgc2_module_desc DESC = {XGC2_MODULE_ABI_MAJOR, XGC2_MODULE_ABI_MINOR, "test_thread_probe", "1.0.0",
                                     PORTS, 2, create, configure, start, step, stop, destroy};

TEST_EXPORT const xgc2_module_desc* xgc2_module_entry(void) { return &DESC; }
