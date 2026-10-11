/*
 * xgc2/module.h - xgc2-module module ABI (binding contract for #226/#219).
 * ABI major 2: the successor of the sync-runtime plugin ABI 1.x (incompatible change, normal major bump).
 *
 * A module is a shared library exporting `const xgc2_module_desc* xgc2_module_entry(void)`.
 * All descriptor strings and the port table must stay valid while the library is loaded.
 *
 * Lifecycle: create, then any number of configure calls, start, step calls, stop, destroy.
 * start/stop may repeat; configure may also arrive between steps or after stop. A module that
 * owns threads must have stopped them before stop() returns; nothing may call the host API after
 * destroy() returns.
 *
 * Threading rules:
 * - create/configure/start/step/stop/destroy are called by host worker threads, never concurrently
 *   for the same instance (an instance is a serial task). Different instances may run in parallel.
 *   The default sticky affinity keeps all calls on one worker; any affinity permits migration.
 *   Replacing a hung worker changes the OS thread while retaining its pool slot.
 * - write_begin/write_commit/write_abort may be called from step, or from module-owned threads for
 *   output ports flagged XGC2_PORT_ASYNC_WRITER (one writer thread at a time per port).
 * - read_latest/read_next/changed are only valid inside step; views are borrowed until step returns.
 * - now_ns, wake, log, report and set_period_ns may be called from any thread.
 *
 * Port numbering: every host API call that takes `port` uses the index into the descriptor's port
 * table (inputs and outputs share one numbering). Only xgc2_step_ctx.changed_inputs numbers the
 * inputs on their own: bit i is the i-th input port in port table order.
 *
 * Data rules:
 * - Payloads are fixed-size POD structs (no pointers) defined in the producing product's public header
 *   and identified by schema_id; the host checks schema_id, size and align when binding channels.
 * - State ports carry the latest value (readers always get the newest complete sample; writers never wait).
 * - Event ports are bounded FIFOs, lossless until full; write_begin returns NULL when full (counted drop).
 *   Event channels accept several writers and several readers (each reader has its own cursor).
 * - write_begin returns a pointer into a host-owned slot that stays valid until write_commit or
 *   write_abort; a second write_begin on the same port before that returns NULL. An output port the
 *   manifest does not connect is bound to a private channel, so it never fails for that reason.
 * - An input port without a producer reports XGC2_ERR_NODATA from read_latest/read_next. Within one
 *   step, read_latest returns the same sample on every call; read_next returns successive events.
 * - Configuration is a JSON object (UTF-8 text, NUL-terminated; length excludes the NUL) passed to
 *   create and configure.
 *
 * Versioning: minor versions only append to xgc2_host_api. A module may use an entry only if
 * host->abi_minor covers it, and the host refuses a module whose abi_minor exceeds its own.
 */
#ifndef XGC2_MODULE_H
#define XGC2_MODULE_H

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

#define XGC2_MODULE_ABI_MAJOR 2u
#define XGC2_MODULE_ABI_MINOR 0u
#define XGC2_MODULE_ENTRY_SYMBOL "xgc2_module_entry"
#define XGC2_MODULE_MAX_PORTS 64u

typedef enum xgc2_status {
  XGC2_OK = 0,
  XGC2_ERR_INVALID = 1,  /* bad argument or configuration */
  XGC2_ERR_STATE = 2,    /* call not allowed in the current state */
  XGC2_ERR_FULL = 3,     /* event queue full */
  XGC2_ERR_NODATA = 4,   /* no sample available */
  XGC2_ERR_INTERNAL = 5  /* module failure; the host reports the instance as failed */
} xgc2_status;

typedef enum xgc2_port_direction { XGC2_PORT_IN = 1, XGC2_PORT_OUT = 2 } xgc2_port_direction;
typedef enum xgc2_port_kind { XGC2_PORT_STATE = 1, XGC2_PORT_EVENT = 2 } xgc2_port_kind;

#define XGC2_PORT_REQUIRED 0x1u     /* input: the instance is not ready until a producer is bound */
#define XGC2_PORT_ASYNC_WRITER 0x2u /* output: written from a module-owned thread */

typedef struct xgc2_port_desc {
  const char* name;      /* unique in the module, [a-z0-9_]{1,63} */
  uint32_t direction;    /* xgc2_port_direction */
  uint32_t kind;         /* xgc2_port_kind */
  const char* schema_id; /* e.g. "xgc2.ugv.unicycle_reference.active.v1" */
  uint32_t size;         /* payload size in bytes */
  uint32_t align;        /* payload alignment, power of two <= 64 */
  uint32_t queue_depth;  /* event ports: >= 1; state ports: 0 */
  uint32_t flags;        /* XGC2_PORT_* */
} xgc2_port_desc;

typedef struct xgc2_sample_view {
  const void* data; /* borrowed until the end of the current step */
  uint32_t size;
  uint64_t seq;     /* channel sequence number, strictly increasing */
  int64_t stamp_ns; /* producer stamp in the host clock domain */
} xgc2_sample_view;

#define XGC2_STEP_INPUT 0x1u  /* at least one input changed */
#define XGC2_STEP_TIMER 0x2u  /* the instance period elapsed */
#define XGC2_STEP_WAKE 0x4u   /* wake() was called */
#define XGC2_STEP_CONFIG 0x8u /* configure() was applied since the last step */

typedef struct xgc2_step_ctx {
  int64_t now_ns;          /* host clock when the step started */
  uint64_t step_index;
  uint64_t changed_inputs; /* bit i: input port with index i (port table order among inputs) changed */
  uint32_t reasons;        /* XGC2_STEP_* */
} xgc2_step_ctx;

typedef struct xgc2_config {
  const char* json; /* UTF-8 JSON object */
  size_t length;
} xgc2_config;

typedef struct xgc2_host_api {
  uint32_t abi_major;
  uint32_t abi_minor;
  void* (*write_begin)(void* host_ctx, uint32_t port);
  xgc2_status (*write_commit)(void* host_ctx, uint32_t port, int64_t stamp_ns);
  void (*write_abort)(void* host_ctx, uint32_t port);
  xgc2_status (*read_latest)(void* host_ctx, uint32_t port, xgc2_sample_view* out);
  xgc2_status (*read_next)(void* host_ctx, uint32_t port, xgc2_sample_view* out);
  int (*changed)(void* host_ctx, uint32_t port);
  int64_t (*now_ns)(void* host_ctx);
  void (*wake)(void* host_ctx);
  void (*set_period_ns)(void* host_ctx, int64_t period_ns); /* 0 disables the timer */
  void (*log)(void* host_ctx, int level, const char* message); /* 0 debug, 1 info, 2 warn, 3 error */
  void (*report)(void* host_ctx, int health, const char* detail); /* 0 ok, 1 degraded, 2 failed */
} xgc2_host_api;

typedef struct xgc2_instance xgc2_instance;

typedef struct xgc2_module_desc {
  uint32_t abi_major;
  uint32_t abi_minor;
  const char* name;
  const char* version;
  const xgc2_port_desc* ports;
  uint32_t port_count;
  xgc2_status (*create)(const xgc2_host_api* host, void* host_ctx, const xgc2_config* config,
                        xgc2_instance** out);
  xgc2_status (*configure)(xgc2_instance* instance, const xgc2_config* config);
  xgc2_status (*start)(xgc2_instance* instance);
  xgc2_status (*step)(xgc2_instance* instance, const xgc2_step_ctx* ctx);
  xgc2_status (*stop)(xgc2_instance* instance);
  void (*destroy)(xgc2_instance* instance);
} xgc2_module_desc;

typedef const xgc2_module_desc* (*xgc2_module_entry_fn)(void);

#ifdef __cplusplus
}
#endif

#endif /* XGC2_MODULE_H */
