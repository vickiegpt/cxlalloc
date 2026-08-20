# Selective devdax Preload Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make the unmodified 128 MiB `lat_mem_rd` working buffer reside entirely on `/dev/dax0.0` while keeping incidental allocations and allocator metadata in DRAM.

**Architecture:** Refactor `cxlalloc-preload` into a routing interposer. Real libc serves allocations smaller than a configurable threshold, while a new DRAM-managed arena maps `/dev/dax0.0` once and serves selected large allocations, including lmbench's `valloc`. Ownership-aware `free` and `realloc` dispatch to the correct allocator without placing control metadata on MMIO.

**Tech Stack:** Rust 2021, `libc`, Linux `mmap`/`flock`/process-shared pthread mutexes, C ABI interposition, Cargo tests, GCC, lmbench.

**Spec:** `docs/superpowers/specs/2026-08-20-devdax-selective-preload-design.md`

## Global Constraints

- `/dev/dax0.0` remains in `devdax` mode.
- The acceptance workload is capped at 128 MiB because the live device is 250 MiB.
- A CXL-selected allocation never silently spills into DRAM.
- Allocator metadata, locks, extent records, libc objects, and small allocations remain in DRAM.
- Do not add a `SIGILL` emulator or modify the lmbench executable.
- Use scalar MMIO-safe initialization and copies for CXL-owned memory.
- Preserve the existing unrelated `cxlalloc-bench/build.rs` modification and untracked `target/` artifacts.
- Each production change follows a witnessed failing test, minimal implementation, and passing regression test.

---

## File Structure

- Create `cxlalloc-preload/src/real.rs`: resolve and call the real libc allocation API without recursive interposition.
- Create `cxlalloc-preload/src/dax_arena.rs`: device discovery, devdax mapping, DRAM-resident process-shared extent management, ownership, allocation, and freeing.
- Modify `cxlalloc-preload/src/lib.rs`: initialization, placement policy, C ABI symbols, alignment handling, and dispatch.
- Create `cxlalloc-preload/tests/valloc_probe.c`: black-box C consumer that checks large `valloc` placement and small `malloc` placement.
- Create `cxlalloc-preload/tests/run_valloc_probe.sh`: compile and run the probe against the built preload library.
- Create `docs/superpowers/evidence/2026-08-20-lat-mem-rd-devdax.md`: exact live commands, mapping proof, output, kernel-log delta, and binary hash.

---

### Task 1: Real libc dispatch and pure placement policy

**Files:**
- Create: `cxlalloc-preload/src/real.rs`
- Modify: `cxlalloc-preload/src/lib.rs:1-498`

**Interfaces:**
- Produces: `real::RealAlloc::resolve() -> Result<&'static RealAlloc, &'static str>`
- Produces: `PlacementPolicy::from_env() -> PlacementPolicy`
- Produces: `PlacementPolicy::uses_cxl(size: usize) -> bool`
- Consumes: the existing early bump allocator during recursive symbol resolution.

- [ ] **Step 1: Add failing policy tests**

Add a private policy type and tests in `cxlalloc-preload/src/lib.rs` that express the required default without implementing it:

```rust
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
```

- [ ] **Step 2: Run the focused tests and witness RED**

Run:

```bash
cargo test -p cxlalloc-preload default_policy -- --nocapture
```

Expected: compilation fails because `PlacementPolicy` does not exist.

- [ ] **Step 3: Implement the minimal placement type**

Implement:

```rust
struct PlacementPolicy {
    min_size: usize,
}

impl PlacementPolicy {
    const DEFAULT_MIN_SIZE: usize = 2 << 20;

    fn new(min_size: usize) -> Self { Self { min_size } }

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
```

- [ ] **Step 4: Run the focused tests and witness GREEN**

Run:

```bash
cargo test -p cxlalloc-preload default_policy -- --nocapture
```

Expected: both policy tests pass.

- [ ] **Step 5: Add failing real-libc resolution tests**

Create `real.rs` with the function-pointer type declarations and tests that resolve libc, allocate 64 bytes, verify non-null, and free it through the resolved functions:

```rust
#[test]
fn resolves_malloc_and_free_from_next_object() {
    let real = RealAlloc::resolve().expect("resolve libc allocator");
    let pointer = unsafe { (real.malloc)(64) };
    assert!(!pointer.is_null());
    unsafe { (real.free)(pointer) };
}
```

- [ ] **Step 6: Run the resolver test and witness RED**

Run:

```bash
cargo test -p cxlalloc-preload resolves_malloc_and_free_from_next_object -- --nocapture
```

Expected: compilation fails because `RealAlloc::resolve` is absent.

- [ ] **Step 7: Implement real libc resolution**

Resolve `malloc`, `calloc`, `realloc`, `free`, `memalign`, `valloc`, `pvalloc`, and `malloc_usable_size` with `libc::dlsym(libc::RTLD_NEXT, ...)`. Store typed pointers in a `OnceLock<RealAlloc>`. Return a static error string if any required symbol is null. Do not call formatting, logging, or heap-allocating error paths while resolution is in progress.

The public shape is:

```rust
pub(crate) struct RealAlloc {
    pub malloc: unsafe extern "C" fn(usize) -> *mut c_void,
    pub calloc: unsafe extern "C" fn(usize, usize) -> *mut c_void,
    pub realloc: unsafe extern "C" fn(*mut c_void, usize) -> *mut c_void,
    pub free: unsafe extern "C" fn(*mut c_void),
    pub memalign: unsafe extern "C" fn(usize, usize) -> *mut c_void,
    pub valloc: unsafe extern "C" fn(usize) -> *mut c_void,
    pub pvalloc: unsafe extern "C" fn(usize) -> *mut c_void,
    pub malloc_usable_size: unsafe extern "C" fn(*mut c_void) -> usize,
}
```

- [ ] **Step 8: Run the resolver and package tests**

Run:

```bash
cargo test -p cxlalloc-preload -- --nocapture
```

Expected: all tests pass without recursion, panic, or allocation warnings.

- [ ] **Step 9: Commit Task 1**

```bash
git add cxlalloc-preload/src/lib.rs cxlalloc-preload/src/real.rs
git commit -m "preload: add libc routing policy"
```

---

### Task 2: DRAM-managed devdax arena

**Files:**
- Create: `cxlalloc-preload/src/dax_arena.rs`
- Modify: `cxlalloc-preload/src/lib.rs`

**Interfaces:**
- Consumes: `PlacementPolicy` from Task 1.
- Produces: `DaxArena::open(path: &CStr) -> Result<DaxArena, ArenaError>`
- Produces: `DaxArena::allocate(size: usize, alignment: usize) -> Result<NonNull<c_void>, ArenaError>`
- Produces: `DaxArena::deallocate(pointer: NonNull<c_void>) -> Result<(), ArenaError>`
- Produces: `DaxArena::allocation_size(pointer: NonNull<c_void>) -> Option<usize>`
- Produces: `DaxArena::owns(pointer: *const c_void) -> bool`

- [ ] **Step 1: Write failing extent-allocation tests**

Define a test-only arena over a 16 MiB anonymous mapping with 2 MiB alignment. Tests must cover alignment, non-overlap, free/reuse, and capacity failure:

```rust
#[test]
fn allocations_are_aligned_and_reused_after_free() {
    let arena = DaxArena::anonymous_for_test(16 << 20, 2 << 20).unwrap();
    let first = arena.allocate(3 << 20, 4096).unwrap();
    assert_eq!((first.as_ptr() as usize) % (2 << 20), 0);
    let second = arena.allocate(2 << 20, 4096).unwrap();
    assert_ne!(first, second);
    arena.deallocate(first).unwrap();
    let reused = arena.allocate(3 << 20, 4096).unwrap();
    assert_eq!(first, reused);
}

#[test]
fn capacity_exhaustion_does_not_fall_back() {
    let arena = DaxArena::anonymous_for_test(4 << 20, 2 << 20).unwrap();
    assert!(arena.allocate(4 << 20, 4096).is_ok());
    assert_eq!(arena.allocate(1, 1), Err(ArenaError::OutOfMemory));
}
```

- [ ] **Step 2: Run the arena tests and witness RED**

Run:

```bash
cargo test -p cxlalloc-preload dax_arena::tests -- --nocapture
```

Expected: compilation fails because `DaxArena` is not implemented.

- [ ] **Step 3: Implement a fixed-capacity extent table in shared DRAM**

Create an anonymous `MAP_SHARED` control page containing a process-shared robust `pthread_mutex_t` and a fixed array of 128 extent records. Initialize the mutex with `pthread_mutexattr_setpshared(..., PTHREAD_PROCESS_SHARED)` and `pthread_mutexattr_setrobust(..., PTHREAD_MUTEX_ROBUST)`. Recover `EOWNERDEAD` with `pthread_mutex_consistent`.

Use first-fit allocation over sorted extents. Round allocation size up to the device alignment and enforce `max(requested_alignment, device_alignment)`. A record stores `offset`, rounded length, requested length, and in-use state. `deallocate` clears the record and coalesces adjacent free extents while holding the mutex.

- [ ] **Step 4: Run extent tests and witness GREEN**

Run:

```bash
cargo test -p cxlalloc-preload dax_arena::tests -- --nocapture
```

Expected: all extent tests pass.

- [ ] **Step 5: Add a failing fork-sharing test**

Fork after arena creation. Allocate one 4 MiB extent in the child, communicate its offset through a pipe, hold it until the parent attempts another allocation, and assert that the parent receives a different non-overlapping offset. Then release and reap the child.

```rust
#[test]
fn allocation_state_is_shared_across_fork() {
    let arena = DaxArena::anonymous_for_test(16 << 20, 2 << 20).unwrap();
    let (read_fd, write_fd) = pipe_for_test();
    let child = unsafe { libc::fork() };
    if child == 0 {
        let allocation = arena.allocate(4 << 20, 4096).unwrap();
        write_offset(write_fd, arena.offset_of(allocation));
        wait_for_parent_release(read_fd);
        unsafe { libc::_exit(0) };
    }
    let child_offset = read_offset(read_fd);
    let parent = arena.allocate(4 << 20, 4096).unwrap();
    assert_ne!(child_offset, arena.offset_of(parent));
    release_child(write_fd);
    assert_child_exited_successfully(child);
}
```

- [ ] **Step 6: Run the fork test and witness RED, then GREEN**

First run before shared-state wiring:

```bash
cargo test -p cxlalloc-preload allocation_state_is_shared_across_fork -- --nocapture
```

Expected RED: parent and child receive the same offset. Wire all mutable extent state through the shared control mapping, rerun, and expect PASS.

- [ ] **Step 7: Add failing devdax discovery and lock tests**

Factor sysfs parsing into pure helpers and test decimal size/alignment parsing, invalid input, and path-to-device-name conversion. Add a temporary-file test proving that a second independent file description cannot acquire the arena's exclusive nonblocking `flock` while the first is alive.

- [ ] **Step 8: Implement live device open and mapping**

`DaxArena::open` must:

1. Open the configured device read/write.
2. Acquire `LOCK_EX | LOCK_NB` for the arena lifetime.
3. Read `/sys/bus/dax/devices/<name>/size` and `align`.
4. Reject zero, malformed, or non-page-aligned values.
5. Map exactly the reported capacity with `PROT_READ | PROT_WRITE` and `MAP_SHARED`.
6. Create the DRAM control mapping.
7. Return an error without a DRAM fallback on any failure.

- [ ] **Step 9: Run all arena tests**

Run:

```bash
cargo test -p cxlalloc-preload dax_arena::tests -- --nocapture
```

Expected: extent, ownership, fork-sharing, parsing, and lock tests all pass.

- [ ] **Step 10: Commit Task 2**

```bash
git add cxlalloc-preload/src/dax_arena.rs cxlalloc-preload/src/lib.rs
git commit -m "preload: add DRAM-managed devdax arena"
```

---

### Task 3: Ownership-aware C allocation interposition

**Files:**
- Modify: `cxlalloc-preload/src/lib.rs`
- Test: `cxlalloc-preload/src/lib.rs`

**Interfaces:**
- Consumes: `RealAlloc`, `PlacementPolicy`, and `DaxArena`.
- Produces: C ABI symbols for `malloc`, `calloc`, `realloc`, `free`, `memalign`, `posix_memalign`, `aligned_alloc`, `valloc`, `pvalloc`, and `malloc_usable_size`.

- [ ] **Step 1: Add failing routing and ownership tests**

Use an anonymous test arena and a fake `RealAlloc` recorder to verify:

```rust
#[test]
fn routing_small_malloc_uses_libc_and_large_malloc_uses_arena() {
    let state = TestState::new(256 << 20, 2 << 20);
    let small = unsafe { routed_malloc(4096, state.state()) };
    let large = unsafe { routed_malloc(128 << 20, state.state()) };
    assert_eq!(state.origin(small), Origin::Libc);
    assert_eq!(state.origin(large), Origin::Arena);
}

#[test]
fn routing_free_dispatches_by_pointer_ownership() {
    let state = TestState::new(256 << 20, 2 << 20);
    let small = unsafe { routed_malloc(4096, state.state()) };
    let large = unsafe { routed_malloc(128 << 20, state.state()) };
    unsafe { routed_free(small, state.state()) };
    unsafe { routed_free(large, state.state()) };
    assert_eq!(state.libc_free_count(), 1);
    assert!(!state.arena_owns(large));
}

#[test]
fn routing_valloc_is_page_aligned_and_cxl_owned() {
    let state = TestState::new(256 << 20, 2 << 20);
    let pointer = unsafe { routed_valloc(128 << 20, state.state()) };
    assert_eq!((pointer as usize) % page_size(), 0);
    assert!(state.arena_owns(pointer));
}

#[test]
fn routing_arena_oom_returns_null_without_libc_fallback() {
    let state = TestState::new(4 << 20, 2 << 20);
    let pointer = unsafe { routed_malloc(8 << 20, state.state()) };
    assert!(pointer.is_null());
    assert_eq!(errno_for_test(), libc::ENOMEM);
    assert_eq!(state.libc_malloc_count(), 0);
}
```

Provide `TestState::new(arena_size, min_size)` as a test-only fixture backed by
the anonymous arena and recorder functions. It exposes `state()`, `origin`,
`arena_owns`, and call-count accessors shown above; none of these helpers enter
the production ABI.

Also add backend-mode parsing tests:

```rust
#[test]
fn backend_mode_defaults_to_pure_dax() {
    assert_eq!(BackendMode::parse(None).unwrap(), BackendMode::Dax);
}

#[test]
fn backend_mode_preserves_explicit_controls() {
    assert_eq!(BackendMode::parse(Some("mmap")).unwrap(), BackendMode::Mmap);
    assert_eq!(
        BackendMode::parse(Some("dax-mmap")).unwrap(),
        BackendMode::LegacyDaxMmap,
    );
    assert!(BackendMode::parse(Some("unknown")).is_err());
}
```

- [ ] **Step 2: Run focused routing tests and witness RED**

Run:

```bash
cargo test -p cxlalloc-preload routing -- --nocapture
```

Expected: tests fail because symbols still route every initialized allocation through `RAW` and `valloc` is absent.

- [ ] **Step 3: Replace unconditional `RAW` routing with origin-aware dispatch**

Introduce internal functions that accept resolved dependencies and are directly testable:

```rust
unsafe fn routed_malloc(size: usize, state: &State) -> *mut c_void;
unsafe fn routed_calloc(count: usize, size: usize, state: &State) -> *mut c_void;
unsafe fn routed_realloc(pointer: *mut c_void, size: usize, state: &State) -> *mut c_void;
unsafe fn routed_free(pointer: *mut c_void, state: &State);
unsafe fn routed_aligned_alloc(alignment: usize, size: usize, state: &State) -> *mut c_void;
```

`State` contains `RealAlloc`, `PlacementPolicy`, and a `BackendState` selected
by `CXLALLOC_BACKEND`. `dax` is the default and owns a `DaxArena`; `mmap` owns
an anonymous arena for the DRAM control; explicit `dax-mmap` retains the
existing `RAW`-based legacy behavior but is excluded from CXL-only acceptance.
Unknown modes are initialization errors, not implicit DRAM fallback. The
constructor publishes `State` through `OnceLock` only after its components
initialize successfully.

- [ ] **Step 4: Implement standards-correct alignment APIs and `valloc`**

- `posix_memalign` validates power-of-two and pointer-size multiples and returns an error code without modifying `*memptr` on failure.
- `aligned_alloc` rejects sizes not divisible by alignment.
- `valloc` uses the live page size as alignment.
- `pvalloc` rounds size up to a page, detects overflow, and uses page alignment.
- CXL-selected requests use the arena; smaller requests call the matching real libc function.

- [ ] **Step 5: Implement ownership-aware free, realloc, and usable size**

- `free(NULL)` is a no-op.
- Early-buffer pointers remain intentionally leaked.
- Arena pointers deallocate through the arena; all other pointers call real libc `free`.
- Arena `realloc` allocates a new routed block, copies `min(old_size, new_size)` with a scalar byte loop using volatile reads/writes when either endpoint is CXL-owned, and frees the old block only after success.
- `malloc_usable_size` returns the requested arena extent size for CXL pointers and delegates all other pointers to libc.

- [ ] **Step 6: Run routing tests and witness GREEN**

Run:

```bash
cargo test -p cxlalloc-preload routing -- --nocapture
```

Expected: all routing, ownership, OOM, and alignment tests pass.

- [ ] **Step 7: Run the complete Rust test suite**

Run:

```bash
cargo test -p cxlalloc-preload -- --nocapture
cargo test -p cxlalloc --lib
```

Expected: both packages pass with no panic or test hang.

- [ ] **Step 8: Build debug and release preload libraries**

Run:

```bash
cargo build -p cxlalloc-preload
cargo build --release -p cxlalloc-preload
```

Expected: both libraries build successfully.

- [ ] **Step 9: Commit Task 3**

```bash
git add cxlalloc-preload/src/lib.rs
git commit -m "preload: route large allocations to devdax"
```

---

### Task 4: Black-box preload placement probe

**Files:**
- Create: `cxlalloc-preload/tests/valloc_probe.c`
- Create: `cxlalloc-preload/tests/run_valloc_probe.sh`

**Interfaces:**
- Consumes: `target/debug/libcxlalloc_preload.so` from Task 3.
- Produces: an executable placement check that exits nonzero unless the large pointer is devdax-backed and the small pointer is not.

- [ ] **Step 1: Write the C probe before wiring its runner**

The probe must allocate 128 MiB with `valloc`, allocate 4096 bytes with `malloc`, parse `/proc/self/maps`, and print:

```text
large=<address> backing=/dev/dax0.0
small=<address> backing=anonymous
```

It exits nonzero if the large address is not within a `/dev/dax0.0` mapping, if the small address is within that mapping, or if either allocation fails. It touches the first and last 64-bit word with scalar volatile accesses before freeing both pointers.

- [ ] **Step 2: Add the runner and prove the probe detects non-CXL placement**

`run_valloc_probe.sh` compiles to a `mktemp -d` directory and runs:

```bash
LD_PRELOAD="$repo/target/debug/libcxlalloc_preload.so" \
    "$tmpdir/valloc_probe"
```

First run the compiled probe without `LD_PRELOAD`. The expected failure is
`large backing is not /dev/dax0.0`. This negative control proves that the map
check detects ordinary DRAM placement rather than passing vacuously.

- [ ] **Step 3: Run the probe against the new library**

Run:

```bash
bash cxlalloc-preload/tests/run_valloc_probe.sh
```

Expected: exit 0 with the large and small backing lines shown above.

- [ ] **Step 4: Confirm capacity fails closed**

Run a probe variant requesting 256 MiB:

```bash
CXLALLOC_PROBE_SIZE=268435456 \
    bash cxlalloc-preload/tests/run_valloc_probe.sh
```

Expected: nonzero exit, `valloc` returns null with `ENOMEM`, and no anonymous 256 MiB fallback mapping is reported.

- [ ] **Step 5: Commit Task 4**

```bash
git add cxlalloc-preload/tests/valloc_probe.c cxlalloc-preload/tests/run_valloc_probe.sh
git commit -m "preload: test devdax valloc placement"
```

---

### Task 5: Live lmbench acceptance and evidence

**Files:**
- Create: `docs/superpowers/evidence/2026-08-20-lat-mem-rd-devdax.md`

**Interfaces:**
- Consumes: the debug preload library and black-box probe.
- Produces: reproducible evidence for genuine CXL placement, benchmark completion, and absence of new traps.

- [ ] **Step 1: Record immutable pre-run facts**

Capture:

```bash
sha256sum ~/lmbench/bin/x86_64-linux-gnu/lat_mem_rd
daxctl list -D -R -M -u
stat -c '%t:%T %s %n' /dev/dax0.0
```

Expected: devdax mode, 250 MiB device, and a benchmark hash retained for the post-run comparison.

- [ ] **Step 2: Establish the kernel-log boundary**

Record UTC time and the current last `lat_mem_rd` trap line before the run:

```bash
date -u +%Y-%m-%dT%H:%M:%SZ
dmesg --color=never | grep -E 'traps: lat_mem_rd|lat_mem_rd.*(segfault|abort)' | tail -n 1
```

- [ ] **Step 3: Run the live placement probe**

Run:

```bash
bash cxlalloc-preload/tests/run_valloc_probe.sh
```

Expected: the 128 MiB pointer is mapped to `/dev/dax0.0`, the 4 KiB pointer is anonymous, and scalar first/last-word access succeeds.

- [ ] **Step 4: Run the full CXL-only acceptance benchmark**

Run:

```bash
LD_PRELOAD=target/debug/libcxlalloc_preload.so \
    ~/lmbench/bin/x86_64-linux-gnu/lat_mem_rd -t -N 4 128 64
```

Expected: output advances through `128.00000`, every latency is greater than zero, and the process returns 0. Do not accept lmbench parent status alone; retain all output and validate the final point.

- [ ] **Step 5: Verify no new kernel failure and unchanged lmbench binary**

Run:

```bash
dmesg --color=never | grep -E 'traps: lat_mem_rd|lat_mem_rd.*(segfault|abort)' | tail -n 20
sha256sum ~/lmbench/bin/x86_64-linux-gnu/lat_mem_rd
```

Expected: no new matching kernel line after the recorded boundary and the hash matches Step 1.

- [ ] **Step 6: Run the labeled DRAM control**

Run:

```bash
CXLALLOC_BACKEND=mmap \
LD_PRELOAD=target/debug/libcxlalloc_preload.so \
    ~/lmbench/bin/x86_64-linux-gnu/lat_mem_rd -t -N 4 128 64
```

Expected: full nonzero output. Label this as DRAM; do not use it as CXL proof.

- [ ] **Step 7: Write the evidence document**

Record the exact commands, device facts, mapping lines, benchmark outputs, return codes, pre/post kernel-log boundary, hash comparison, and any measured capacity limit. Clearly separate unit/build evidence from live devdax proof.

- [ ] **Step 8: Run final formatting and regression checks**

Run:

```bash
cargo fmt --all -- --check
cargo test -p cxlalloc-preload -- --nocapture
cargo build --release -p cxlalloc-preload
git diff --check
```

Expected: all commands pass.

- [ ] **Step 9: Commit Task 5**

```bash
git add docs/superpowers/evidence/2026-08-20-lat-mem-rd-devdax.md
git commit -m "preload: record live devdax lmbench proof"
```
