#include "lib/shim/shim_syscall.h"

#include <alloca.h>
#include <assert.h>
#include <errno.h>
#include <stdio.h>
#include <stdlib.h>
#include <stdint.h>
#include <inttypes.h>
#include <string.h>
#include <sys/syscall.h>

#include "lib/logger/logger.h"
#include "lib/shadow-shim-helper-rs/shim_helper.h"
#include "lib/shim/shim.h"
#include "lib/shim/shim_api.h"
#include "lib/shim/shim_seccomp.h"
#include "lib/shim/shim_sys.h"
#include "lib/shim/shim_tls.h"

// Syscall 统计：在 shim 一侧统计所有通过 shim_syscall 的系统调用次数。
// 为降低对行为的影响，目前只做计数，不测量时间。
// 可通过 ENABLE_PERF_LOGGING 宏启用
#ifdef ENABLE_PERF_LOGGING
static _Atomic uint64_t g_shim_syscall_count = 0;
#define SHIM_SYSCALL_LOG_EVERY 100000
#endif

// Handle to the real syscall function, initialized once at load-time for
// thread-safety.
long shim_native_syscall(ucontext_t* ctx, long n, ...) {
    va_list args;
    va_start(args, n);
    long rv = shim_native_syscallv(n, args);
    va_end(args);
    return rv;
}

long shim_emulated_syscall(ucontext_t* ctx, long n, ...) {
    va_list(args);
    va_start(args, n);
    long rv = shim_emulated_syscallv(ctx, n, args);
    va_end(args);
    return rv;
}

long shim_syscallv(ucontext_t* ctx, ExecutionContext exe_ctx, long n, va_list args) {
    shim_ensure_init();

    long rv;

    if (exe_ctx == EXECUTION_CONTEXT_APPLICATION && shim_sys_handle_syscall_locally(n, &rv, args)) {
        // No inter-process syscall needed, we handled it on the shim side! :)
        trace("Handled syscall %ld from the shim; we avoided inter-process overhead.", n);
        // rv was already set
    } else if ((exe_ctx == EXECUTION_CONTEXT_APPLICATION || syscall_num_is_shadow(n)) &&
               shim_thisThreadEventIPC()) {
        // The syscall is made using the shmem IPC channel.
        trace("Making syscall %ld indirectly; we ask shadow to handle it using the shmem IPC "
              "channel.",
              n);
        rv = shim_emulated_syscallv(ctx, n, args);
    } else {
        // The syscall is made directly; ptrace or seccomp will get the syscall signal.
        trace("Making syscall %ld directly; we expect ptrace or seccomp will interpose it, or it "
              "will be handled natively by the kernel.",
              n);
        rv = shim_native_syscallv(n, args);
    }

#ifdef ENABLE_PERF_LOGGING
    // 仅统计调用次数，避免对 shim 行为和时序产生额外干扰。
    uint64_t count = atomic_fetch_add_explicit(&g_shim_syscall_count, 1, memory_order_relaxed) + 1;
    if (count % SHIM_SYSCALL_LOG_EVERY == 0) {
        fprintf(stderr,
                "[shim-syscall-agg] calls=%" PRIu64 " last_n=%ld\n",
                count,
                n);
    }
#endif

    return rv;
}

long shim_syscall(ucontext_t* ctx, ExecutionContext exe_ctx, long n, ...) {
    va_list(args);
    va_start(args, n);
    long rv = shim_syscallv(ctx, exe_ctx, n, args);
    va_end(args);
    return rv;
}
