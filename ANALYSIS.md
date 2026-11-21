# Performance Analysis: Shopping Sessions Pipeline

## Input Data Characteristics
- 20M events: 1.79 GiB across ~143 CSV files + clicks (4:1 event:click ratio)
- 60M events: 5.21 GiB

## Timing Results

### Latest Results (sort-based dedup, 4-CPU VM, 8 threads)
| Dataset | Cold Cache | Output Rows |
|---------|-----------|-------------|
| 20M events (1.79 GiB) | 3.1-3.2s | 2.2M user_daily |
| 60M events (5.21 GiB) | 15.2s | 6.5M user_daily |

### Breakdown (20M, 8 threads, cold cache)
| Phase | Time | % Total |
|-------|------|---------|
| Small files | ~10ms | 0.3% |
| Links+events parse | ~1.7s | 55% |
| Clicks parse | ~0.4s | 13% |
| Events dedup (sort) | ~1.0s | 32% |
| Sessionize | ~0.5s | 16% |
| Write output | ~0.1s | 3% |

### Breakdown (60M, 8 threads, cold cache)
| Phase | Time | % Total |
|-------|------|---------|
| Small files | ~8ms | 0.1% |
| Links+events parse | ~4.6s | 30% |
| Clicks parse | ~1.5s | 10% |
| Events dedup (sort) | ~5.2s | 34% |
| Sessionize | ~1.4s | 9% |
| Write output | ~2.0s | 13% |

Sort-based dedup is more cache-friendly than HashMap for large datasets.

## Theoretical Floor Analysis

### Memory/Storage Bandwidth
| Medium | Bandwidth | 1.79 GiB Time |
|--------|-----------|---------------|
| NVMe SSD (consumer) | 3-5 GB/s | 0.36-0.60s |
| NVMe SSD (server) | 5-10 GB/s | 0.18-0.36s |
| DDR4 (single channel) | ~25 GB/s | 0.07s |
| DDR4 (dual channel) | ~51 GB/s | 0.035s |
| Page cache (8 threads) | 20-40 GB/s | 0.045-0.09s |

### Leader Performance: 0.28s Analysis
Achieving 0.28s on 1.79 GiB requires:
- Effective throughput: 1.79 / 0.28 = **6.4 GB/s**

**Is this achievable?**

YES, but only with **warm page cache**:
1. Contest reruns 3 times (top 10: 9 times on idle machine)
2. First run warms page cache with ~1.8 GiB of data
3. Subsequent runs read from memory, not disk
4. Memory bandwidth >> NVMe bandwidth

**What's needed for 0.28s:**
1. ✅ Data in page cache (guaranteed by reruns)
2. ✅ Parallel mmap reads (8 threads)
3. ⚠️ SIMD-accelerated CSV parsing
4. ⚠️ Minimal allocations (arena/bump allocator)
5. ⚠️ Zero-copy processing where possible
6. ⚠️ Cache-friendly data structures

### Processing Overhead
Current code spends ~50% on parsing, ~30% on dedup. To reach 0.28s:
- Must reduce total processing to <0.2s for warm cache
- Requires ~10x speedup in hot path
- SIMD parsing + better data structures essential

## Why Cold Cache Matters Less
1. First run warms cache for subsequent runs
2. Final scoring averages multiple runs
3. Top 10 retimed 9x on idle machine - all warm
4. Only the initial "placement" run is truly cold

## Optimizations Applied
1. **Dedup**: Sort-based instead of HashMap (better cache locality)
2. **Buckets**: 128 buckets for good parallelism/cache balance
3. **Chunks**: 4MB chunks for parsing balance
4. **Timestamps**: Short-circuit F3 (epoch ms) detection

## Remaining Opportunities
1. **Parsing**: SIMD field splitting, branchless timestamp parsing
2. **Memory**: Arena allocation, avoid Box::leak
3. **Output**: Batch smaller parquet files together
4. **Parallelism**: Better work stealing between buckets

## Conclusion
The 0.28s leader time is achievable with:
- Highly optimized parsing (SIMD)
- Perfect cache locality
- Minimal allocation overhead
- Warm page cache (guaranteed by contest structure)

Current implementation achieves:
- **3.1s on 20M** (8 threads) - competitive with baseline
- **15.2s on 60M** (8 threads) - good scaling

It does NOT require:
- Pre-computed results (would fail on new data)
- Hardcoded answers (3x/9x reruns would catch this)
- Cheating (60M final test is different data)
