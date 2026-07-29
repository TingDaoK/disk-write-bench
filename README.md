# disk-write-bench

Measures sustained disk **write** throughput on Linux, with four write paths so
each layer of the stack can be isolated: `O_DIRECT` vs page cache, and io_uring
vs `write(2)`.

Built to answer a specific question — why a parallel S3 download-to-file path
was not reaching line rate on a striped EBS array — and the answer turned out to
be non-obvious enough to be worth keeping. See [Results](#results).

Dependencies are `io-uring` and `libc` only. No async runtime, no CLI framework.

## Build

```bash
cargo build --release
```

## Usage

```bash
# 30s O_DIRECT write test, 8 MiB blocks, 8 writes in flight
./target/release/disk-write-bench --path /mnt/raid0/bench.dat --queue-depth 8

# Full comparison matrix (~8 min)
./sweep.sh /mnt/raid0/bench.dat 15
```

| Flag | Meaning |
|---|---|
| `--path <FILE>` | Output file. Must be on the filesystem under test. |
| `--mode <MODE>` | `direct` \| `direct-sync` \| `uring` \| `buffered` (default `direct`) |
| `--block-size <SIZE>` | Bytes per write, multiple of 4096. Accepts `k`/`m`/`g`. Default `8m`. |
| `--queue-depth <N>` | Writes kept in flight. Default 1. |
| `--duration <SECS>` | How long to write. Default 30. |
| `--max-bytes <SIZE>` | Stop after this many bytes; also bounds the `fallocate` reservation. |
| `--no-prealloc` | Skip `fallocate`, so writes extend the file (see [Results](#results)). |
| `--fsync` | `fsync()` at the end, inside the measured window. |
| `--keep` | Do not delete the output file on exit. |

### Modes

| Mode | Page cache | Syscall per write | Concurrent |
|---|---|---|---|
| `direct` | bypassed | no (io_uring) | yes, up to queue depth |
| `direct-sync` | bypassed | yes | no |
| `uring` | used | no (io_uring) | yes, up to queue depth |
| `buffered` | used | yes | no |

Buffered modes without `--fsync` measure the rate of filling RAM, not of
reaching the device — the tool prints a warning saying so. Use `--fsync` for a
fair comparison against `O_DIRECT`.

Every run reports `peak in-flight`, the highest number of writes observed
outstanding simultaneously. This exists so concurrency can be **verified**
rather than assumed: a flat throughput curve with `peak in-flight = 32 of 32`
means something other than your submission path is serializing the writes.

## Results

Measured on a `m8ib.48xlarge` with nine `gp3` EBS volumes in a software RAID0,
XFS, 15s per run. Setup in [EC2 setup](#ec2-setup).

### Preallocation and queue depth are both required; neither works alone

`--mode direct`, 8 MiB blocks. Throughput in GiB/s:

| queue depth | preallocated | not preallocated |
|---|---|---|
| 1 | 1.36 | 1.33 |
| 2 | 2.67 | 1.32 |
| 4 | 5.47 | 1.32 |
| 8 | 8.79 | 1.34 |
| 16 | 12.06 | 1.34 |
| 32 | **14.40** | 1.36 |
| 64 | 12.48 | 1.35 |

Two things to read out of this:

**Preallocation at depth 1 buys nothing** (1.36 vs 1.33). `fallocate` does not
make an individual write faster — it makes writes *parallelizable*.

**Without preallocation, depth buys nothing** — pinned at ~1.33 GiB/s from depth
1 to 64, while `peak in-flight` truthfully reports 64 of 64. The requests really
are concurrent; the filesystem retires them one at a time anyway.

The mechanism is the inode lock. A write past EOF must both allocate blocks and
grow `i_size`, and XFS takes the inode lock **exclusively** to do it, so
extending writes serialize against each other. Preallocating with `fallocate`
sets the file size up front, after which writes are overwrites of already-mapped
blocks and take the lock shared.

`filefrag` shows the three states a region can be in:

```
after fallocate, before any write:
   0:  0..16383:  47254528..47270911:  16384:  last,unwritten,eof

after writing the first half:
   0:  0.. 8191:  47254528..47262719:   8192:                        <- written
   1:  8192..16383:  47262720..47270911:   8192:  last,unwritten,eof <- untouched
```

`fallocate` produces `unwritten` extents: allocated, size set, but flagged as
containing no valid data. A first write must still convert the extent to
written, which is cheaper than extending but not free. A pure overwrite of
already-written blocks is the fastest case of all — but a download to a *new*
file can never reach it, so `fallocate` is the only lever available there.

### O_DIRECT vs the page cache

Preallocated, depth 32, 8 MiB blocks. Buffered modes include `--fsync`:

| Mode | Throughput |
|---|---|
| `direct` (O_DIRECT + io_uring) | **16.41 GiB/s** (141 Gb/s) |
| `uring` (buffered + io_uring) | 2.58 GiB/s |
| `buffered` (`write(2)`) | 2.57 GiB/s |

A 6.4x gap, and the two buffered variants are indistinguishable — once `fsync`
forces data to the device, io_uring's syscall savings are irrelevant. The cost
is the page-cache copy and writeback, which also shows up as very high system
CPU time (an unrelated `aws s3 cp` of 1 TiB on this array spent ~44 minutes of
`sys` time against 2m40s of wall clock).

### Block size

Preallocated, depth 32:

| block size | 1m | 2m | 4m | 8m | 16m | 64m |
|---|---|---|---|---|---|---|
| GiB/s | 7.13 | 8.02 | 12.80 | 14.78 | 15.14 | 15.66 |

Sharp gains up to 8 MiB, diminishing after. 8 MiB is a reasonable default; going
to 64 MiB gains ~6% for 8x the buffer memory.

### Caveats

These are single runs, not averages, from one instance on one day. EBS
throughput depends on volume provisioning, instance network allocation, and
burst credit state. Treat the shape of the curves as the finding and the
absolute numbers as indicative. Reproduce with `./sweep.sh` on your own setup
before relying on them.

## EC2 setup

Nine `gp3` volumes striped with `mdadm`, each provisioned at 2000 MiB/s and
16000 IOPS. Striping is what makes the array fast enough to expose the
serialization above — a single volume caps well below it.

Instance: `m8ib.48xlarge` (chosen for network bandwidth; any instance with
enough EBS bandwidth works). Volumes attached as `/dev/xvdb` … `/dev/xvdj`.

```bash
# 1. Create the array (adjust device list to match your attachments)
sudo mdadm --create --verbose /dev/md0 \
    --level=0 --raid-devices=9 \
    /dev/xvd{b,c,d,e,f,g,h,i,j}

# 2. Filesystem. XFS handles large sequential writes and parallel
#    positioned writes well, and supports O_DIRECT.
sudo mkfs.xfs /dev/md0

# 3. Mount
sudo mkdir -p /mnt/raid0
sudo mount /dev/md0 /mnt/raid0
sudo chown "$USER" /mnt/raid0
```

To survive reboots:

```bash
# Record the array so it is assembled at boot
sudo mdadm --detail --scan | sudo tee -a /etc/mdadm.conf

# Amazon Linux: rebuild initramfs so md is available early
sudo dracut -H -f /boot/initramfs-"$(uname -r)".img "$(uname -r)"

# fstab by UUID, with nofail so a missing array cannot block boot
sudo blkid /dev/md0        # take the UUID
echo 'UUID=<uuid> /mnt/raid0 xfs defaults,nofail 0 2' | sudo tee -a /etc/fstab
```

After a reboot the array may come back under a different device node (`/dev/md0`
becoming `/dev/md127` is common) — this is why `fstab` should reference the
filesystem UUID rather than the device. If it did not assemble at all:

```bash
sudo mdadm --assemble --scan
sudo mount /mnt/raid0
```

### Verifying the array before benchmarking

```bash
cat /proc/mdstat                    # all devices present and active
df -T /mnt/raid0                    # confirms xfs, not tmpfs
```

The `df -T` check matters. `O_DIRECT` is **not supported on tmpfs**, and opening
with it there fails outright — pointing `--path` at `/tmp` on a host where
`/tmp` is a tmpfs mount will not measure what you think. The tool prints the
detected filesystem type and warns when it is tmpfs.
