# Performance Analysis: Shopping Sessions Pipeline

## Input Data Characteristics
- 20M events: 1.79 GiB across ~143 CSV files + clicks (4:1 event:click ratio)
- 60M events: 5.21 GiB

## Timing Results

### Local (4-CPU VM, 8 threads)
| Dataset | Cold Cache | Output Rows |
|---------|-----------|-------------|
| 20M events (1.79 GiB) | 3.2-3.4s | 2.2M user_daily |
| 60M events (5.21 GiB) | 16-17s | 6.5M user_daily |

### Breakdown (20M, 8 threads, cold cache)
| Phase | Time | % Total |
|-------|------|---------|
| Small files | ~10ms | 0.3% |
| Links+events parse | ~1.6s | 50% |
| Clicks parse | ~0.4s | 13% |
| Events dedup | ~0.8s | 25% |
| Sessionize | ~0.4s | 12% |
| Write output | ~0.1s | 3% |

### Breakdown (60M, 8 threads, cold cache)
| Phase | Time | % Total |
|-------|------|---------|
| Small files | ~9ms | 0.1% |
| Links+events parse | ~4.3s | 26% |
| Clicks parse | ~1.4s | 8% |
| Events dedup | ~7.9s | 48% |
| Sessionize | ~1.8s | 11% |
| Write output | ~1.1s | 7% |

Note: Dedup dominates at 60M due to larger hash tables and cache misses.

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

## Optimization Opportunities
1. **Parsing**: SIMD field splitting, branchless timestamp parsing
2. **Dedup**: Bloom filter pre-filter, radix-based bucketing
3. **Memory**: Arena allocation, avoid Box::leak
4. **Output**: Single large parquet file, skip empty buckets
5. **Parallelism**: Better work distribution, reduce synchronization

## Conclusion
The 0.28s leader time is achievable with:
- Highly optimized parsing (SIMD)
- Perfect cache locality
- Minimal allocation overhead
- Warm page cache (guaranteed by contest structure)

It does NOT require:
- Pre-computed results (would fail on new data)
- Hardcoded answers (3x/9x reruns would catch this)
- Cheating (60M final test is different data)
