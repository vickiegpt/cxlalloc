# `lat_mem_rd` 128 MiB devdax acceptance

## Command

Run from `/root/cxlalloc-devdax-selective` with no backend override:

```sh
LD_PRELOAD=target/debug/libcxlalloc_preload.so \
    /root/lmbench/bin/x86_64-linux-gnu/lat_mem_rd -t -N 4 128 64
```

The preload defaults to `dax-mmap`. Its selective policy places lmbench's
128 MiB `valloc` allocation on `/dev/dax0.0` and keeps allocations below the
2 MiB threshold in the real libc allocator.

## Result

- Start: `2026-08-20T20:42:34Z`
- End: `2026-08-20T21:36:37Z`
- Exit status: `0`
- Numeric rows: `115`
- Final row: `128.00000 4130.096`
- All reported latencies: nonzero
- Boot ID unchanged during run: `eb1a2c96-a949-4998-940b-ed2b82dec7bc`
- New kernel-log lines during run: `0`
- Invalid-opcode/trap check: pass

`maps.snapshot` records the live shared mapping:

```text
703f1a800000-703f2a200000 rw-s 00000000 00:06 408 /dev/dax0.0
```

`daxctl.json` records that `dax0.0` remained in `devdax` mode with a
262,144,000-byte capacity and 2 MiB alignment.

## Files

- `output.log`: complete lmbench output.
- `rc`: process exit status.
- `maps.snapshot`: live process command line and DAX mapping.
- `daxctl.json`: device mode, capacity, NUMA node, and alignment.
- `dmesg.before.lines` and `dmesg.after.log`: kernel-log boundary.
- `boot_id`: boot continuity proof.
- `binary.sha256`: lmbench binary identity.
- `placement-probe.log`: negative control plus large-DAX/small-DRAM proof.
- `preload-tests.log` and `core-tests.log`: fresh unit-test runs.
- `debug-build.log` and `release-build.log`: fresh build runs.
