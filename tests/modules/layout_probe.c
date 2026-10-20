/* Sizes and offsets of the header's types as this compiler sees them. The Rust ABI mirror is
 * compared against these numbers in tests/abi_layout.rs. */
#include <stddef.h>

#include "common.h"

#define PROBES(X)                                                                                     \
  X(sizeof(xgc2_port_desc)) X(offsetof(xgc2_port_desc, direction)) X(offsetof(xgc2_port_desc, kind))  \
  X(offsetof(xgc2_port_desc, schema_id)) X(offsetof(xgc2_port_desc, size))                            \
  X(offsetof(xgc2_port_desc, align)) X(offsetof(xgc2_port_desc, queue_depth))                         \
  X(offsetof(xgc2_port_desc, flags))                                                                  \
  X(sizeof(xgc2_sample_view)) X(offsetof(xgc2_sample_view, size)) X(offsetof(xgc2_sample_view, seq))  \
  X(offsetof(xgc2_sample_view, stamp_ns))                                                             \
  X(sizeof(xgc2_step_ctx)) X(offsetof(xgc2_step_ctx, step_index))                                     \
  X(offsetof(xgc2_step_ctx, changed_inputs)) X(offsetof(xgc2_step_ctx, reasons))                      \
  X(sizeof(xgc2_config)) X(offsetof(xgc2_config, length))                                             \
  X(sizeof(xgc2_host_api)) X(offsetof(xgc2_host_api, abi_minor))                                      \
  X(offsetof(xgc2_host_api, write_begin)) X(offsetof(xgc2_host_api, write_commit))                    \
  X(offsetof(xgc2_host_api, write_abort)) X(offsetof(xgc2_host_api, read_latest))                     \
  X(offsetof(xgc2_host_api, read_next)) X(offsetof(xgc2_host_api, changed))                           \
  X(offsetof(xgc2_host_api, now_ns)) X(offsetof(xgc2_host_api, wake))                                 \
  X(offsetof(xgc2_host_api, set_period_ns)) X(offsetof(xgc2_host_api, log))                           \
  X(offsetof(xgc2_host_api, report)) X(offsetof(xgc2_host_api, wake_at_ns))                           \
  X(offsetof(xgc2_host_api, phase_t0_ns))                                                             \
  X(sizeof(xgc2_module_desc)) X(offsetof(xgc2_module_desc, abi_minor))                                \
  X(offsetof(xgc2_module_desc, name)) X(offsetof(xgc2_module_desc, version))                          \
  X(offsetof(xgc2_module_desc, ports)) X(offsetof(xgc2_module_desc, port_count))                      \
  X(offsetof(xgc2_module_desc, create)) X(offsetof(xgc2_module_desc, configure))                      \
  X(offsetof(xgc2_module_desc, start)) X(offsetof(xgc2_module_desc, step))                            \
  X(offsetof(xgc2_module_desc, stop)) X(offsetof(xgc2_module_desc, destroy))                          \
  X(XGC2_MODULE_ABI_MAJOR) X(XGC2_MODULE_ABI_MINOR) X(XGC2_MODULE_MAX_PORTS)                          \
  X(XGC2_OK) X(XGC2_ERR_INVALID) X(XGC2_ERR_STATE) X(XGC2_ERR_FULL) X(XGC2_ERR_NODATA)                \
  X(XGC2_ERR_INTERNAL) X(XGC2_PORT_IN) X(XGC2_PORT_OUT) X(XGC2_PORT_STATE) X(XGC2_PORT_EVENT)         \
  X(XGC2_PORT_REQUIRED) X(XGC2_PORT_ASYNC_WRITER) X(XGC2_STEP_INPUT) X(XGC2_STEP_TIMER)               \
  X(XGC2_STEP_WAKE) X(XGC2_STEP_CONFIG) X(XGC2_STEP_DEADLINE)

#define VALUE(expression) (unsigned long long)(expression),

static const unsigned long long VALUES[] = {PROBES(VALUE)};

TEST_EXPORT unsigned long long xgc2_probe_count(void) { return sizeof VALUES / sizeof VALUES[0]; }
TEST_EXPORT unsigned long long xgc2_probe(unsigned long long index) { return VALUES[index]; }
TEST_EXPORT const char *xgc2_probe_entry_symbol(void) { return XGC2_MODULE_ENTRY_SYMBOL; }
