//! `LD_PRELOAD` allocator that places selected large allocations on CXL devdax.
//!
//! `dax-mmap` is allocation-selective: allocations at least
//! `CXLALLOC_MIN_SIZE` bytes use `/dev/dax0.0`, while smaller allocations use
//! the real libc allocator in DRAM. A selected allocation never spills into
//! DRAM.

#![allow(clippy::missing_safety_doc)]

mod dax_arena;
mod real;

use core::ffi;
use core::ptr;
use core::ptr::NonNull;
use std::ffi::CString;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::sync::OnceLock;

use dax_arena::ArenaError;
use dax_arena::DaxArena;
use real::RealAlloc;

#[cfg(test)]
unsafe extern "C" {
    fn __libc_malloc(size: usize) -> *mut ffi::c_void;
    fn __libc_calloc(count: usize, size: usize) -> *mut ffi::c_void;
    fn __libc_realloc(pointer: *mut ffi::c_void, size: usize) -> *mut ffi::c_void;
    fn __libc_free(pointer: *mut ffi::c_void);
    fn __libc_memalign(alignment: usize, size: usize) -> *mut ffi::c_void;
}

static STATE: OnceLock<State> = OnceLock::new();
static INITIALIZED: AtomicBool = AtomicBool::new(false);

struct PlacementPolicy {
    min_size: usize,
}

impl PlacementPolicy {
    const DEFAULT_MIN_SIZE: usize = 2 << 20;

    fn new(min_size: usize) -> Self {
        Self { min_size }
    }

    fn from_env() -> Self {
        let min_size = std::env::var("CXLALLOC_MIN_SIZE")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(Self::DEFAULT_MIN_SIZE);
        Self::new(min_size)
    }

    fn uses_cxl(&self, size: usize) -> bool {
        size >= self.min_size
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BackendMode {
    Dax,
    DaxMmap,
    Mmap,
}

impl BackendMode {
    fn parse(value: Option<&str>) -> Result<Self, &'static str> {
        match value.unwrap_or("dax-mmap") {
            "dax" => Ok(Self::Dax),
            "dax-mmap" => Ok(Self::DaxMmap),
            "mmap" => Ok(Self::Mmap),
            _ => Err("cxlalloc-preload: unsupported CXLALLOC_BACKEND\n"),
        }
    }
}

struct State {
    real: &'static RealAlloc,
    policy: PlacementPolicy,
    arena: Option<DaxArena>,
}

impl State {
    fn from_env() -> Result<Self, &'static str> {
        let real = RealAlloc::resolve()?;
        let backend_value = std::env::var("CXLALLOC_BACKEND").ok();
        let backend = BackendMode::parse(backend_value.as_deref())?;
        let policy = PlacementPolicy::from_env();
        let arena = match backend {
            BackendMode::Dax | BackendMode::DaxMmap => {
                let devices = std::env::var("CXLALLOC_DAX_DEVICES")
                    .unwrap_or_else(|_| "/dev/dax0.0".to_owned());
                let first = devices.split(',').next().unwrap_or("").trim();
                CString::new(first)
                    .ok()
                    .and_then(|path| DaxArena::open(path.as_c_str()).ok())
            }
            BackendMode::Mmap => {
                let size = std::env::var("CXLALLOC_HEAP_SIZE")
                    .ok()
                    .and_then(|value| value.parse().ok())
                    .unwrap_or(256 << 20);
                DaxArena::anonymous(size, PlacementPolicy::DEFAULT_MIN_SIZE).ok()
            }
        };

        Ok(Self {
            real,
            policy,
            arena,
        })
    }

    #[cfg(test)]
    fn anonymous_for_test(size: usize, min_size: usize) -> Result<Self, ArenaError> {
        let real = RealAlloc::resolve().map_err(|_| ArenaError::InvalidDevice)?;
        Ok(Self {
            real,
            policy: PlacementPolicy::new(min_size),
            arena: Some(DaxArena::anonymous(size, min_size)?),
        })
    }

    fn arena_owns(&self, pointer: *const ffi::c_void) -> bool {
        self.arena.as_ref().is_some_and(|arena| arena.owns(pointer))
    }
}

// ---------------------------------------------------------------------------
// Early bump allocator
// ---------------------------------------------------------------------------

#[cfg(not(test))]
const EARLY_SIZE: usize = 8 << 20;
#[cfg(test)]
const EARLY_SIZE: usize = 64 << 20;

#[repr(C, align(16))]
struct EarlyBuf(core::cell::UnsafeCell<[u8; EARLY_SIZE]>);
unsafe impl Sync for EarlyBuf {}
static EARLY_BUF: EarlyBuf = EarlyBuf(core::cell::UnsafeCell::new([0u8; EARLY_SIZE]));
static EARLY_OFFSET: AtomicUsize = AtomicUsize::new(0);

fn early_buf_base() -> *mut u8 {
    EARLY_BUF.0.get().cast()
}

fn early_malloc(size: usize) -> *mut ffi::c_void {
    let alignment = 16;
    let Some(size) = size
        .max(alignment)
        .checked_add(alignment - 1)
        .map(|value| value & !(alignment - 1))
    else {
        return ptr::null_mut();
    };
    let Ok(offset) = EARLY_OFFSET.fetch_update(Ordering::AcqRel, Ordering::Relaxed, |offset| {
        offset.checked_add(size).filter(|end| *end <= EARLY_SIZE)
    }) else {
        return ptr::null_mut();
    };
    unsafe { early_buf_base().add(offset).cast() }
}

fn is_early_pointer(pointer: *mut ffi::c_void) -> bool {
    let base = early_buf_base() as usize;
    let address = pointer as usize;
    (base..base + EARLY_SIZE).contains(&address)
}

// ---------------------------------------------------------------------------
// Constructor
// ---------------------------------------------------------------------------

#[cfg(not(test))]
#[used]
#[link_section = ".init_array"]
static INIT: unsafe extern "C" fn() = init;

unsafe extern "C" fn init() {
    match State::from_env() {
        Ok(state) => {
            let _ = STATE.set(state);
            INITIALIZED.store(true, Ordering::Release);
        }
        Err(message) => write_diagnostic(message.as_bytes()),
    }
}

fn write_diagnostic(message: &[u8]) {
    unsafe {
        libc::write(libc::STDERR_FILENO, message.as_ptr().cast(), message.len());
    }
}

fn state() -> Option<&'static State> {
    INITIALIZED
        .load(Ordering::Acquire)
        .then(|| STATE.get())
        .flatten()
}

fn set_errno(value: libc::c_int) {
    unsafe { *libc::__errno_location() = value };
}

fn errno() -> libc::c_int {
    unsafe { *libc::__errno_location() }
}

fn page_size() -> usize {
    usize::try_from(unsafe { libc::sysconf(libc::_SC_PAGESIZE) })
        .ok()
        .filter(|size| *size != 0)
        .unwrap_or(4096)
}

// ---------------------------------------------------------------------------
// Routed allocation implementation
// ---------------------------------------------------------------------------

unsafe fn routed_malloc(size: usize, state: &State) -> *mut ffi::c_void {
    if !state.policy.uses_cxl(size) {
        return (state.real.malloc)(size);
    }
    let Some(arena) = state.arena.as_ref() else {
        set_errno(libc::ENOMEM);
        return ptr::null_mut();
    };
    match arena.allocate(size, 16) {
        Ok(pointer) => pointer.as_ptr(),
        Err(_) => {
            set_errno(libc::ENOMEM);
            ptr::null_mut()
        }
    }
}

unsafe fn routed_aligned_alloc(alignment: usize, size: usize, state: &State) -> *mut ffi::c_void {
    if !alignment.is_power_of_two() {
        set_errno(libc::EINVAL);
        return ptr::null_mut();
    }
    if !state.policy.uses_cxl(size) {
        return (state.real.memalign)(alignment, size);
    }
    let Some(arena) = state.arena.as_ref() else {
        set_errno(libc::ENOMEM);
        return ptr::null_mut();
    };
    match arena.allocate(size, alignment) {
        Ok(pointer) => pointer.as_ptr(),
        Err(ArenaError::InvalidAlignment) => {
            set_errno(libc::EINVAL);
            ptr::null_mut()
        }
        Err(_) => {
            set_errno(libc::ENOMEM);
            ptr::null_mut()
        }
    }
}

unsafe fn routed_valloc(size: usize, state: &State) -> *mut ffi::c_void {
    routed_aligned_alloc(page_size(), size, state)
}

unsafe fn routed_free(pointer: *mut ffi::c_void, state: &State) {
    if pointer.is_null() || is_early_pointer(pointer) {
        return;
    }
    if state.arena_owns(pointer) {
        if state
            .arena
            .as_ref()
            .unwrap()
            .deallocate(NonNull::new_unchecked(pointer))
            .is_err()
        {
            set_errno(libc::EINVAL);
        }
    } else {
        (state.real.free)(pointer);
    }
}

unsafe fn routed_calloc(count: usize, size: usize, state: &State) -> *mut ffi::c_void {
    let Some(total) = count.checked_mul(size) else {
        set_errno(libc::ENOMEM);
        return ptr::null_mut();
    };
    if !state.policy.uses_cxl(total) {
        return (state.real.calloc)(count, size);
    }
    let pointer = routed_malloc(total, state);
    if !pointer.is_null() {
        scalar_fill(pointer.cast(), 0, total);
    }
    pointer
}

unsafe fn routed_realloc(
    pointer: *mut ffi::c_void,
    size: usize,
    state: &State,
) -> *mut ffi::c_void {
    if pointer.is_null() {
        return routed_malloc(size, state);
    }
    if size == 0 {
        routed_free(pointer, state);
        return ptr::null_mut();
    }
    if is_early_pointer(pointer) {
        let new_pointer = routed_malloc(size, state);
        if !new_pointer.is_null() {
            let old_size = EARLY_SIZE.saturating_sub(pointer as usize - early_buf_base() as usize);
            scalar_copy(new_pointer.cast(), pointer.cast(), size.min(old_size));
        }
        return new_pointer;
    }

    let old_size = if state.arena_owns(pointer) {
        state
            .arena
            .as_ref()
            .and_then(|arena| arena.allocation_size(NonNull::new_unchecked(pointer)))
            .unwrap_or(0)
    } else if !state.policy.uses_cxl(size) {
        return (state.real.realloc)(pointer, size);
    } else {
        (state.real.malloc_usable_size)(pointer)
    };

    let new_pointer = routed_malloc(size, state);
    if new_pointer.is_null() {
        return ptr::null_mut();
    }
    scalar_copy(new_pointer.cast(), pointer.cast(), old_size.min(size));
    routed_free(pointer, state);
    new_pointer
}

unsafe fn scalar_fill(destination: *mut u8, value: u8, size: usize) {
    for index in 0..size {
        ptr::write_volatile(destination.add(index), value);
    }
}

unsafe fn scalar_copy(destination: *mut u8, source: *const u8, size: usize) {
    for index in 0..size {
        let value = ptr::read_volatile(source.add(index));
        ptr::write_volatile(destination.add(index), value);
    }
}

// ---------------------------------------------------------------------------
// Interposed C allocation API
// ---------------------------------------------------------------------------

#[no_mangle]
pub unsafe extern "C" fn malloc(size: usize) -> *mut ffi::c_void {
    #[cfg(test)]
    {
        return __libc_malloc(size);
    }
    #[cfg(not(test))]
    state()
        .map(|state| routed_malloc(size, state))
        .unwrap_or_else(|| early_malloc(size))
}

#[no_mangle]
pub unsafe extern "C" fn free(pointer: *mut ffi::c_void) {
    #[cfg(test)]
    {
        __libc_free(pointer);
        return;
    }
    #[cfg(not(test))]
    if let Some(state) = state() {
        routed_free(pointer, state);
    }
}

#[no_mangle]
pub unsafe extern "C" fn calloc(count: usize, size: usize) -> *mut ffi::c_void {
    #[cfg(test)]
    {
        return __libc_calloc(count, size);
    }
    #[cfg(not(test))]
    state()
        .map(|state| routed_calloc(count, size, state))
        .unwrap_or_else(|| {
            let total = count.checked_mul(size).unwrap_or(usize::MAX);
            let pointer = early_malloc(total);
            if !pointer.is_null() {
                scalar_fill(pointer.cast(), 0, total);
            }
            pointer
        })
}

#[no_mangle]
pub unsafe extern "C" fn realloc(pointer: *mut ffi::c_void, size: usize) -> *mut ffi::c_void {
    #[cfg(test)]
    {
        return __libc_realloc(pointer, size);
    }
    #[cfg(not(test))]
    state()
        .map(|state| routed_realloc(pointer, size, state))
        .unwrap_or(ptr::null_mut())
}

#[no_mangle]
pub unsafe extern "C" fn memalign(alignment: usize, size: usize) -> *mut ffi::c_void {
    #[cfg(test)]
    {
        return __libc_memalign(alignment, size);
    }
    #[cfg(not(test))]
    state()
        .map(|state| routed_aligned_alloc(alignment, size, state))
        .unwrap_or_else(|| early_malloc(size))
}

#[no_mangle]
pub unsafe extern "C" fn posix_memalign(
    output: *mut *mut ffi::c_void,
    alignment: usize,
    size: usize,
) -> libc::c_int {
    if output.is_null()
        || alignment < core::mem::size_of::<*mut ffi::c_void>()
        || !alignment.is_power_of_two()
    {
        return libc::EINVAL;
    }
    let pointer = memalign(alignment, size);
    if pointer.is_null() {
        libc::ENOMEM
    } else {
        *output = pointer;
        0
    }
}

#[no_mangle]
pub unsafe extern "C" fn aligned_alloc(alignment: usize, size: usize) -> *mut ffi::c_void {
    if alignment == 0 || size % alignment != 0 {
        set_errno(libc::EINVAL);
        return ptr::null_mut();
    }
    memalign(alignment, size)
}

#[no_mangle]
pub unsafe extern "C" fn valloc(size: usize) -> *mut ffi::c_void {
    #[cfg(test)]
    {
        return __libc_memalign(page_size(), size);
    }
    #[cfg(not(test))]
    state()
        .map(|state| routed_valloc(size, state))
        .unwrap_or_else(|| early_malloc(size))
}

#[no_mangle]
pub unsafe extern "C" fn pvalloc(size: usize) -> *mut ffi::c_void {
    let page = page_size();
    let Some(rounded) = size.checked_add(page - 1).map(|value| value & !(page - 1)) else {
        set_errno(libc::ENOMEM);
        return ptr::null_mut();
    };
    valloc(rounded)
}

#[no_mangle]
pub unsafe extern "C" fn malloc_usable_size(pointer: *mut ffi::c_void) -> usize {
    #[cfg(test)]
    {
        let _ = pointer;
        return 0;
    }
    #[cfg(not(test))]
    {
        if pointer.is_null() || is_early_pointer(pointer) {
            return 0;
        }
        let Some(state) = state() else {
            return 0;
        };
        if state.arena_owns(pointer) {
            state
                .arena
                .as_ref()
                .and_then(|arena| arena.allocation_size(NonNull::new_unchecked(pointer)))
                .unwrap_or(0)
        } else {
            (state.real.malloc_usable_size)(pointer)
        }
    }
}

// ---------------------------------------------------------------------------
// MMIO-safe memory primitives
// ---------------------------------------------------------------------------

#[no_mangle]
pub unsafe extern "C" fn memset(
    destination: *mut ffi::c_void,
    value: libc::c_int,
    size: usize,
) -> *mut ffi::c_void {
    core::arch::asm!(
        "rep stosb",
        inout("rdi") destination => _,
        inout("rcx") size => _,
        in("al") value as u8,
        options(nostack, preserves_flags),
    );
    destination
}

#[no_mangle]
pub unsafe extern "C" fn memcpy(
    destination: *mut ffi::c_void,
    source: *const ffi::c_void,
    size: usize,
) -> *mut ffi::c_void {
    core::arch::asm!(
        "rep movsb",
        inout("rdi") destination => _,
        inout("rsi") source => _,
        inout("rcx") size => _,
        options(nostack, preserves_flags),
    );
    destination
}

#[no_mangle]
pub unsafe extern "C" fn memmove(
    destination: *mut ffi::c_void,
    source: *const ffi::c_void,
    size: usize,
) -> *mut ffi::c_void {
    if size == 0 || (destination as usize) <= (source as usize) {
        return memcpy(destination, source, size);
    }

    let destination_end = destination.cast::<u8>().add(size - 1);
    let source_end = source.cast::<u8>().add(size - 1);
    core::arch::asm!(
        "std",
        "rep movsb",
        "cld",
        inout("rdi") destination_end => _,
        inout("rsi") source_end => _,
        inout("rcx") size => _,
        options(nostack),
    );
    destination
}

#[cfg(test)]
mod tests {
    use super::{
        errno, routed_aligned_alloc, routed_calloc, routed_free, routed_malloc, routed_realloc,
        routed_valloc, BackendMode, PlacementPolicy, State,
    };

    #[test]
    fn default_policy_keeps_small_allocations_in_dram() {
        let policy = PlacementPolicy::new(2 << 20);
        assert!(!policy.uses_cxl(4096));
    }

    #[test]
    fn default_policy_routes_128_mib_to_cxl() {
        let policy = PlacementPolicy::new(2 << 20);
        assert!(policy.uses_cxl(128 << 20));
    }

    #[test]
    fn backend_mode_defaults_to_dax_mmap() {
        assert_eq!(BackendMode::parse(None).unwrap(), BackendMode::DaxMmap);
    }

    #[test]
    fn backend_mode_preserves_explicit_controls() {
        assert_eq!(BackendMode::parse(Some("dax")).unwrap(), BackendMode::Dax);
        assert_eq!(
            BackendMode::parse(Some("dax-mmap")).unwrap(),
            BackendMode::DaxMmap,
        );
        assert_eq!(BackendMode::parse(Some("mmap")).unwrap(), BackendMode::Mmap);
        assert!(BackendMode::parse(Some("unknown")).is_err());
    }

    #[test]
    fn routing_small_malloc_uses_libc_and_large_malloc_uses_arena() {
        let state = State::anonymous_for_test(256 << 20, 2 << 20).unwrap();
        let small = unsafe { routed_malloc(4096, &state) };
        let large = unsafe { routed_malloc(128 << 20, &state) };
        assert!(!small.is_null());
        assert!(!large.is_null());
        assert!(!state.arena_owns(small));
        assert!(state.arena_owns(large));
        unsafe {
            routed_free(small, &state);
            routed_free(large, &state);
        }
    }

    #[test]
    fn routing_valloc_is_page_aligned_and_arena_owned() {
        let state = State::anonymous_for_test(256 << 20, 2 << 20).unwrap();
        let pointer = unsafe { routed_valloc(128 << 20, &state) };
        assert!(!pointer.is_null());
        assert_eq!((pointer as usize) % 4096, 0);
        assert!(state.arena_owns(pointer));
        unsafe { routed_free(pointer, &state) };
    }

    #[test]
    fn routing_arena_oom_returns_null_without_libc_fallback() {
        let state = State::anonymous_for_test(4 << 20, 2 << 20).unwrap();
        let pointer = unsafe { routed_malloc(8 << 20, &state) };
        assert!(pointer.is_null());
        assert_eq!(errno(), libc::ENOMEM);
    }

    #[test]
    fn routing_large_calloc_zeroes_the_arena_allocation() {
        let state = State::anonymous_for_test(8 << 20, 2 << 20).unwrap();
        let pointer = unsafe { routed_calloc(1, 2 << 20, &state) };
        assert!(!pointer.is_null());
        assert_eq!(unsafe { pointer.cast::<u8>().read_volatile() }, 0);
        assert_eq!(
            unsafe { pointer.cast::<u8>().add((2 << 20) - 1).read_volatile() },
            0,
        );
        unsafe { routed_free(pointer, &state) };
    }

    #[test]
    fn routing_realloc_preserves_arena_bytes_and_ownership() {
        let state = State::anonymous_for_test(16 << 20, 2 << 20).unwrap();
        let pointer = unsafe { routed_malloc(2 << 20, &state) };
        unsafe {
            pointer.cast::<u8>().write_volatile(0x5a);
            pointer.cast::<u8>().add((2 << 20) - 1).write_volatile(0xa5);
        }
        let grown = unsafe { routed_realloc(pointer, 4 << 20, &state) };
        assert!(!grown.is_null());
        assert!(state.arena_owns(grown));
        assert_eq!(unsafe { grown.cast::<u8>().read_volatile() }, 0x5a);
        assert_eq!(
            unsafe { grown.cast::<u8>().add((2 << 20) - 1).read_volatile() },
            0xa5,
        );
        unsafe { routed_free(grown, &state) };
    }

    #[test]
    fn routing_aligned_allocation_rejects_invalid_alignment() {
        let state = State::anonymous_for_test(8 << 20, 2 << 20).unwrap();
        let pointer = unsafe { routed_aligned_alloc(24, 2 << 20, &state) };
        assert!(pointer.is_null());
        assert_eq!(errno(), libc::EINVAL);
    }
}
