//! LD_PRELOAD shared library for transparent CXL DAX + main memory interleaving.
//!
//! Intercepts standard `malloc`/`free`/`realloc`/`calloc`/`memalign` calls and
//! routes them through cxlalloc with the configured backend.
//!
//! # Environment variables
//!
//! | Variable | Default | Description |
//! |----------|---------|-------------|
//! | `CXLALLOC_BACKEND` | `dax-mmap` | Backend: `mmap`, `shm`, `dax`, `dax-mmap` |
//! | `CXLALLOC_DAX_DEVICES` | `/dev/dax0.0` | Comma-separated DAX device paths |
//! | `CXLALLOC_HEAP_SIZE` | `4294967296` (4 GiB) | Total heap size in bytes |
//! | `CXLALLOC_MAX_THREADS` | `64` | Maximum number of concurrent threads |
//! | `CXLALLOC_NUMA` | (none) | NUMA node to bind heap memory to |
//!
//! # Usage
//!
//! ```bash
//! cargo build --release -p cxlalloc-preload
//!
//! # Test with mmap backend (no CXL hardware needed):
//! CXLALLOC_BACKEND=mmap \
//!   LD_PRELOAD=target/release/libcxlalloc_preload.so ./my_application
//!
//! # CXL DAX + DRAM interleaving:
//! CXLALLOC_BACKEND=dax-mmap CXLALLOC_DAX_DEVICES=/dev/dax0.0 \
//!   LD_PRELOAD=target/release/libcxlalloc_preload.so ./my_application
//! ```

#![allow(clippy::missing_safety_doc)]

use core::ffi;
use core::mem;
use core::ptr;
use core::ptr::NonNull;
use std::alloc::Layout;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::sync::OnceLock;

use cxlalloc::raw;

// ---------------------------------------------------------------------------
// Global state
// ---------------------------------------------------------------------------

static RAW: OnceLock<raw::Raw> = OnceLock::new();

/// Set to true once the allocator is fully initialized.
static INITIALIZED: AtomicBool = AtomicBool::new(false);


// ---------------------------------------------------------------------------
// Early bump allocator
//
// Before the cxlalloc constructor runs (and for any re-entrant malloc calls
// during initialization), we service allocations from a static buffer.
// This avoids all TLS and Rust runtime dependencies.
// ---------------------------------------------------------------------------

const EARLY_SIZE: usize = 8 << 20; // 8 MiB

/// Early bump buffer, 16-byte aligned so that allocations returned from
/// `early_malloc` satisfy the alignment requirements of all standard types.
#[repr(C, align(16))]
struct EarlyBuf(core::cell::UnsafeCell<[u8; EARLY_SIZE]>);
unsafe impl Sync for EarlyBuf {}
static EARLY_BUF: EarlyBuf = EarlyBuf(core::cell::UnsafeCell::new([0u8; EARLY_SIZE]));
static EARLY_OFFSET: AtomicUsize = AtomicUsize::new(0);

fn early_buf_base() -> *mut u8 {
    EARLY_BUF.0.get().cast()
}

fn early_malloc(size: usize) -> *mut ffi::c_void {
    let align = 16;
    let size = if size == 0 { align } else { (size + align - 1) & !(align - 1) };
    let offset = EARLY_OFFSET.fetch_add(size, Ordering::Relaxed);
    if offset + size > EARLY_SIZE {
        unsafe {
            let p = libc::mmap(
                ptr::null_mut(),
                size,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_ANONYMOUS | libc::MAP_PRIVATE,
                -1,
                0,
            );
            if p == libc::MAP_FAILED {
                ptr::null_mut()
            } else {
                p
            }
        }
    } else {
        unsafe { early_buf_base().add(offset).cast() }
    }
}

fn is_early_pointer(p: *mut ffi::c_void) -> bool {
    let base = early_buf_base() as usize;
    let addr = p as usize;
    addr >= base && addr < base + EARLY_SIZE
}

// ---------------------------------------------------------------------------
// Thread ID management — no TLS, uses gettid() syscall + global registry
//
// This avoids the classic LD_PRELOAD re-entrancy problem where Rust's
// thread_local! macro calls malloc during TLS initialization.
// ---------------------------------------------------------------------------

const MAX_SLOTS: usize = 512;

/// Each slot stores a Linux TID (0 = empty).
static TID_SLOTS: [AtomicU64; MAX_SLOTS] = {
    const ZERO: AtomicU64 = AtomicU64::new(0);
    [ZERO; MAX_SLOTS]
};
static NEXT_SLOT: AtomicUsize = AtomicUsize::new(0);

/// Per-slot re-entrancy guard. When true, the thread in this slot is already
/// inside cxlalloc (e.g., `raw.allocator()` uses Vec internally which calls malloc).
/// Re-entrant calls fall back to early_malloc.
static IN_CXLALLOC: [AtomicBool; MAX_SLOTS] = {
    const FALSE: AtomicBool = AtomicBool::new(false);
    [FALSE; MAX_SLOTS]
};

#[inline]
fn current_tid() -> u64 {
    unsafe { libc::syscall(libc::SYS_gettid) as u64 }
}

/// Map the current Linux TID to a cxlalloc slot (0..MAX_SLOTS-1).
/// Uses compare-and-swap registration — lock-free and malloc-free.
#[inline]
fn get_slot() -> usize {
    let tid = current_tid();

    // Fast path: scan existing registrations
    let limit = NEXT_SLOT.load(Ordering::Relaxed).min(MAX_SLOTS);
    for i in 0..limit {
        if TID_SLOTS[i].load(Ordering::Relaxed) == tid {
            return i;
        }
    }

    // Slow path: register a new slot
    let slot = NEXT_SLOT.fetch_add(1, Ordering::Relaxed);
    if slot >= MAX_SLOTS {
        // Saturate — share slot 0 as last resort
        NEXT_SLOT.store(MAX_SLOTS, Ordering::Relaxed);
        return 0;
    }
    TID_SLOTS[slot].store(tid, Ordering::Relaxed);
    slot
}

// ---------------------------------------------------------------------------
// SIGSEGV handler for lazy page mapping
// ---------------------------------------------------------------------------

extern "C" fn handle_sigsegv(
    _sig: libc::c_int,
    info: *const libc::siginfo_t,
    _ctx: *const libc::c_void,
) {
    let address = unsafe { (*info).si_addr() };
    if let Some(raw) = RAW.get() {
        let id = unsafe { cxlalloc::thread::Id::new(get_slot() as u16) };
        if raw.map(id, address) {
            return;
        }
    }

    // Not our fault — restore default handler
    unsafe {
        let mut action = mem::zeroed::<libc::sigaction>();
        action.sa_sigaction = libc::SIG_DFL;
        libc::sigaction(libc::SIGSEGV, &action, ptr::null_mut());
    }
}

// ---------------------------------------------------------------------------
// Constructor — runs when the .so is loaded (before main)
// ---------------------------------------------------------------------------

#[used]
#[link_section = ".init_array"]
static INIT: unsafe extern "C" fn() = init;

unsafe extern "C" fn init() {
    #[cfg(feature = "log")]
    let _ = env_logger::try_init();

    // Register the constructor thread as slot 0
    TID_SLOTS[0].store(current_tid(), Ordering::Relaxed);

    let backend_name = std::env::var("CXLALLOC_BACKEND")
        .unwrap_or_else(|_| "dax-mmap".to_owned());

    let heap_size: usize = std::env::var("CXLALLOC_HEAP_SIZE")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(256 << 20); // 256 MiB default (set larger via env var)

    let max_threads: usize = std::env::var("CXLALLOC_MAX_THREADS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(64);

    let numa_node: Option<shm::Numa> = std::env::var("CXLALLOC_NUMA")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .map(|node| shm::Numa::Bind { node });

    let backend = {
        // Use Physical populate for DAX backends so pages are pre-faulted and
        // can be zeroed before the allocator touches them.
        let populate_dax = Some(shm::Populate::Physical);
        let builder = raw::Backend::builder().maybe_numa(numa_node);
        match backend_name.as_str() {
            "mmap" => builder.backend(raw::backend::Mmap).build(),
            "shm" => builder.backend(raw::backend::Shm).build(),
            #[cfg(feature = "backend-dax")]
            "dax" => {
                let devices = dax_devices();
                let paths: Vec<&str> = devices.iter().map(String::as_str).collect();
                builder
                    .backend(shm::backend::Dax::new(&paths).expect("Failed to open DAX devices"))
                    .maybe_populate(populate_dax)
                    .build()
            }
            #[cfg(feature = "backend-dax")]
            "dax-mmap" => {
                let devices = dax_devices();
                let paths: Vec<&str> = devices.iter().map(String::as_str).collect();
                builder
                    .backend(
                        shm::backend::DaxMmap::new(&paths)
                            .expect("Failed to open DAX devices for dax-mmap interleaving"),
                    )
                    .maybe_populate(populate_dax)
                    .build()
            }
            other => {
                eprintln!(
                    "cxlalloc-preload: unknown CXLALLOC_BACKEND={other:?}, falling back to mmap"
                );
                builder.backend(raw::backend::Mmap).build()
            }
        }
    };

    RAW.get_or_init(|| {
        let raw = raw::Raw::builder()
            .backend(backend)
            .size_small(heap_size / 2)
            .size_large(heap_size / 2)
            .thread_count(max_threads)
            .build("cxlalloc-preload")
            .expect("cxlalloc-preload: failed to initialize allocator");

        // DAX persistent memory may contain stale data from previous runs.
        // Zero the metadata regions so the allocator starts with clean state.
        if backend_name != "mmap" && backend_name != "shm" {
            raw.zero_metadata();
        }

        raw
    });

    // Mark initialized — subsequent malloc calls go through cxlalloc
    INITIALIZED.store(true, Ordering::Release);

    // Install SIGSEGV handler for lazy page mapping.
    let mut action = mem::zeroed::<libc::sigaction>();
    action.sa_sigaction = handle_sigsegv as _;
    action.sa_flags = libc::SA_SIGINFO | libc::SA_NODEFER;
    libc::sigaction(libc::SIGSEGV, &action, ptr::null_mut());
}

#[cfg(feature = "backend-dax")]
fn dax_devices() -> Vec<String> {
    std::env::var("CXLALLOC_DAX_DEVICES")
        .unwrap_or_else(|_| "/dev/dax0.0".to_owned())
        .split(',')
        .map(|s| s.trim().to_owned())
        .collect()
}

// ---------------------------------------------------------------------------
// Standard allocator symbols — intercepted by LD_PRELOAD
//
// These use gettid + global registry instead of TLS, so they are safe to
// call at any point during process initialization.
// ---------------------------------------------------------------------------

#[no_mangle]
pub unsafe extern "C" fn malloc(size: usize) -> *mut ffi::c_void {
    if !INITIALIZED.load(Ordering::Acquire) {
        return early_malloc(size);
    }
    let slot = get_slot();
    // Guard against re-entrancy: raw.allocator() uses Vec (calls malloc)
    if IN_CXLALLOC[slot].swap(true, Ordering::Acquire) {
        return early_malloc(size);
    }
    let id = unsafe { cxlalloc::thread::Id::new(slot as u16) };
    let raw = RAW.get().unwrap();
    let mut allocator = raw.allocator::<(), ()>(id);
    let result = allocator.allocate_untyped(size);
    IN_CXLALLOC[slot].store(false, Ordering::Release);
    result
}

#[no_mangle]
pub unsafe extern "C" fn free(pointer: *mut ffi::c_void) {
    if pointer.is_null() || is_early_pointer(pointer) {
        return;
    }
    if !INITIALIZED.load(Ordering::Acquire) {
        return; // pre-init mmap pointer, leaked
    }
    if let Some(p) = NonNull::new(pointer) {
        let slot = get_slot();
        if IN_CXLALLOC[slot].swap(true, Ordering::Acquire) {
            return; // re-entrant free — leak (rare, from layout! Vec drop)
        }
        let id = unsafe { cxlalloc::thread::Id::new(slot as u16) };
        let raw = RAW.get().unwrap();
        let mut allocator = raw.allocator::<(), ()>(id);
        allocator.free_untyped(p.cast());
        IN_CXLALLOC[slot].store(false, Ordering::Release);
    }
}

#[no_mangle]
pub unsafe extern "C" fn calloc(count: usize, size: usize) -> *mut ffi::c_void {
    let total = count.saturating_mul(size);
    let p = malloc(total);
    if !p.is_null() {
        ptr::write_bytes(p.cast::<u8>(), 0, total);
    }
    p
}

#[no_mangle]
pub unsafe extern "C" fn realloc(pointer: *mut ffi::c_void, size: usize) -> *mut ffi::c_void {
    if pointer.is_null() {
        return malloc(size);
    }
    if size == 0 {
        free(pointer);
        return ptr::null_mut();
    }
    if is_early_pointer(pointer) {
        let new = malloc(size);
        if !new.is_null() {
            let base = early_buf_base() as usize;
            let old_offset = pointer as usize - base;
            let max_old = EARLY_SIZE.saturating_sub(old_offset);
            let copy_size = size.min(max_old);
            // Use ptr::copy (not copy_nonoverlapping) because both old and
            // new may come from the early bump buffer and overlap.
            ptr::copy(pointer.cast::<u8>(), new.cast::<u8>(), copy_size);
        }
        return new;
    }
    if !INITIALIZED.load(Ordering::Acquire) {
        return ptr::null_mut();
    }
    match NonNull::new(pointer) {
        None => malloc(size),
        Some(block) => {
            let slot = get_slot();
            if IN_CXLALLOC[slot].swap(true, Ordering::Acquire) {
                return early_malloc(size); // re-entrant
            }
            let id = unsafe { cxlalloc::thread::Id::new(slot as u16) };
            let raw = RAW.get().unwrap();
            let mut allocator = raw.allocator::<(), ()>(id);
            let result = allocator.realloc_untyped(block, size);
            IN_CXLALLOC[slot].store(false, Ordering::Release);
            result
        }
    }
}

#[no_mangle]
pub unsafe extern "C" fn memalign(alignment: usize, size: usize) -> *mut ffi::c_void {
    if let Ok(layout) = Layout::from_size_align(size, alignment) {
        malloc(layout.pad_to_align().size())
    } else {
        ptr::null_mut()
    }
}

#[no_mangle]
pub unsafe extern "C" fn posix_memalign(
    memptr: *mut *mut ffi::c_void,
    alignment: usize,
    size: usize,
) -> libc::c_int {
    if alignment < core::mem::size_of::<*mut ffi::c_void>() || !alignment.is_power_of_two() {
        return libc::EINVAL;
    }
    let p = memalign(alignment, size);
    if p.is_null() {
        libc::ENOMEM
    } else {
        *memptr = p;
        0
    }
}

#[no_mangle]
pub unsafe extern "C" fn aligned_alloc(alignment: usize, size: usize) -> *mut ffi::c_void {
    memalign(alignment, size)
}

#[no_mangle]
pub unsafe extern "C" fn malloc_usable_size(_pointer: *mut ffi::c_void) -> usize {
    0
}

// ---------------------------------------------------------------------------
// Safe memset/memcpy/memmove — override glibc's SIMD versions
//
// glibc's memset/memcpy use AVX-512 non-temporal stores (vmovntdq) which
// trigger SIGILL on QEMU-emulated CXL DAX memory. We override them with
// `rep stosb`/`rep movsb` which use simple stores that work everywhere.
//
// `rep movsb` and `rep stosb` are fast on modern x86 CPUs with ERMS
// (Enhanced REP MOVSB/STOSB) — comparable to hand-tuned SIMD loops for
// most practical sizes.
// ---------------------------------------------------------------------------

#[no_mangle]
pub unsafe extern "C" fn memset(
    dest: *mut ffi::c_void,
    c: libc::c_int,
    n: usize,
) -> *mut ffi::c_void {
    core::arch::asm!(
        "rep stosb",
        inout("rdi") dest => _,
        inout("rcx") n => _,
        in("al") c as u8,
        options(nostack, preserves_flags),
    );
    dest
}

#[no_mangle]
pub unsafe extern "C" fn memcpy(
    dest: *mut ffi::c_void,
    src: *const ffi::c_void,
    n: usize,
) -> *mut ffi::c_void {
    core::arch::asm!(
        "rep movsb",
        inout("rdi") dest => _,
        inout("rsi") src => _,
        inout("rcx") n => _,
        options(nostack, preserves_flags),
    );
    dest
}

#[no_mangle]
pub unsafe extern "C" fn memmove(
    dest: *mut ffi::c_void,
    src: *const ffi::c_void,
    n: usize,
) -> *mut ffi::c_void {
    if (dest as usize) <= (src as usize) {
        // Forward copy is safe — no overlap issue
        memcpy(dest, src, n)
    } else {
        // Backward copy: use `std` direction flag with `rep movsb`
        let dest_end = (dest as *mut u8).add(n - 1);
        let src_end = (src as *const u8).add(n - 1);
        core::arch::asm!(
            "std",
            "rep movsb",
            "cld",
            inout("rdi") dest_end => _,
            inout("rsi") src_end => _,
            inout("rcx") n => _,
            options(nostack),
        );
        dest
    }
}
