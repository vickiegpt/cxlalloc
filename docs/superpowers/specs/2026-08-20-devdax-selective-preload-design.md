# Selective devdax Placement for `cxlalloc-preload`

Date: 2026-08-20

## Problem

The target workload is the unmodified lmbench command:

```bash
LD_PRELOAD=target/debug/libcxlalloc_preload.so \
    ~/lmbench/bin/x86_64-linux-gnu/lat_mem_rd -t -N 4 128 64
```

The measured buffer must be backed entirely by `/dev/dax0.0`, which remains in
`devdax` mode. Anonymous `mmap` is a DRAM baseline only. The original 1024 MiB
range is outside the 250 MiB device capacity, so the initial acceptance range is
128 MiB.

The current preload library has two relevant problems:

1. It sends ordinary process allocations through the CXL allocator. Library or
   runtime code can then execute vector memory operations against KVM MMIO,
   producing an invalid-opcode trap.
2. lmbench obtains the measured buffer with `valloc`, but the preload library
   does not currently interpose `valloc`. Consequently, successful execution
   alone does not prove the measured buffer is CXL-backed.

The disassembled `lat_mem_rd` pointer-chase loop uses scalar 64-bit loads. Its
working buffer can therefore use devdax while unrelated allocations remain in
DRAM.

## Goals

- Keep `/dev/dax0.0` in `devdax` mode.
- Place the complete 128 MiB lmbench working buffer on `/dev/dax0.0`.
- Keep allocator bookkeeping and small process allocations in DRAM.
- Preserve the required alignment and ownership behavior of the intercepted C
  allocation APIs.
- Fail closed when a requested CXL allocation exceeds available devdax space;
  never silently spill a CXL-selected allocation into DRAM.
- Run the unmodified lmbench binary without new invalid-opcode traps or false
  `0.000` results.

## Non-goals

- Emulating arbitrary SSE, AVX, or AVX-512 instructions in a signal handler.
- Converting the DAX device to system RAM.
- Claiming a 1 GiB CXL-only result from a 250 MiB device.
- Modifying or rebuilding lmbench to change its memory instructions.
- Providing persistent allocation recovery across independent preload runs.

## Design

### Placement policy

`cxlalloc-preload` will resolve and retain the real libc allocation functions.
Small and incidental allocations will use libc and remain in DRAM. CXL-selected
large allocations will use a dedicated devdax arena owned by the preload
library.

The routing threshold will be configurable with `CXLALLOC_MIN_SIZE` and will
default to 2 MiB, matching the live device alignment. `valloc` and `pvalloc`
will be interposed and follow the same size policy. Thus lmbench's 128 MiB
`valloc` request takes the CXL path without requiring an application change.

The default and acceptance backend remains `dax-mmap`. In selective preload
mode, this name describes allocation-level placement: large CXL-selected
allocations use the devdax arena, while small incidental allocations use
libc-backed DRAM. A selected allocation is never striped across or spilled into
DRAM, so lmbench's complete measured buffer remains CXL-only.

### devdax arena

The preload constructor will open `/dev/dax0.0`, read its size and alignment
from sysfs, and map the device once. Arena bookkeeping will live in anonymous
DRAM, not inside the MMIO mapping. Allocation will reserve aligned contiguous
extents from the mapped device; freeing will return those extents to the arena.

Bookkeeping shared by lmbench's forked workers will be created with a shared
anonymous mapping before the workers fork. Independent preload invocations will
take an exclusive advisory lock on the DAX device so two unrelated processes
cannot unknowingly allocate overlapping offsets.

The arena will reserve only actual device capacity. Capacity exhaustion,
alignment failure, or device-open failure returns an allocation failure and a
clear diagnostic; it must not fall back to DRAM.

### Interposed API behavior

The preload library will provide correct routing for `malloc`, `calloc`,
`realloc`, `free`, `memalign`, `aligned_alloc`, `posix_memalign`, `valloc`,
`pvalloc`, and `malloc_usable_size`.

Pointer ownership will be determined from the mapped devdax address range and
the DRAM-resident extent records:

- CXL-owned pointers are handled by the devdax arena.
- Other pointers are forwarded to the real libc function.
- CXL `realloc` uses a new extent plus an MMIO-safe scalar copy.
- CXL `calloc` uses MMIO-safe scalar initialization.
- Alignment APIs return pointers satisfying their documented alignment.

Re-entrant allocations during symbol resolution or constructor startup continue
to use the existing early allocator until libc dispatch and the DAX arena are
ready.

### MMIO instruction boundary

Only the application's explicitly selected working buffer is exposed as
devdax. The allocator's ownership table, locks, free-space state, libc objects,
and Rust runtime allocations stay in DRAM. The devdax path itself uses scalar
volatile accesses or the already validated simple-string operations; it does
not depend on vectorized libc copies.

The design does not install a `SIGILL` handler. An unexpected signal remains a
hard failure so unsupported accesses are visible rather than producing
fabricated benchmark results.

## Error handling

- Device size and alignment are validated before the arena becomes available.
- A CXL-selected allocation that cannot fit returns `NULL` with `errno=ENOMEM`.
- Invalid alignment returns the standard API error.
- `free` rejects neither valid libc pointers nor valid arena pointers; unknown
  pointers continue to libc's normal handling.
- No allocation selected for CXL silently changes to DRAM.
- Diagnostic messages identify the requested size, remaining capacity, device,
  and backend without printing sensitive data.

## Verification

Implementation follows test-driven development.

1. Unit tests first cover threshold routing, page alignment, extent reuse,
   capacity exhaustion, ownership dispatch, and fork-shared arena state.
2. A small unmodified C probe calls `valloc(128 MiB)`. Its returned address must
   fall inside a `/proc/self/maps` entry for `/dev/dax0.0`; a neighboring small
   `malloc` must not.
3. Run the exact 128 MiB lmbench acceptance command and require nonzero output
   through the final 128 MiB point.
4. Compare kernel logs immediately before and after the run and require no new
   `lat_mem_rd` invalid-opcode, segmentation-fault, or allocator-abort messages.
5. Run the same command with `CXLALLOC_BACKEND=mmap` only as a clearly labeled
   DRAM control.
6. Confirm the lmbench executable hash is unchanged.

Passing build or unit tests is not sufficient; completion requires the live
devdax mapping and full lmbench run evidence.
