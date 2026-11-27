/* JABS Garbage Collector & Fast TLAB/Immix Allocator.
 *
 * Cooperative Stop-The-World (STW) non-moving collector:
 * - Small/medium objects (<= 8192 bytes) allocate lock-free in per-thread Immix TLABs
 *   inside 64 KB blocks carved from a contiguous 512 MB virtual arena reservation.
 * - Large objects (> 8192 bytes) allocate directly from the OS via VirtualAlloc / mmap
 *   on a doubly-linked Large Object Space (LOS) and are released immediately on sweep.
 * - Conservative C stack & register scanning (exact/interior Immix alloc_bits & LOS
 *   interval lookup) + exact heap traversal via flattened class ref_offsets.
 * - Deferred Reference processing (Soft, Weak, Phantom) feeding JDK ReferenceHandler.
 */
#include "jabs_runtime.h"
#include <setjmp.h>

#ifdef _WIN32
#ifndef WIN32_LEAN_AND_MEAN
#define WIN32_LEAN_AND_MEAN
#endif
#include <windows.h>
#else
#include <pthread.h>
#include <sys/mman.h>
#include <sched.h>
#include <unistd.h>
#endif

enum {
    JABS_GC_BLOCK_SHIFT       = 16u,
    JABS_GC_BLOCK_SIZE        = 65536u,
    JABS_GC_LINE_SHIFT        = 8u,
    JABS_GC_LINE_SIZE         = 256u,
    JABS_GC_LINES_PER_BLOCK   = 256u,
    JABS_GC_GRANULE_SHIFT     = 4u,
    JABS_GC_GRANULE_SIZE      = 16u,
    JABS_GC_GRANULES_PER_BLK  = 4096u,
    JABS_GC_MAX_SMALL_SIZE    = 8192u,
    JABS_GC_FREE_RESERVE_BLKS = 48u,
    JABS_GC_RECYCLE_MAX_LINES = 224u
};

/* The arena's size is the maximum heap, like the JVM's -Xmx: set before the first allocation
 * (jabs_gc_set_max_heap, from the launcher's -Xmx), else the JABS_MAX_HEAP environment variable,
 * else a quarter of physical memory as HotSpot picks by default. Only the address space is
 * reserved; blocks are committed as they are used. */
#define JABS_GC_MIN_HEAP ((size_t)64u << 20)
#define JABS_GC_DEFAULT_HEAP_FALLBACK ((size_t)1u << 30)
#define JABS_GC_ARENA_BYTES ((size_t)gc_arena_blocks * (size_t)JABS_GC_BLOCK_SIZE)
#define JABS_GC_ARENA_BLOCKS gc_arena_blocks
#define JABS_GC_INIT_THRESHOLD ((size_t)(16u << 20))
#define JABS_GC_MIN_HEADROOM   ((size_t)(8u << 20))
#define JABS_GC_MAX_HEADROOM   ((size_t)(32u << 20))

enum {
    JABS_GC_BLK_UNCOMMITTED = 0,
    JABS_GC_BLK_FREE        = 1,
    JABS_GC_BLK_ACTIVE      = 2
};

enum {
    JABS_GC_THR_IN_JAVA   = 0,
    JABS_GC_THR_IN_NATIVE = 1,
    JABS_GC_THR_PARKED    = 2
};

typedef struct jabs_gc_block_meta {
    uint64_t alloc_bits[64];
    uint64_t mark_bits[64];
    uint64_t line_bits[4];
    uint16_t live_lines;
    uint16_t hole_cursor;
    uint8_t state;
    uint8_t zeroed;
    uint16_t pad;
    uint32_t next_idx;
} jabs_gc_block_meta;

typedef struct jabs_gc_los_node {
    struct jabs_gc_los_node *prev;
    struct jabs_gc_los_node *next;
    size_t payload_size;
    size_t map_size;
    uint8_t marked;
    uint8_t pad[7];
    /* 8*5 + 8 = 48 bytes -> 16-byte aligned payload follows immediately. */
} jabs_gc_los_node;

typedef struct jabs_gc_los_span {
    uintptr_t start;
    uintptr_t end;
    jabs_gc_los_node *node;
} jabs_gc_los_span;

typedef struct jabs_gc_thread {
    struct jabs_gc_thread *next;
    _Atomic int32_t state;
    uint32_t native_depth;
    uintptr_t stack_top;
    uintptr_t saved_sp;
    jmp_buf saved_regs;
    jabs_gc_tlab *tlab_ptr;
    jabs_object **pending_ptr;
} jabs_gc_thread;

_Atomic uint32_t jabs_gc_safepoint_flag = 0u;
_Thread_local jabs_gc_tlab jabs_tls_tlab = { 0u, 0u, 0u, NULL };
static _Thread_local jabs_gc_thread *gc_tls_thread = NULL;

static uintptr_t gc_arena_base = 0u;
static uintptr_t gc_arena_end = 0u;
static uint32_t gc_arena_blocks = 0u;
static size_t gc_requested_max_heap = 0u;
static jabs_gc_block_meta *gc_blocks = NULL;
static uint32_t gc_committed_blocks = 0u;
static uint32_t gc_free_blocks = 0u;
static uint32_t gc_free_head = UINT32_MAX;
static uint32_t gc_recycle_head = UINT32_MAX;
static uint32_t gc_highest_block = 0u;

static jabs_gc_los_node *gc_los_head = NULL;
static size_t gc_los_count = 0u;
static size_t gc_los_bytes = 0u;
static jabs_gc_los_span *gc_los_spans = NULL;
static size_t gc_los_span_count = 0u;
static size_t gc_los_span_cap = 0u;

static _Atomic size_t gc_used_bytes = 0u;
static _Atomic size_t gc_committed_bytes = 0u;
static size_t gc_live_bytes = 0u;
static size_t gc_next_threshold = JABS_GC_INIT_THRESHOLD;

static jabs_gc_thread *gc_thread_list = NULL;
static jabs_object *gc_pending_refs_head = NULL;
static jabs_object *gc_oom_cached = NULL;
static int gc_oom_immortal = 0;

/* Scratch dynamic arrays used exclusively by collector thread during STW. */
static jabs_object **gc_mark_stack = NULL;
static size_t gc_mark_len = 0u;
static size_t gc_mark_cap = 0u;
static jabs_object **gc_discovered_refs = NULL;
static size_t gc_discovered_len = 0u;
static size_t gc_discovered_cap = 0u;
static jabs_object **gc_clear_refs = NULL;
static size_t gc_clear_len = 0u;
static size_t gc_clear_cap = 0u;

#ifdef _WIN32
static INIT_ONCE gc_init_once = INIT_ONCE_STATIC_INIT;
static CRITICAL_SECTION gc_heap_cs;
static CRITICAL_SECTION gc_tlist_cs;
static CRITICAL_SECTION gc_stw_cs;
static CONDITION_VARIABLE gc_stw_cv;
static CRITICAL_SECTION gc_ref_cs;
static CONDITION_VARIABLE gc_ref_cv;
#else
static pthread_once_t gc_init_once = PTHREAD_ONCE_INIT;
static pthread_mutex_t gc_heap_mu = PTHREAD_MUTEX_INITIALIZER;
static pthread_mutex_t gc_tlist_mu = PTHREAD_MUTEX_INITIALIZER;
static pthread_mutex_t gc_stw_mu = PTHREAD_MUTEX_INITIALIZER;
static pthread_cond_t gc_stw_cv = PTHREAD_COND_INITIALIZER;
static pthread_mutex_t gc_ref_mu = PTHREAD_MUTEX_INITIALIZER;
static pthread_cond_t gc_ref_cv = PTHREAD_COND_INITIALIZER;
#endif

static void gc_heap_lock(void) {
#ifdef _WIN32
    EnterCriticalSection(&gc_heap_cs);
#else
    pthread_mutex_lock(&gc_heap_mu);
#endif
}

static int gc_heap_trylock(void) {
#ifdef _WIN32
    return TryEnterCriticalSection(&gc_heap_cs) != 0;
#else
    return pthread_mutex_trylock(&gc_heap_mu) == 0;
#endif
}

static void gc_heap_unlock(void) {
#ifdef _WIN32
    LeaveCriticalSection(&gc_heap_cs);
#else
    pthread_mutex_unlock(&gc_heap_mu);
#endif
}

static void gc_tlist_lock(void) {
#ifdef _WIN32
    EnterCriticalSection(&gc_tlist_cs);
#else
    pthread_mutex_lock(&gc_tlist_mu);
#endif
}

static void gc_tlist_unlock(void) {
#ifdef _WIN32
    LeaveCriticalSection(&gc_tlist_cs);
#else
    pthread_mutex_unlock(&gc_tlist_mu);
#endif
}

static void gc_stw_lock(void) {
#ifdef _WIN32
    EnterCriticalSection(&gc_stw_cs);
#else
    pthread_mutex_lock(&gc_stw_mu);
#endif
}

static void gc_stw_unlock(void) {
#ifdef _WIN32
    LeaveCriticalSection(&gc_stw_cs);
#else
    pthread_mutex_unlock(&gc_stw_mu);
#endif
}

static void gc_stw_wait(void) {
#ifdef _WIN32
    SleepConditionVariableCS(&gc_stw_cv, &gc_stw_cs, INFINITE);
#else
    pthread_cond_wait(&gc_stw_cv, &gc_stw_mu);
#endif
}

static void gc_stw_broadcast(void) {
#ifdef _WIN32
    WakeAllConditionVariable(&gc_stw_cv);
#else
    pthread_cond_broadcast(&gc_stw_cv);
#endif
}

/* "512m", "4G", "65536k" or plain bytes, as -Xmx takes them; 0 if malformed or too large. */
static size_t gc_parse_size(const char *text) {
    if (!text || !*text) return 0u;
    size_t value = 0u;
    const char *p = text;
    for (; *p >= '0' && *p <= '9'; p++) {
        if (value > (SIZE_MAX - 9u) / 10u) return 0u;
        value = value * 10u + (size_t)(*p - '0');
    }
    if (p == text) return 0u;
    unsigned shift = 0u;
    if (*p == 'k' || *p == 'K') shift = 10u;
    else if (*p == 'm' || *p == 'M') shift = 20u;
    else if (*p == 'g' || *p == 'G') shift = 30u;
    else if (*p == 't' || *p == 'T') shift = 40u;
    if (shift) p++;
    if (*p != '\0') return 0u;
    if (shift && value > (SIZE_MAX >> shift)) return 0u;
    return value << shift;
}

int jabs_gc_set_max_heap(const char *size) {
    size_t bytes = gc_parse_size(size);
    if (bytes == 0u) return -1;
    gc_requested_max_heap = bytes;
    return 0;
}

static size_t gc_physical_memory(void) {
#ifdef _WIN32
    MEMORYSTATUSEX status;
    status.dwLength = sizeof status;
    return GlobalMemoryStatusEx(&status) ? (size_t)status.ullTotalPhys : 0u;
#else
    long pages = sysconf(_SC_PHYS_PAGES);
    long page = sysconf(_SC_PAGE_SIZE);
    return pages > 0 && page > 0 ? (size_t)pages * (size_t)page : 0u;
#endif
}

static size_t gc_choose_max_heap(void) {
    size_t bytes = gc_requested_max_heap;
    if (bytes == 0u) {
        const char *env = getenv("JABS_MAX_HEAP");
        if (env && *env) {
            bytes = gc_parse_size(env);
            if (bytes == 0u) fprintf(stderr, "jabs: ignoring malformed JABS_MAX_HEAP=%s\n", env);
        }
    }
    if (bytes == 0u) {
        size_t phys = gc_physical_memory();
        bytes = phys ? phys / 4u : JABS_GC_DEFAULT_HEAP_FALLBACK;
    }
    if (bytes < JABS_GC_MIN_HEAP) bytes = JABS_GC_MIN_HEAP;
    size_t blocks = (bytes + JABS_GC_BLOCK_SIZE - 1u) >> JABS_GC_BLOCK_SHIFT;
    if (blocks > (size_t)(UINT32_MAX - 1u)) blocks = (size_t)(UINT32_MAX - 1u);
    return blocks;
}

static void gc_init_impl(void) {
#ifdef _WIN32
    InitializeCriticalSection(&gc_heap_cs);
    InitializeCriticalSection(&gc_tlist_cs);
    InitializeCriticalSection(&gc_stw_cs);
    InitializeConditionVariable(&gc_stw_cv);
    InitializeCriticalSection(&gc_ref_cs);
    InitializeConditionVariable(&gc_ref_cv);
#endif
    gc_arena_blocks = (uint32_t)gc_choose_max_heap();
    /* Zeroed and committed lazily by the OS: only the metadata of used blocks costs memory. */
    gc_blocks = (jabs_gc_block_meta *)calloc(gc_arena_blocks, sizeof *gc_blocks);
    if (!gc_blocks) {
        fprintf(stderr, "jabs: failed to allocate GC metadata for a %zu MB heap\n", JABS_GC_ARENA_BYTES >> 20);
        abort();
    }
#ifdef _WIN32
    void *base = VirtualAlloc(NULL, JABS_GC_ARENA_BYTES, MEM_RESERVE, PAGE_NOACCESS);
#else
    /* Blocks are found by masking addresses (addr & (BLOCK_SIZE - 1)), so the arena must start on
       a block boundary. VirtualAlloc reserves on 64 KB boundaries; mmap only on pages, so reserve
       one block more, round the base up, and give back the ends. */
    void *base = NULL;
    size_t span = JABS_GC_ARENA_BYTES + JABS_GC_BLOCK_SIZE;
    void *raw = mmap(NULL, span, PROT_NONE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (raw != MAP_FAILED) {
        uintptr_t start = (uintptr_t)raw;
        uintptr_t aligned = (start + JABS_GC_BLOCK_SIZE - 1u) & ~(uintptr_t)(JABS_GC_BLOCK_SIZE - 1u);
        size_t head = (size_t)(aligned - start);
        size_t tail = span - head - JABS_GC_ARENA_BYTES;
        if (head) munmap(raw, head);
        if (tail) munmap((void *)(aligned + JABS_GC_ARENA_BYTES), tail);
        base = (void *)aligned;
    }
#endif
    if (!base) {
        fprintf(stderr, "jabs: failed to reserve %zu MB GC arena\n", JABS_GC_ARENA_BYTES >> 20);
        abort();
    }
    gc_arena_base = (uintptr_t)base;
    gc_arena_end = gc_arena_base + JABS_GC_ARENA_BYTES;
}

#ifdef _WIN32
static BOOL CALLBACK gc_init_win32(PINIT_ONCE once, PVOID param, PVOID *ctx) {
    (void)once; (void)param; (void)ctx;
    gc_init_impl();
    return TRUE;
}
#endif

static void gc_init_once_run(void) {
#ifdef _WIN32
    InitOnceExecuteOnce(&gc_init_once, gc_init_win32, NULL, NULL);
#else
    pthread_once(&gc_init_once, gc_init_impl);
#endif
}

static uintptr_t gc_current_stack_top(void) {
#ifdef _WIN32
    NT_TIB *tib = (NT_TIB *)NtCurrentTeb();
    return (uintptr_t)tib->StackBase;
#else
    pthread_attr_t attr;
    void *addr = NULL;
    size_t sz = 0u;
    if (pthread_getattr_np(pthread_self(), &attr) == 0) {
        if (pthread_attr_getstack(&attr, &addr, &sz) == 0) {
            pthread_attr_destroy(&attr);
            return (uintptr_t)addr + sz;
        }
        pthread_attr_destroy(&attr);
    }
    int dummy = 0;
    return ((uintptr_t)&dummy + 65536u) & ~(uintptr_t)4095u;
#endif
}

static jabs_gc_thread *gc_ensure_thread(void) {
    jabs_gc_thread *t = gc_tls_thread;
    if (__builtin_expect(t != NULL, 1)) return t;
    gc_init_once_run();
    t = (jabs_gc_thread *)calloc(1, sizeof *t);
    if (!t) abort();
    atomic_init(&t->state, JABS_GC_THR_IN_JAVA);
    t->native_depth = 0u;
    t->stack_top = gc_current_stack_top();
    t->tlab_ptr = &jabs_tls_tlab;
    t->pending_ptr = &jabs_pending;
    gc_tlist_lock();
    t->next = gc_thread_list;
    gc_thread_list = t;
    gc_tlist_unlock();
    gc_tls_thread = t;
    return t;
}

__attribute__((noinline)) static void gc_capture_context(jabs_gc_thread *t) {
    volatile uintptr_t anchor = (uintptr_t)&t;
    t->saved_sp = anchor;
    (void)setjmp(t->saved_regs);
}

static void gc_safepoint_park(jabs_gc_thread *t) {
    gc_capture_context(t);
    gc_stw_lock();
    atomic_store_explicit(&t->state, JABS_GC_THR_PARKED, memory_order_seq_cst);
    gc_stw_broadcast();
    while (atomic_load_explicit(&jabs_gc_safepoint_flag, memory_order_acquire) != 0u) {
        gc_stw_wait();
    }
    atomic_store_explicit(&t->state, JABS_GC_THR_IN_JAVA, memory_order_seq_cst);
    gc_stw_unlock();
}

void jabs_gc_safepoint_slow(void) {
    jabs_gc_thread *t = gc_ensure_thread();
    if (t->native_depth > 0u) return;
    if (atomic_load_explicit(&jabs_gc_safepoint_flag, memory_order_acquire) != 0u) {
        gc_safepoint_park(t);
    }
}

void jabs_gc_enter_native(void) {
    jabs_gc_thread *t = gc_ensure_thread();
    if (t->native_depth++ > 0u) return;
    gc_capture_context(t);
    atomic_store_explicit(&t->state, JABS_GC_THR_IN_NATIVE, memory_order_seq_cst);
    if (__builtin_expect(atomic_load_explicit(&jabs_gc_safepoint_flag, memory_order_seq_cst) != 0u, 0)) {
        gc_stw_lock();
        gc_stw_broadcast();
        gc_stw_unlock();
    }
}

void jabs_gc_exit_native(void) {
    jabs_gc_thread *t = gc_ensure_thread();
    if (t->native_depth == 0u || --t->native_depth > 0u) return;
    atomic_store_explicit(&t->state, JABS_GC_THR_IN_JAVA, memory_order_seq_cst);
    if (__builtin_expect(atomic_load_explicit(&jabs_gc_safepoint_flag, memory_order_seq_cst) != 0u, 0)) {
        gc_safepoint_park(t);
    }
}

void jabs_gc_thread_attach(void) {
    (void)gc_ensure_thread();
}

void jabs_gc_thread_detach(void) {
    jabs_gc_thread *t = gc_tls_thread;
    if (!t) return;
    jabs_tls_tlab.cursor = 0u;
    jabs_tls_tlab.limit = 0u;
    jabs_tls_tlab.block_base = 0u;
    jabs_tls_tlab.alloc_bits = NULL;
    gc_capture_context(t);
    atomic_store_explicit(&t->state, JABS_GC_THR_IN_NATIVE, memory_order_seq_cst);
    if (atomic_load_explicit(&jabs_gc_safepoint_flag, memory_order_seq_cst) != 0u) {
        gc_stw_lock();
        gc_stw_broadcast();
        gc_stw_unlock();
    }
    gc_tlist_lock();
    jabs_gc_thread **pp = &gc_thread_list;
    while (*pp && *pp != t) pp = &(*pp)->next;
    if (*pp == t) *pp = t->next;
    gc_tlist_unlock();
    gc_tls_thread = NULL;
    free(t);
}

static void gc_acquire_heap_safely(void) {
    gc_ensure_thread();
    for (;;) {
        if (atomic_load_explicit(&jabs_gc_safepoint_flag, memory_order_acquire) != 0u) {
            jabs_gc_safepoint_slow();
        }
        if (gc_heap_trylock()) return;
        jabs_gc_enter_native();
        gc_heap_lock();
        gc_heap_unlock();
        jabs_gc_exit_native();
    }
}

/* ---- Object Size, Marking, Root Visiting & Sweeping (STW only) ---- */

static size_t gc_object_size(const jabs_object *o) {
    const jabs_class *cls = o->cls;
    if (cls == jabs_array_class()) {
        const jabs_array *a = (const jabs_array *)o;
        size_t hdr = (sizeof(jabs_array) + 7u) & ~(size_t)7u;
        return hdr + (size_t)a->length * jabs_element_size(a);
    }
    if ((cls->gc_flags & JABS_CLS_VAR_HANDLE) != 0u && jabs_gc_var_handle_size > cls->size) {
        return jabs_gc_var_handle_size;
    }
    return cls->size;
}

static int gc_span_cmp(const void *a, const void *b) {
    const jabs_gc_los_span *sa = (const jabs_gc_los_span *)a;
    const jabs_gc_los_span *sb = (const jabs_gc_los_span *)b;
    if (sa->start < sb->start) return -1;
    if (sa->start > sb->start) return 1;
    return 0;
}

static jabs_gc_los_node *gc_find_los_node(uintptr_t addr) {
    size_t lo = 0u, hi = gc_los_span_count;
    while (lo < hi) {
        size_t mid = lo + (hi - lo) / 2u;
        const jabs_gc_los_span *sp = &gc_los_spans[mid];
        if (addr < sp->start) hi = mid;
        else if (addr >= sp->end) lo = mid + 1u;
        else return sp->node;
    }
    return NULL;
}

static jabs_object *gc_find_conservative_object(uintptr_t addr) {
    if (addr >= gc_arena_base && addr < gc_arena_end) {
        size_t bidx = (addr - gc_arena_base) >> JABS_GC_BLOCK_SHIFT;
        if (bidx >= JABS_GC_ARENA_BLOCKS) return NULL;
        const jabs_gc_block_meta *meta = &gc_blocks[bidx];
        if (meta->state != JABS_GC_BLK_ACTIVE) return NULL;
        size_t g = (addr & (JABS_GC_BLOCK_SIZE - 1u)) >> JABS_GC_GRANULE_SHIFT;
        size_t w = g >> 6;
        uint64_t mask = (~UINT64_C(0)) >> (63u - (g & 63u));
        uint64_t bits = meta->alloc_bits[w] & mask;
        while (bits == 0u) {
            if (w == 0u) return NULL;
            w--;
            bits = meta->alloc_bits[w];
        }
        unsigned bit = 63u - (unsigned)__builtin_clzll(bits);
        size_t sg = (w << 6) | (size_t)bit;
        uintptr_t obj_addr = gc_arena_base + (bidx << JABS_GC_BLOCK_SHIFT) + (sg << JABS_GC_GRANULE_SHIFT);
        jabs_object *cand = (jabs_object *)obj_addr;
        if (!cand->cls) return NULL;
        size_t sz = gc_object_size(cand);
        if (addr >= obj_addr && addr < obj_addr + sz) return cand;
        return NULL;
    }
    jabs_gc_los_node *node = gc_find_los_node(addr);
    if (!node) return NULL;
    jabs_object *obj = (jabs_object *)(node + 1);
    return obj->cls ? obj : NULL;
}

static int gc_is_marked(const jabs_object *o) {
    if (!o) return 0;
    uintptr_t addr = (uintptr_t)o;
    if (addr >= gc_arena_base && addr < gc_arena_end) {
        size_t bidx = (addr - gc_arena_base) >> JABS_GC_BLOCK_SHIFT;
        if (gc_blocks[bidx].state != JABS_GC_BLK_ACTIVE) return 0;
        size_t g = (addr & (JABS_GC_BLOCK_SIZE - 1u)) >> JABS_GC_GRANULE_SHIFT;
        return (int)((gc_blocks[bidx].mark_bits[g >> 6] >> (g & 63u)) & 1u);
    }
    jabs_gc_los_node *node = gc_find_los_node(addr);
    if (node) return node->marked != 0u;
    /* Objects outside arena/LOS (e.g. immortal emergency OOM fallback) stay alive. */
    return o == gc_oom_cached && gc_oom_immortal;
}

static void gc_push_mark(jabs_object *o) {
    if (!o || (gc_oom_immortal && o == gc_oom_cached)) return;
    uintptr_t addr = (uintptr_t)o;
    if (addr >= gc_arena_base && addr < gc_arena_end) {
        size_t bidx = (addr - gc_arena_base) >> JABS_GC_BLOCK_SHIFT;
        jabs_gc_block_meta *meta = &gc_blocks[bidx];
        if (meta->state != JABS_GC_BLK_ACTIVE) return;
        size_t g = (addr & (JABS_GC_BLOCK_SIZE - 1u)) >> JABS_GC_GRANULE_SHIFT;
        uint64_t bit = UINT64_C(1) << (g & 63u);
        if (meta->mark_bits[g >> 6] & bit) return;
        meta->mark_bits[g >> 6] |= bit;

        size_t sz = gc_object_size(o);
        size_t asize = (sz + (JABS_GC_GRANULE_SIZE - 1u)) & ~(size_t)(JABS_GC_GRANULE_SIZE - 1u);
        gc_live_bytes += asize;

        size_t off0 = (size_t)(addr & (JABS_GC_BLOCK_SIZE - 1u));
        size_t off1 = off0 + (sz ? sz - 1u : 0u);
        size_t l0 = off0 >> JABS_GC_LINE_SHIFT;
        size_t l1 = off1 >> JABS_GC_LINE_SHIFT;
        if (l1 >= JABS_GC_LINES_PER_BLOCK) l1 = JABS_GC_LINES_PER_BLOCK - 1u;
        for (size_t l = l0; l <= l1; l++) {
            meta->line_bits[l >> 6] |= (UINT64_C(1) << (l & 63u));
        }
    } else {
        jabs_gc_los_node *node = gc_find_los_node(addr);
        if (!node || node->marked) return;
        node->marked = 1u;
        o = (jabs_object *)(node + 1);
    }

    if (gc_mark_len == gc_mark_cap) {
        size_t ncap = gc_mark_cap ? gc_mark_cap * 2u : 4096u;
        jabs_object **n = (jabs_object **)realloc(gc_mark_stack, ncap * sizeof *n);
        if (!n) abort();
        gc_mark_stack = n;
        gc_mark_cap = ncap;
    }
    gc_mark_stack[gc_mark_len++] = o;
}

static void gc_root_visit(jabs_object *o, void *ctx) {
    (void)ctx;
    gc_push_mark(o);
}

static void gc_push_discovered(jabs_object *ref) {
    if (gc_discovered_len == gc_discovered_cap) {
        size_t ncap = gc_discovered_cap ? gc_discovered_cap * 2u : 256u;
        jabs_object **n = (jabs_object **)realloc(gc_discovered_refs, ncap * sizeof *n);
        if (!n) abort();
        gc_discovered_refs = n;
        gc_discovered_cap = ncap;
    }
    gc_discovered_refs[gc_discovered_len++] = ref;
}

static void gc_drain_marks(void) {
    while (gc_mark_len > 0u) {
        jabs_object *o = gc_mark_stack[--gc_mark_len];
        const jabs_class *cls = o->cls;
        if (!cls) continue;
        if (cls == jabs_array_class()) {
            const jabs_array *a = (const jabs_array *)o;
            if (!jabs_array_is_ref(a)) continue;
            jabs_object *const *elems = (jabs_object *const *)((const char *)a + ((sizeof(jabs_array) + 7u) & ~(size_t)7u));
            int32_t len = a->length;
            for (int32_t i = 0; i < len; i++) {
                if (elems[i]) gc_push_mark(elems[i]);
            }
            continue;
        }
        if (cls->gc_flags & JABS_CLS_HAS_REFS) {
            const uint32_t *offs = cls->ref_offsets;
            uint32_t n = cls->ref_offsets_len;
            for (uint32_t i = 0u; i < n; i++) {
                jabs_object *child = *(jabs_object *const *)((const char *)o + offs[i]);
                if (child) gc_push_mark(child);
            }
        }
        if ((cls->gc_flags & JABS_CLS_REF_MASK) != 0u && jabs_gc_ref_referent_offset != 0u) {
            jabs_object *referent = *(jabs_object *const *)((const char *)o + jabs_gc_ref_referent_offset);
            if (referent) gc_push_discovered(o);
        }
    }
}

static void gc_scan_memory_range(uintptr_t low, uintptr_t high) {
    if (low >= high) return;
    uintptr_t p = (low + sizeof(uintptr_t) - 1u) & ~(uintptr_t)(sizeof(uintptr_t) - 1u);
    for (; p + sizeof(uintptr_t) <= high; p += sizeof(uintptr_t)) {
        uintptr_t word;
        memcpy(&word, (const void *)p, sizeof word);
        if (word < 0x10000u) continue;
        jabs_object *obj = gc_find_conservative_object(word);
        if (obj) gc_push_mark(obj);
    }
}

static void gc_process_references(int clear_soft) {
    if (!jabs_gc_ref_referent_offset || gc_discovered_len == 0u) return;
    /* Preserve SoftReference referents unless explicit System.gc() / memory pressure. */
    if (!clear_soft) {
        for (size_t i = 0u; i < gc_discovered_len; i++) {
            jabs_object *r = gc_discovered_refs[i];
            if (r->cls->gc_flags & JABS_CLS_REF_SOFT) {
                jabs_object *referent = *(jabs_object *const *)((const char *)r + jabs_gc_ref_referent_offset);
                if (referent) gc_push_mark(referent);
            }
        }
        gc_drain_marks();
    }

    gc_clear_len = 0u;
    for (size_t i = 0u; i < gc_discovered_len; i++) {
        jabs_object *r = gc_discovered_refs[i];
        uint16_t rf = r->cls->gc_flags & JABS_CLS_REF_MASK;
        if (!clear_soft && (rf & JABS_CLS_REF_SOFT)) continue;
        jabs_object *referent = *(jabs_object *const *)((const char *)r + jabs_gc_ref_referent_offset);
        if (!referent || gc_is_marked(referent)) continue;
        if (gc_clear_len == gc_clear_cap) {
            size_t ncap = gc_clear_cap ? gc_clear_cap * 2u : 128u;
            jabs_object **n = (jabs_object **)realloc(gc_clear_refs, ncap * sizeof *n);
            if (!n) abort();
            gc_clear_refs = n;
            gc_clear_cap = ncap;
        }
        gc_clear_refs[gc_clear_len++] = r;
    }

    for (size_t i = 0u; i < gc_clear_len; i++) {
        jabs_object *r = gc_clear_refs[i];
        *(jabs_object **)((char *)r + jabs_gc_ref_referent_offset) = NULL;
        if (jabs_gc_ref_discovered_offset != 0u) {
            *(jabs_object **)((char *)r + jabs_gc_ref_discovered_offset) = gc_pending_refs_head;
            gc_pending_refs_head = r;
        }
    }
}

static void gc_rebuild_los_spans(void) {
    if (gc_los_count > gc_los_span_cap) {
        size_t ncap = gc_los_count + 16u;
        jabs_gc_los_span *n = (jabs_gc_los_span *)realloc(gc_los_spans, ncap * sizeof *n);
        if (!n) abort();
        gc_los_spans = n;
        gc_los_span_cap = ncap;
    }
    size_t idx = 0u;
    for (jabs_gc_los_node *cur = gc_los_head; cur; cur = cur->next) {
        cur->marked = 0u;
        uintptr_t start = (uintptr_t)(cur + 1);
        gc_los_spans[idx].start = start;
        gc_los_spans[idx].end = start + cur->payload_size;
        gc_los_spans[idx].node = cur;
        idx++;
    }
    gc_los_span_count = idx;
    if (idx > 1u) {
        qsort(gc_los_spans, idx, sizeof *gc_los_spans, gc_span_cmp);
    }
}

static void gc_free_os_pages(void *ptr, size_t size) {
#ifdef _WIN32
    (void)size;
    VirtualFree(ptr, 0, MEM_RELEASE);
#else
    munmap(ptr, size);
#endif
}

static void gc_decommit_block(uint32_t bidx) {
    uintptr_t addr = gc_arena_base + ((uintptr_t)bidx << JABS_GC_BLOCK_SHIFT);
#ifdef _WIN32
    VirtualFree((void *)addr, JABS_GC_BLOCK_SIZE, MEM_DECOMMIT);
#else
    madvise((void *)addr, JABS_GC_BLOCK_SIZE, MADV_DONTNEED);
    mprotect((void *)addr, JABS_GC_BLOCK_SIZE, PROT_NONE);
#endif
    memset(&gc_blocks[bidx], 0, sizeof gc_blocks[bidx]);
    gc_committed_blocks--;
}

/* Whether `bytes` more fit under the maximum heap: committed small-object blocks and large objects
   share it, as -Xmx bounds the JVM's whole heap. */
static int gc_heap_has_room(size_t bytes) {
    size_t committed = ((size_t)gc_committed_blocks << JABS_GC_BLOCK_SHIFT) + gc_los_bytes;
    return committed <= JABS_GC_ARENA_BYTES && bytes <= JABS_GC_ARENA_BYTES - committed;
}

static int gc_commit_block(uint32_t bidx) {
    if (!gc_heap_has_room(JABS_GC_BLOCK_SIZE)) return 0;
    uintptr_t addr = gc_arena_base + ((uintptr_t)bidx << JABS_GC_BLOCK_SHIFT);
#ifdef _WIN32
    if (!VirtualAlloc((void *)addr, JABS_GC_BLOCK_SIZE, MEM_COMMIT, PAGE_READWRITE)) return 0;
#else
    if (mprotect((void *)addr, JABS_GC_BLOCK_SIZE, PROT_READ | PROT_WRITE) != 0) return 0;
#endif
    memset(&gc_blocks[bidx], 0, sizeof gc_blocks[bidx]);
    gc_blocks[bidx].state = JABS_GC_BLK_FREE;
    gc_blocks[bidx].zeroed = 1u;
    gc_committed_blocks++;
    return 1;
}

static void gc_sweep_all(void) {
    gc_free_head = UINT32_MAX;
    gc_free_blocks = 0u;
    gc_recycle_head = UINT32_MAX;
    uint32_t new_highest = 0u;

    for (uint32_t b = 0u; b <= gc_highest_block && b < JABS_GC_ARENA_BLOCKS; b++) {
        jabs_gc_block_meta *meta = &gc_blocks[b];
        if (meta->state == JABS_GC_BLK_UNCOMMITTED) continue;
        if (meta->state == JABS_GC_BLK_FREE) {
            if (gc_free_blocks >= JABS_GC_FREE_RESERVE_BLKS) {
                gc_decommit_block(b);
            } else {
                meta->next_idx = gc_free_head;
                gc_free_head = b;
                gc_free_blocks++;
                new_highest = b;
            }
            continue;
        }
        /* BLK_ACTIVE */
        uint32_t live_lines = 0u;
        for (int i = 0; i < 4; i++) live_lines += (uint32_t)__builtin_popcountll(meta->line_bits[i]);
        if (live_lines == 0u) {
            memset(meta->alloc_bits, 0, sizeof meta->alloc_bits);
            memset(meta->mark_bits, 0, sizeof meta->mark_bits);
            meta->live_lines = 0u;
            meta->hole_cursor = 0u;
            meta->zeroed = 0u;
            if (gc_free_blocks >= JABS_GC_FREE_RESERVE_BLKS) {
                gc_decommit_block(b);
            } else {
                meta->state = JABS_GC_BLK_FREE;
                meta->next_idx = gc_free_head;
                gc_free_head = b;
                gc_free_blocks++;
                new_highest = b;
            }
            continue;
        }
        for (int i = 0; i < 64; i++) meta->alloc_bits[i] = meta->mark_bits[i];
        meta->live_lines = (uint16_t)live_lines;
        meta->hole_cursor = 0u;
        meta->zeroed = 0u;
        meta->next_idx = UINT32_MAX;
        new_highest = b;
        if (live_lines <= JABS_GC_RECYCLE_MAX_LINES) {
            meta->next_idx = gc_recycle_head;
            gc_recycle_head = b;
        }
    }
    gc_highest_block = new_highest;

    jabs_gc_los_node *cur = gc_los_head;
    while (cur) {
        jabs_gc_los_node *nxt = cur->next;
        if (!cur->marked) {
            if (cur->prev) cur->prev->next = cur->next;
            else gc_los_head = cur->next;
            if (cur->next) cur->next->prev = cur->prev;
            gc_los_bytes -= cur->payload_size;
            gc_los_count--;
            gc_free_os_pages(cur, cur->map_size);
        }
        cur = nxt;
    }

    size_t total_live = gc_live_bytes + gc_los_bytes;
    size_t committed = ((size_t)gc_committed_blocks << JABS_GC_BLOCK_SHIFT) + gc_los_bytes;
    atomic_store_explicit(&gc_used_bytes, total_live, memory_order_relaxed);
    atomic_store_explicit(&gc_committed_bytes, committed, memory_order_relaxed);
    atomic_store_explicit(&jabs_heap_bytes, (int64_t)total_live, memory_order_relaxed);
}

static void gc_stw_collect(jabs_gc_thread *collector, uintptr_t collector_sp, const jmp_buf *collector_regs, int clear_soft) {
    gc_tlist_lock();
    atomic_store_explicit(&jabs_gc_safepoint_flag, 1u, memory_order_seq_cst);

    /* Wait until every other attached thread is IN_NATIVE or PARKED. */
    gc_stw_lock();
    for (;;) {
        int waiting = 0;
        for (jabs_gc_thread *t = gc_thread_list; t; t = t->next) {
            if (t == collector) continue;
            int32_t st = atomic_load_explicit(&t->state, memory_order_seq_cst);
            if (st == JABS_GC_THR_IN_JAVA) {
                waiting = 1;
                break;
            }
        }
        if (!waiting) break;
        gc_stw_wait();
    }
    gc_stw_unlock();

    /* Retire every thread's TLAB before preparing mark/sweep bitmaps. */
    for (jabs_gc_thread *t = gc_thread_list; t; t = t->next) {
        if (t->tlab_ptr) {
            t->tlab_ptr->cursor = 0u;
            t->tlab_ptr->limit = 0u;
            t->tlab_ptr->block_base = 0u;
            t->tlab_ptr->alloc_bits = NULL;
        }
    }

    for (uint32_t b = 0u; b <= gc_highest_block && b < JABS_GC_ARENA_BLOCKS; b++) {
        if (gc_blocks[b].state == JABS_GC_BLK_ACTIVE) {
            memset(gc_blocks[b].mark_bits, 0, sizeof gc_blocks[b].mark_bits);
            memset(gc_blocks[b].line_bits, 0, sizeof gc_blocks[b].line_bits);
        }
    }
    gc_rebuild_los_spans();
    gc_live_bytes = 0u;
    gc_mark_len = 0u;
    gc_discovered_len = 0u;

    /* 1. Exact roots: static reference slots, literals/reflect/modules/rproxy, Thread objs, pending refs, OOM. */
    for (size_t i = 0u; jabs_gc_static_refs[i]; i++) {
        jabs_object *obj = *jabs_gc_static_refs[i];
        if (obj) gc_push_mark(obj);
    }
    jabs_gc_visit_runtime_roots(gc_root_visit, NULL);
    jabs_thread_visit_gc_roots(gc_root_visit, NULL);
    if (gc_oom_cached) gc_push_mark(gc_oom_cached);
    for (jabs_object *p = gc_pending_refs_head; p;) {
        gc_push_mark(p);
        p = jabs_gc_ref_discovered_offset
            ? *(jabs_object *const *)((const char *)p + jabs_gc_ref_discovered_offset)
            : NULL;
    }

    /* 2. Conservative thread stacks, saved register buffers, and per-thread jabs_pending. */
    gc_scan_memory_range(collector_sp, collector->stack_top);
    gc_scan_memory_range((uintptr_t)collector_regs, (uintptr_t)collector_regs + sizeof *collector_regs);
    if (collector->pending_ptr && *collector->pending_ptr) gc_push_mark(*collector->pending_ptr);
    for (jabs_gc_thread *t = gc_thread_list; t; t = t->next) {
        if (t == collector) continue;
        gc_scan_memory_range(t->saved_sp, t->stack_top);
        gc_scan_memory_range((uintptr_t)&t->saved_regs, (uintptr_t)&t->saved_regs + sizeof t->saved_regs);
        if (t->pending_ptr && *t->pending_ptr) gc_push_mark(*t->pending_ptr);
    }

    /* 3. Transitive closure + Soft/Weak/Phantom reference handling. */
    gc_drain_marks();
    int had_pending = gc_pending_refs_head != NULL;
    size_t prev_used = gc_live_bytes + gc_los_bytes;
    int force_soft = clear_soft || prev_used > (gc_next_threshold * 3u) / 4u;
    gc_process_references(force_soft);
    int newly_pending = (!had_pending && gc_pending_refs_head != NULL) || gc_clear_len > 0u;

    /* 4. Weak tables (String intern cache, MethodType table) & inflated object monitors. */
    jabs_gc_sweep_weak_roots(gc_is_marked);
    jabs_gc_sweep_sync_monitors(gc_is_marked);

    /* 5. Reclaim dead Immix lines/blocks & dead LOS objects. */
    gc_sweep_all();

    /* Adapt next collection trigger. */
    size_t live = gc_live_bytes + gc_los_bytes;
    size_t headroom = live / 2u;
    if (headroom < JABS_GC_MIN_HEADROOM) headroom = JABS_GC_MIN_HEADROOM;
    if (headroom > JABS_GC_MAX_HEADROOM) headroom = JABS_GC_MAX_HEADROOM;
    size_t next_thresh = live + headroom;
    if (next_thresh < JABS_GC_INIT_THRESHOLD) next_thresh = JABS_GC_INIT_THRESHOLD;
    gc_next_threshold = next_thresh;

    /* Resume world. */
    gc_stw_lock();
    atomic_store_explicit(&jabs_gc_safepoint_flag, 0u, memory_order_release);
    gc_stw_broadcast();
    gc_stw_unlock();
    gc_tlist_unlock();

    if (newly_pending) {
#ifdef _WIN32
        EnterCriticalSection(&gc_ref_cs);
        WakeAllConditionVariable(&gc_ref_cv);
        LeaveCriticalSection(&gc_ref_cs);
#else
        pthread_mutex_lock(&gc_ref_mu);
        pthread_cond_broadcast(&gc_ref_cv);
        pthread_mutex_unlock(&gc_ref_mu);
#endif
    }
}

__attribute__((noinline)) static void gc_collect_locked(jabs_gc_thread *self, int clear_soft) {
    jmp_buf regs;
    volatile uintptr_t sp = (uintptr_t)&regs;
    (void)setjmp(regs);
    gc_stw_collect(self, sp, &regs, clear_soft);
}

/* ---- Allocation Slow Paths (called with gc_heap_lock held) ---- */

static int gc_find_free_lines(jabs_gc_block_meta *meta, uint32_t need_lines, uint32_t *out_start, uint32_t *out_end) {
    uint32_t cur = meta->hole_cursor;
    while (cur < JABS_GC_LINES_PER_BLOCK) {
        /* Skip marked lines. */
        while (cur < JABS_GC_LINES_PER_BLOCK && ((meta->line_bits[cur >> 6] >> (cur & 63u)) & 1u)) {
            cur++;
        }
        uint32_t start = cur;
        while (cur < JABS_GC_LINES_PER_BLOCK && !((meta->line_bits[cur >> 6] >> (cur & 63u)) & 1u)) {
            cur++;
        }
        if (cur - start >= need_lines) {
            meta->hole_cursor = (uint16_t)cur;
            *out_start = start;
            *out_end = cur;
            return 1;
        }
    }
    meta->hole_cursor = JABS_GC_LINES_PER_BLOCK;
    return 0;
}

static jabs_object *gc_alloc_small_in_block(size_t asize, int fill_tlab) {
    uint32_t need_lines = (uint32_t)((asize + JABS_GC_LINE_SIZE - 1u) >> JABS_GC_LINE_SHIFT);
    if (need_lines == 0u) need_lines = 1u;

    /* 1. Try recyclable partial blocks first for dense Immix packing. */
    while (gc_recycle_head != UINT32_MAX) {
        uint32_t b = gc_recycle_head;
        jabs_gc_block_meta *meta = &gc_blocks[b];
        uint32_t sline = 0u, eline = 0u;
        if (gc_find_free_lines(meta, need_lines, &sline, &eline)) {
            uintptr_t bbase = gc_arena_base + ((uintptr_t)b << JABS_GC_BLOCK_SHIFT);
            uintptr_t start_addr = bbase + ((uintptr_t)sline << JABS_GC_LINE_SHIFT);
            uintptr_t end_addr = bbase + ((uintptr_t)eline << JABS_GC_LINE_SHIFT);
            if (!meta->zeroed) {
                memset((void *)start_addr, 0, (size_t)(end_addr - start_addr));
            }
            for (uint32_t l = sline; l < eline; l++) {
                meta->line_bits[l >> 6] |= (UINT64_C(1) << (l & 63u));
            }
            size_t g = ((start_addr - bbase) >> JABS_GC_GRANULE_SHIFT);
            meta->alloc_bits[g >> 6] |= (UINT64_C(1) << (g & 63u));
            if (fill_tlab) {
                jabs_tls_tlab.cursor = start_addr + asize;
                jabs_tls_tlab.limit = end_addr;
                jabs_tls_tlab.block_base = bbase;
                jabs_tls_tlab.alloc_bits = meta->alloc_bits;
            }
            size_t granted = (size_t)(end_addr - start_addr);
            size_t used = atomic_fetch_add_explicit(&gc_used_bytes, fill_tlab ? granted : asize, memory_order_relaxed) + (fill_tlab ? granted : asize);
            atomic_store_explicit(&jabs_heap_bytes, (int64_t)used, memory_order_relaxed);
            if (meta->hole_cursor >= JABS_GC_LINES_PER_BLOCK) {
                gc_recycle_head = meta->next_idx;
                meta->next_idx = UINT32_MAX;
            }
            return (jabs_object *)start_addr;
        }
        gc_recycle_head = meta->next_idx;
        meta->next_idx = UINT32_MAX;
    }

    /* 2. Allocate from cached FREE block or commit fresh block in arena. */
    uint32_t b = UINT32_MAX;
    if (gc_free_head != UINT32_MAX) {
        b = gc_free_head;
        gc_free_head = gc_blocks[b].next_idx;
        gc_blocks[b].next_idx = UINT32_MAX;
        gc_free_blocks--;
    } else {
        for (uint32_t idx = 0u; idx < JABS_GC_ARENA_BLOCKS; idx++) {
            if (gc_blocks[idx].state == JABS_GC_BLK_UNCOMMITTED) {
                if (!gc_commit_block(idx)) return NULL;
                b = idx;
                break;
            }
        }
    }
    if (b == UINT32_MAX) return NULL;
    if (b > gc_highest_block) gc_highest_block = b;

    jabs_gc_block_meta *meta = &gc_blocks[b];
    uintptr_t bbase = gc_arena_base + ((uintptr_t)b << JABS_GC_BLOCK_SHIFT);
    if (!meta->zeroed) {
        memset((void *)bbase, 0, JABS_GC_BLOCK_SIZE);
        meta->zeroed = 1u;
    }
    meta->state = JABS_GC_BLK_ACTIVE;
    memset(meta->line_bits, 0xff, sizeof meta->line_bits);
    meta->hole_cursor = JABS_GC_LINES_PER_BLOCK;
    meta->alloc_bits[0] |= UINT64_C(1);

    if (fill_tlab) {
        jabs_tls_tlab.cursor = bbase + asize;
        jabs_tls_tlab.limit = bbase + JABS_GC_BLOCK_SIZE;
        jabs_tls_tlab.block_base = bbase;
        jabs_tls_tlab.alloc_bits = meta->alloc_bits;
    } else if (asize < JABS_GC_BLOCK_SIZE) {
        /* Leave rest of fresh block usable on future cycles by placing back only if lines remain?
         * Caller asked >4KB single medium object: if fill_tlab==0, mark remaining lines unmarked for reuse! */
        uint32_t used_lines = (uint32_t)((asize + JABS_GC_LINE_SIZE - 1u) >> JABS_GC_LINE_SHIFT);
        memset(meta->line_bits, 0, sizeof meta->line_bits);
        for (uint32_t l = 0u; l < used_lines; l++) meta->line_bits[l >> 6] |= (UINT64_C(1) << (l & 63u));
        meta->hole_cursor = (uint16_t)used_lines;
        meta->next_idx = gc_recycle_head;
        gc_recycle_head = b;
    }

    size_t add_used = fill_tlab ? JABS_GC_BLOCK_SIZE : asize;
    size_t used = atomic_fetch_add_explicit(&gc_used_bytes, add_used, memory_order_relaxed) + add_used;
    size_t committed = ((size_t)gc_committed_blocks << JABS_GC_BLOCK_SHIFT) + gc_los_bytes;
    atomic_store_explicit(&gc_committed_bytes, committed, memory_order_relaxed);
    atomic_store_explicit(&jabs_heap_bytes, (int64_t)used, memory_order_relaxed);
    return (jabs_object *)bbase;
}

static jabs_object *gc_alloc_los(size_t asize) {
    size_t total = sizeof(jabs_gc_los_node) + asize;
    if (total < sizeof(jabs_gc_los_node)) return NULL;
    if (!gc_heap_has_room(asize)) return NULL;
#ifdef _WIN32
    jabs_gc_los_node *node = (jabs_gc_los_node *)VirtualAlloc(NULL, total, MEM_RESERVE | MEM_COMMIT, PAGE_READWRITE);
#else
    jabs_gc_los_node *node = (jabs_gc_los_node *)mmap(NULL, total, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (node == MAP_FAILED) node = NULL;
#endif
    if (!node) return NULL;
    node->payload_size = asize;
    node->map_size = total;
    node->marked = 0u;
    node->prev = NULL;
    node->next = gc_los_head;
    if (gc_los_head) gc_los_head->prev = node;
    gc_los_head = node;
    gc_los_count++;
    gc_los_bytes += asize;

    size_t used = atomic_fetch_add_explicit(&gc_used_bytes, asize, memory_order_relaxed) + asize;
    size_t committed = ((size_t)gc_committed_blocks << JABS_GC_BLOCK_SHIFT) + gc_los_bytes;
    atomic_store_explicit(&gc_committed_bytes, committed, memory_order_relaxed);
    atomic_store_explicit(&jabs_heap_bytes, (int64_t)used, memory_order_relaxed);
    return (jabs_object *)(node + 1);
}

jabs_object *jabs_gc_alloc_obj(size_t size, const jabs_class *cls) {
    jabs_gc_thread *self = gc_ensure_thread();
    size_t asize = (size + (JABS_GC_GRANULE_SIZE - 1u)) & ~(size_t)(JABS_GC_GRANULE_SIZE - 1u);
    if (asize < JABS_GC_GRANULE_SIZE) asize = JABS_GC_GRANULE_SIZE;

    gc_acquire_heap_safely();

    size_t cur_used = atomic_load_explicit(&gc_used_bytes, memory_order_relaxed);
    if (cur_used + asize > gc_next_threshold) {
        gc_collect_locked(self, 0);
    }

    jabs_object *o = NULL;
    if (asize <= JABS_GC_MAX_SMALL_SIZE) {
        int use_tlab = asize <= 4096u;
        o = gc_alloc_small_in_block(asize, use_tlab);
        if (!o) {
            gc_collect_locked(self, 1);
            o = gc_alloc_small_in_block(asize, use_tlab);
        }
    } else {
        o = gc_alloc_los(asize);
        if (!o) {
            gc_collect_locked(self, 1);
            o = gc_alloc_los(asize);
        }
    }

    gc_heap_unlock();
    if (!o) {
        jabs_pending = jabs_vm_throwable(JABS_VM_OUT_OF_MEMORY);
        return NULL;
    }
    o->cls = cls;
    return o;
}

jabs_object *jabs_gc_oom_object(const jabs_class *cls) {
    if (gc_oom_cached) return gc_oom_cached;
    if (!cls) return NULL;
    gc_acquire_heap_safely();
    if (!gc_oom_cached) {
        size_t asize = (cls->size + (JABS_GC_GRANULE_SIZE - 1u)) & ~(size_t)(JABS_GC_GRANULE_SIZE - 1u);
        if (asize < JABS_GC_GRANULE_SIZE) asize = JABS_GC_GRANULE_SIZE;
        jabs_object *o = gc_alloc_small_in_block(asize, 0);
        if (!o) {
            o = (jabs_object *)calloc(1, asize);
            gc_oom_immortal = o != NULL;
        }
        if (o) o->cls = cls;
        gc_oom_cached = o;
    }
    gc_heap_unlock();
    return gc_oom_cached;
}

int64_t jabs_gc_total_memory(void) {
    gc_init_once_run();
    size_t c = atomic_load_explicit(&gc_committed_bytes, memory_order_relaxed);
    size_t u = atomic_load_explicit(&gc_used_bytes, memory_order_relaxed);
    return (int64_t)(c > u ? c : u);
}

int64_t jabs_gc_used_memory(void) {
    gc_init_once_run();
    return (int64_t)atomic_load_explicit(&gc_used_bytes, memory_order_relaxed);
}

int64_t jabs_gc_free_memory(void) {
    int64_t total = jabs_gc_total_memory();
    int64_t used = jabs_gc_used_memory();
    return total > used ? total - used : 0;
}

int64_t jabs_gc_max_memory(void) {
    gc_init_once_run(); /* the arena's size is chosen at init */
    return (int64_t)JABS_GC_ARENA_BYTES;
}

void jabs_gc_collect(void) {
    jabs_gc_thread *self = gc_ensure_thread();
    gc_acquire_heap_safely();
    gc_collect_locked(self, 1);
    gc_heap_unlock();
}

/* ---- ReferenceHandler pending list queue ---- */

int32_t jabs_gc_has_pending_refs(void) {
    gc_init_once_run();
#ifdef _WIN32
    EnterCriticalSection(&gc_ref_cs);
    int32_t has = gc_pending_refs_head != NULL ? 1 : 0;
    LeaveCriticalSection(&gc_ref_cs);
    return has;
#else
    pthread_mutex_lock(&gc_ref_mu);
    int32_t has = gc_pending_refs_head != NULL ? 1 : 0;
    pthread_mutex_unlock(&gc_ref_mu);
    return has;
#endif
}

jabs_object *jabs_gc_take_pending_refs(void) {
    gc_init_once_run();
#ifdef _WIN32
    EnterCriticalSection(&gc_ref_cs);
    jabs_object *head = gc_pending_refs_head;
    gc_pending_refs_head = NULL;
    LeaveCriticalSection(&gc_ref_cs);
    return head;
#else
    pthread_mutex_lock(&gc_ref_mu);
    jabs_object *head = gc_pending_refs_head;
    gc_pending_refs_head = NULL;
    pthread_mutex_unlock(&gc_ref_mu);
    return head;
#endif
}

void jabs_gc_wait_pending_refs(void) {
    gc_init_once_run();
    jabs_gc_enter_native();
#ifdef _WIN32
    EnterCriticalSection(&gc_ref_cs);
    while (gc_pending_refs_head == NULL) {
        SleepConditionVariableCS(&gc_ref_cv, &gc_ref_cs, INFINITE);
    }
    LeaveCriticalSection(&gc_ref_cs);
#else
    pthread_mutex_lock(&gc_ref_mu);
    while (gc_pending_refs_head == NULL) {
        pthread_cond_wait(&gc_ref_cv, &gc_ref_mu);
    }
    pthread_mutex_unlock(&gc_ref_mu);
#endif
    jabs_gc_exit_native();
}
