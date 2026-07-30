# Results

Measurements from an `m8ib.48xlarge` with nine `gp3` EBS volumes in a software
RAID0, XFS. 15s per run, single run each.

Two implementations were measured:

- **Rust** — `O_DIRECT` + io_uring, concurrency from queue depth
- **C** — `O_DIRECT` + `pwrite`, concurrency from OS threads. Code can be found in https://github.com/awslabs/aws-c-common/tree/disk-write-bench/bin/disk_write_bench

## Headline: block size can substitute for concurrency

A **single thread** issuing 512 MiB writes reaches the same throughput as **32
threads** issuing 8 MiB writes:

| Configuration | Throughput |
|---|---|
| 32 threads × 8 MiB | 15.24 GiB/s |
| 1 thread × 512 MiB | 14.61 GiB/s |
| 1 thread × 8 MiB | 1.63 GiB/s |

This matters for any writer that can choose how much it coalesces before
issuing: buffering into larger writes is an alternative to keeping many writes
in flight, not merely a complement to it.

Single-thread block-size ladder, both write paths:

| block size | `aws_file_path_write_to_offset_direct_io` | `pwrite` (long-lived fd) |
|---|---|---|
| 1 MiB | 0.25 GiB/s | 0.28 GiB/s |
| 64 MiB | 8.51 GiB/s | 10.04 GiB/s |
| 128 MiB | 11.46 GiB/s | 12.13 GiB/s |
| 256 MiB | 13.47 GiB/s | 13.39 GiB/s |
| 512 MiB | 14.61 GiB/s | 14.24 GiB/s |

### The helper's per-call open/close is not a problem

`aws_file_path_write_to_offset_direct_io()` does `open` → `lseek` → `write` →
`close` on **every call**, which looked like it might be a meaningful cost. It
isn't: the two columns above are within noise of each other at every block size,
and at 256 MiB and 512 MiB the helper is nominally *faster*. The fd churn is
irrelevant next to the cost of moving the bytes.

## Preallocation and concurrency

`fallocate` sets the file size up front so writes become overwrites of
already-mapped blocks. Without it, a write past EOF must grow `i_size`, and XFS
takes the inode lock **exclusively** to do that, serializing extending writes
against each other.

**C, threads, 8 MiB blocks:**

| threads | preallocated | not preallocated |
|---|---|---|
| 1 | 1.63 | 1.56 |
| 2 | 3.30 | 1.63 |
| 4 | 6.49 | 2.00 |
| 8 | 12.63 | 3.22 |
| 16 | 14.78 | 5.51 |
| 32 | **15.24** | 8.01 |
| 64 | 13.16 | 12.85 |

**Rust, io_uring queue depth, 8 MiB blocks:**

| depth | preallocated | not preallocated |
|---|---|---|
| 1 | 1.36 | 1.33 |
| 2 | 2.67 | 1.32 |
| 4 | 5.47 | 1.32 |
| 8 | 8.79 | 1.34 |
| 16 | 12.06 | 1.34 |
| 32 | **14.40** | 1.36 |
| 64 | 12.48 | 1.35 |

Preallocated, the two implementations track each other closely and both saturate
around 16-32 concurrent writes, regressing by 64. **io_uring and threads are
interchangeable here** — io_uring's advantage is syscall overhead, which is
negligible at these block sizes.

At depth/threads = 1 preallocation gains nothing (1.63 vs 1.56; 1.36 vs 1.33).
`fallocate` does not make an individual write faster; it makes writes
*parallelizable*.

### Why the two "not preallocated" columns disagree

The Rust column is pinned flat at ~1.33 GiB/s at every depth. The C column
climbs to 12.85. That is **not** io_uring versus threads — it is a difference in
offset access pattern between the two programs, and it means these two columns
are not directly comparable:

- The Rust tool writes one sequential stream from offset 0, so every write lands
  at the current EOF and every write is extending. Serialized throughout.
- The C tool assigns each thread an interleaved stride (thread *t* takes blocks
  *t*, *t+N*, *t+2N*, …). The thread holding the highest offset extends `i_size`
  past the whole current window, after which the other threads' writes fall
  *inside* the existing size and are no longer extending — so they proceed under
  a shared lock. The more threads, the larger that already-extended window, which
  is why the column climbs with thread count.

The practical read is that a writer which happens to touch high offsets early
partially escapes the extending-write penalty. Preallocating escapes it entirely
and is the thing to rely on.

## fsync

| Configuration | Throughput |
|---|---|
| 32 threads, no fsync | 12.95 GiB/s |
| 32 threads, fsync | 12.71 GiB/s |

Negligible, as expected: `O_DIRECT` already bypasses the page cache, so `fsync`
only flushes residual filesystem metadata.

## Block size at high concurrency

C, 32 threads, preallocated:

| block size |  1m | 32m | 64m | 128m | 256m | 512m |
|---|---|---|---|---|---|---|
| GiB/s | 8.51 | 14.39 | 9.19 | 16.14 | 14.63 | 17.33 |


Rust, Preallocated, depth 32:

| block size | 1m | 32m | 64m | 128m | 256m |
|---|---|---|---|---|---|
| GiB/s | 0.26 | 4.64 | 10.57 | 15.23 | 16.81 |

## Measurement noise

Every number here is a single 15s run. The size of the noise is directly
measurable from this data set: **section C's "32 threads, no fsync" (12.95 GiB/s)
and section B's "bs=8m" (16.14 GiB/s) are the same configuration**, and they
differ by 25%.

So:

- Differences under ~25% between single runs are not meaningful.
- The *shapes* — near-linear scaling to 8-16, saturation by 32, regression at 64,
  the collapse below 2 MiB blocks, the flat no-prealloc Rust column — are large
  enough to be real.
- Individual point comparisons are not. Average several runs before acting on
  any specific pair.

EBS throughput also depends on volume provisioning, instance network allocation,
and burst credit state, so absolute numbers are specific to this array on this
day.
