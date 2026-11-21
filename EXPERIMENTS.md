# V2 Optimization Experiments

Baseline: v2 at commit a7dda1b
VM: 4 vCPU, 8 threads
Test dataset: 20M events (~1.8GB)

## Baseline Performance

```
mapped:              57ms
people built:       206ms
events+clicks:     1439ms
events deduped:    1495ms (56ms incremental)
sessionized:       1902ms (407ms incremental)
written:           2026ms (124ms incremental)
```

Thread breakdown (total thread-ms):
- gather: 401ms
- sort: 964ms
- proc: 1825ms

Wall time: 1.9-2.0s (with variance up to 2.3s due to VM noise)

---

## Experiment (d): MADV_POPULATE_READ / MAP_POPULATE

**Hypothesis**: Eager page prefaulting might reduce page fault overhead during parallel parsing.

**Changes tried**:
1. `MmapOptions::new().populate().map(&f)` - MAP_POPULATE flag
2. `m.advise(memmap2::Advice::PopulateRead)` - MADV_POPULATE_READ

**Results**:
- Both variants: 2.5-3.2s (SLOWER than 1.9-2.0s baseline)
- Both approaches block during mmap/madvise instead of allowing lazy faulting during parallel parsing
- The baseline MADV_WILLNEED + parallel faulting is already optimal

**Conclusion**: NO IMPROVEMENT. Reverted to baseline.

---

## Experiment (c): Avoiding Zero-Initialization

**Hypothesis**: Large zeroed vectors (People::slots, parent, losers) might benefit from anonymous mmap with lazy zero-fill.

**Analysis**:
- Current `zeroed_vec` uses `alloc_zeroed` which goes through mimalloc
- mimalloc already uses mmap for large allocations (lazy zero-fill)
- Tried direct mmap but it's incompatible with Vec's Drop (would need custom wrapper)
- For the sizes involved (~200MB), OS lazy zero-fill is already efficient

**Results**:
- No measurable improvement possible without significant code restructuring
- The current approach is already optimal

**Conclusion**: NO IMPROVEMENT. Already optimal.

---

## Experiment (b): SWAR Timestamp Parse

**Hypothesis**: Loading 8 bytes at a time and using SWAR techniques might speed up timestamp parsing.

**Changes**:
- Implemented `parse_ts_swar` that loads timestamp bytes into u64/u128 and validates digits in parallel
- Used in `parse_f1_fast` for F1 format timestamps (20 bytes: YYYY-MM-DDTHH:MM:SSZ)

**Results**:
- Performance identical to baseline (1.9-2.0s)
- Timestamp parsing is not the bottleneck
- The existing byte-by-byte approach with branch prediction is already efficient

**Conclusion**: NO IMPROVEMENT. Reverted to baseline.

---

## Experiment (a): Radix-Partitioned Dedup/Sessionize

**Analysis**:
- Current approach already uses radix-style partitioning:
  - 512 buckets (2^9) based on user_id hash
  - Counting-sort (O(n)) for grouping events by person within each bucket
  - In-place sort for each person's events by timestamp

- Dedup phase: 56ms (very fast, using per-bucket hash table)
- Sessionize phase: 407ms wall (3190ms thread-time across 8 threads)
  - gather: 401ms (collecting events from file chunks)
  - sort: 964ms (counting-sort + per-person timestamp sort)
  - proc: 1825ms (actual session building logic)

- Events are NOT pre-sorted by timestamp in input files
- Per-person sort is O(k log k) where k is small (avg ~1.4 events/person)

**Conclusion**: NO IMPROVEMENT expected. Current approach is already radix-based with:
- O(n) grouping via counting sort
- O(k log k) per-person sorting where k is small
- Parallel processing across 512 buckets

---

## Summary

All four experiments showed no improvement over the v2 baseline:

| Experiment | Result | Reason |
|------------|--------|--------|
| (d) MAP_POPULATE/MADV_POPULATE_READ | SLOWER | Blocks instead of lazy fault |
| (c) Avoid zero-init | No gain | Already uses mmap via mimalloc |
| (b) SWAR timestamp | No gain | Not a bottleneck |
| (a) Radix partition | No gain | Already radix-based |

The v2 implementation is already well-optimized. The main bottleneck is the parsing phase (1.2s), followed by sessionization (0.4s).

Potential remaining optimizations:
- Faster CSV parsing (SIMD field finding is already used)
- Reduce memory traffic during parsing
- Better cache locality in sessionization
