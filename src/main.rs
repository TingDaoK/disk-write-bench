//! Standalone disk write throughput benchmark.
//!
//! Writes to a file for a fixed duration and reports sustained throughput.
//! Mirrors the O_DIRECT + io_uring approach used for parallel writes: a
//! page-aligned staging buffer written via io_uring positioned writes against
//! a descriptor opened with `O_DIRECT`, so the page cache is bypassed
//! entirely.
//!
//! Three write modes are supported so the cost of each layer can be isolated:
//!   - `direct`   O_DIRECT + io_uring   (no page cache, no per-write syscall)
//!   - `uring`    io_uring only         (page cache, no per-write syscall)
//!   - `buffered` plain write(2)        (page cache, one syscall per write)
//!
//! Queue depth is configurable: at depth 1 each write is submitted and waited
//! on individually (submit-and-wait), while higher depths keep multiple writes
//! in flight, which is what lets a multi-queue device reach full bandwidth.

use std::alloc::{alloc, dealloc, Layout};
use std::fs::{File, OpenOptions};
use std::io;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::io::AsRawFd;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use io_uring::{opcode, types, IoUring};

/// O_DIRECT requires the file position, the length, and the buffer address to
/// be aligned to the device's logical block size. 4096 covers 512e and 4Kn
/// devices and matches the page size on the platforms of interest.
const ALIGN: usize = 4096;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Mode {
    Direct,
    DirectSync,
    Uring,
    Buffered,
}

impl Mode {
    fn parse(s: &str) -> Result<Self, String> {
        match s {
            "direct" => Ok(Mode::Direct),
            "direct-sync" => Ok(Mode::DirectSync),
            "uring" => Ok(Mode::Uring),
            "buffered" => Ok(Mode::Buffered),
            other => Err(format!(
                "unknown mode {other:?} (expected direct, direct-sync, uring, or buffered)"
            )),
        }
    }

    fn label(self) -> &'static str {
        match self {
            Mode::Direct => "O_DIRECT + io_uring",
            Mode::DirectSync => "O_DIRECT + write(2)",
            Mode::Uring => "io_uring (buffered)",
            Mode::Buffered => "write(2) (buffered)",
        }
    }

    fn uses_uring(self) -> bool {
        matches!(self, Mode::Direct | Mode::Uring)
    }

    /// Whether the output file needs a descriptor opened with `O_DIRECT`.
    fn needs_direct_fd(self) -> bool {
        matches!(self, Mode::Direct | Mode::DirectSync)
    }
}

struct Args {
    path: PathBuf,
    block_size: usize,
    queue_depth: usize,
    duration: Duration,
    mode: Mode,
    max_bytes: Option<u64>,
    keep: bool,
    fsync: bool,
    no_prealloc: bool,
}

impl Args {
    fn parse() -> Result<Self, String> {
        let mut path = None;
        let mut block_size = 8 * 1024 * 1024; // 8 MiB, matches the TM part size
        let mut queue_depth = 1;
        let mut secs = 30u64;
        let mut mode = Mode::Direct;
        let mut max_bytes = None;
        let mut keep = false;
        let mut fsync = false;
        let mut no_prealloc = false;

        let argv: Vec<String> = std::env::args().skip(1).collect();
        let mut i = 0;
        while i < argv.len() {
            let arg = argv[i].as_str();
            // Every flag below takes a value, so fetch it up front.
            let mut value = || -> Result<String, String> {
                i += 1;
                argv.get(i)
                    .cloned()
                    .ok_or_else(|| format!("{arg} requires a value"))
            };
            match arg {
                "-h" | "--help" => {
                    print_usage();
                    std::process::exit(0);
                }
                "--path" => path = Some(PathBuf::from(value()?)),
                "--block-size" => block_size = parse_size(&value()?)?,
                "--queue-depth" => {
                    queue_depth = value()?.parse().map_err(|e| format!("--queue-depth: {e}"))?
                }
                "--duration" => secs = value()?.parse().map_err(|e| format!("--duration: {e}"))?,
                "--mode" => mode = Mode::parse(&value()?)?,
                "--max-bytes" => max_bytes = Some(parse_size(&value()?)? as u64),
                "--keep" => keep = true,
                "--fsync" => fsync = true,
                "--no-prealloc" => no_prealloc = true,
                other => return Err(format!("unknown argument {other:?} (try --help)")),
            }
            i += 1;
        }

        let path = path.ok_or("--path is required (a file on the target filesystem)")?;

        if block_size == 0 || block_size % ALIGN != 0 {
            return Err(format!(
                "--block-size must be a non-zero multiple of {ALIGN} for O_DIRECT"
            ));
        }
        if queue_depth == 0 {
            return Err("--queue-depth must be at least 1".to_string());
        }

        Ok(Args {
            path,
            block_size,
            queue_depth,
            duration: Duration::from_secs(secs),
            mode,
            max_bytes,
            keep,
            fsync,
            no_prealloc,
        })
    }
}

/// Parse a byte size with an optional `k`/`m`/`g` suffix (binary multiples).
fn parse_size(s: &str) -> Result<usize, String> {
    let s = s.trim();
    let (digits, mult) = match s.chars().last() {
        Some('k') | Some('K') => (&s[..s.len() - 1], 1024),
        Some('m') | Some('M') => (&s[..s.len() - 1], 1024 * 1024),
        Some('g') | Some('G') => (&s[..s.len() - 1], 1024 * 1024 * 1024),
        _ => (s, 1),
    };
    let n: usize = digits
        .trim()
        .parse()
        .map_err(|e| format!("invalid size {s:?}: {e}"))?;
    Ok(n * mult)
}

fn print_usage() {
    eprintln!(
        "\
Disk write throughput benchmark (O_DIRECT + io_uring)

USAGE:
    disk-write-bench --path <FILE> [OPTIONS]

OPTIONS:
    --path <FILE>          Output file. Must be on the filesystem under test.
                           O_DIRECT is unsupported on tmpfs, so avoid /tmp if
                           it is a tmpfs mount.
    --mode <MODE>          direct | direct-sync | uring | buffered  [default: direct]
                             direct      = O_DIRECT + io_uring (bypasses page cache)
                             direct-sync = O_DIRECT + plain write(2), one at a
                                           time; mirrors a simple C write loop
                             uring    = io_uring, page cache in use
                             buffered = write(2), page cache in use
    --block-size <SIZE>    Bytes per write, multiple of 4096  [default: 8m]
                           Accepts k/m/g suffixes.
    --queue-depth <N>      Writes kept in flight               [default: 1]
                           1 = submit-and-wait per write.
    --duration <SECS>      How long to write                   [default: 30]
    --max-bytes <SIZE>     Stop after this many bytes (optional cap)
    --fsync                fsync() at the end and include it in the total time
    --keep                 Do not delete the output file on exit
    --no-prealloc          Skip fallocate. Writes then EXTEND the file, which
                           serializes them on the inode lock (XFS takes it
                           exclusively for size-extending O_DIRECT writes) and
                           makes queue depth irrelevant. Use to demonstrate the
                           effect; not a realistic configuration.

EXAMPLES:
    # 30s O_DIRECT write test with 4 writes in flight
    disk-write-bench --path /mnt/raid0/bench.dat --queue-depth 4

    # Compare against the buffered path
    disk-write-bench --path /mnt/raid0/bench.dat --mode buffered

    # Sweep queue depth to find where the device saturates
    for q in 1 2 4 8 16 32; do
        disk-write-bench --path /mnt/raid0/bench.dat --queue-depth $q --duration 10
    done
"
    );
}

/// Page-aligned heap buffer. `O_DIRECT` rejects unaligned buffer addresses,
/// and Rust's allocator gives no alignment guarantee beyond the type's, so the
/// allocation is made explicitly with an aligned `Layout`.
struct AlignedBuf {
    ptr: *mut u8,
    layout: Layout,
    len: usize,
}

impl AlignedBuf {
    fn new(len: usize) -> io::Result<Self> {
        let layout = Layout::from_size_align(len, ALIGN)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
        // SAFETY: layout has non-zero size (callers validate block_size != 0).
        let ptr = unsafe { alloc(layout) };
        if ptr.is_null() {
            return Err(io::Error::new(
                io::ErrorKind::OutOfMemory,
                "failed to allocate aligned buffer",
            ));
        }
        // Fill with a non-trivial pattern: an all-zero buffer can be optimized
        // away by a filesystem or device that detects sparse writes.
        // SAFETY: ptr is valid for len bytes, just allocated.
        unsafe {
            for i in 0..len {
                ptr.add(i).write((i % 251) as u8);
            }
        }
        Ok(Self { ptr, layout, len })
    }
}

impl Drop for AlignedBuf {
    fn drop(&mut self) {
        // SAFETY: ptr came from alloc with this exact layout and is freed once.
        unsafe { dealloc(self.ptr, self.layout) }
    }
}

// The raw pointer is only ever handed to io_uring/pwrite as a read-only source
// from the single thread that owns the buffer.
unsafe impl Send for AlignedBuf {}

struct Stats {
    bytes: u64,
    writes: u64,
    elapsed: Duration,
    fsync_time: Option<Duration>,
    /// Highest number of writes observed simultaneously in flight. If this stays
    /// at 1 while --queue-depth is higher, submissions are not overlapping.
    peak_in_flight: usize,
}

fn main() {
    let args = match Args::parse() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("error: {e}\n");
            print_usage();
            std::process::exit(2);
        }
    };

    if let Err(e) = run(&args) {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

/// Preallocate `len` bytes so the benchmark writes are overwrites rather than
/// size-extending appends.
///
/// This matters more than it looks: on XFS a size-extending `O_DIRECT` write
/// takes the inode lock exclusively, so appends serialize against each other no
/// matter how many are in flight — queue depth buys nothing and the measurement
/// reports the serialized-append rate instead of device bandwidth. Overwrites of
/// already-allocated blocks take the lock shared and proceed concurrently.
/// `fallocate` also avoids interleaving extent-allocation work with the writes.
fn preallocate(file: &File, len: u64) -> io::Result<()> {
    // SAFETY: fd is valid for the lifetime of `file`; mode 0 is plain fallocate.
    let rc = unsafe { libc::fallocate(file.as_raw_fd(), 0, 0, len as libc::off_t) };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn run(args: &Args) -> io::Result<()> {
    // Create the file first with a plain descriptor. O_DIRECT is applied on a
    // second open below so a failure there can be reported distinctly from a
    // failure to create the file at all.
    let plain = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(&args.path)?;

    let file = if args.mode.needs_direct_fd() {
        match OpenOptions::new()
            .write(true)
            .custom_flags(libc::O_DIRECT)
            .open(&args.path)
        {
            Ok(f) => f,
            Err(e) => {
                return Err(io::Error::new(
                    e.kind(),
                    format!(
                        "cannot open {} with O_DIRECT: {e}. \
                         The filesystem may not support it (tmpfs does not); \
                         try --mode uring or --mode buffered, or a path on a real disk.",
                        args.path.display()
                    ),
                ));
            }
        }
    } else {
        plain.try_clone()?
    };

    let fs_kind = filesystem_hint(&args.path);
    println!("disk write benchmark");
    println!("  path         : {}", args.path.display());
    println!("  filesystem   : {fs_kind}");
    println!("  mode         : {}", args.mode.label());
    println!(
        "  block size   : {} ({} bytes)",
        human_bytes(args.block_size as u64),
        args.block_size
    );
    println!("  queue depth  : {}", args.queue_depth);
    println!("  duration     : {}s", args.duration.as_secs());
    if let Some(cap) = args.max_bytes {
        println!("  max bytes    : {}", human_bytes(cap));
    }
    println!("  fsync at end : {}", args.fsync);

    // Preallocate unless explicitly disabled. Without this, every write extends
    // the file and serializes on the inode lock (see preallocate()).
    if args.no_prealloc {
        println!("  preallocate  : disabled  <-- appends serialize on the inode lock");
    } else {
        let target = args.max_bytes.unwrap_or_else(|| {
            // No explicit cap: reserve enough for the duration at an optimistic
            // rate so the writes stay inside the allocated extent.
            let optimistic_bytes_per_sec = 20u64 * 1024 * 1024 * 1024;
            args.duration.as_secs().max(1) * optimistic_bytes_per_sec
        });
        match preallocate(&plain, target) {
            Ok(()) => println!("  preallocate  : {} (fallocate)", human_bytes(target)),
            Err(e) => println!("  preallocate  : FAILED ({e}) -- appends will serialize"),
        }
    }
    println!();

    let stats = if args.mode.uses_uring() {
        run_uring(args, &file)?
    } else {
        run_buffered(args, &file)?
    };

    report(args, &stats);

    drop(file);
    drop(plain);
    if !args.keep {
        let _ = std::fs::remove_file(&args.path);
    } else {
        println!("\noutput file kept at {}", args.path.display());
    }
    Ok(())
}

/// io_uring write loop. Keeps up to `queue_depth` writes in flight: the
/// submission queue is topped up from a pool of free staging buffers, then
/// completions are reaped as they arrive and their buffers returned to the
/// pool. At depth 1 this degenerates to submit-one-wait-one.
///
/// Completions can arrive in any order, so a buffer is only reusable once its
/// own completion has been seen. Each submission carries its slot index as
/// `user_data`, and that slot is pushed back onto the free list when the
/// matching CQE is reaped — the kernel owns a buffer for exactly the interval
/// between its submission and its completion.
fn run_uring(args: &Args, file: &File) -> io::Result<Stats> {
    // Ring must be at least the queue depth; io_uring rounds to a power of two.
    let entries = args.queue_depth.next_power_of_two().max(2) as u32;
    let mut ring = IoUring::new(entries)?;

    // One buffer per in-flight slot: a write owns its buffer until its
    // completion is reaped, so slots cannot share one.
    let buffers: Vec<AlignedBuf> = (0..args.queue_depth)
        .map(|_| AlignedBuf::new(args.block_size))
        .collect::<io::Result<_>>()?;

    // Slots not currently owned by the kernel. Starts full.
    let mut free_slots: Vec<usize> = (0..args.queue_depth).rev().collect();

    let fd = types::Fd(file.as_raw_fd());
    let block = args.block_size as u64;

    let mut offset = 0u64;
    let mut bytes_done = 0u64;
    let mut writes_done = 0u64;
    let mut peak_in_flight = 0usize;

    let start = Instant::now();
    let deadline_reached = |off: u64| -> bool {
        start.elapsed() >= args.duration || args.max_bytes.is_some_and(|cap| off >= cap)
    };

    loop {
        // Top up the submission queue from the free pool while work remains.
        let mut submitted_now = 0usize;
        while !free_slots.is_empty() && !deadline_reached(offset) {
            let slot = *free_slots.last().expect("free_slots is non-empty");
            let buf = &buffers[slot];
            let sqe = opcode::Write::new(fd, buf.ptr, buf.len as u32)
                .offset(offset)
                .build()
                .user_data(slot as u64);
            // SAFETY: `buffers` outlives this loop, and `slot` was on the free
            // list, so the kernel does not already own this buffer. The slot is
            // not returned to the free list until its completion is reaped.
            let pushed = unsafe { ring.submission().push(&sqe).is_ok() };
            if !pushed {
                // Submission queue full: drain completions before retrying.
                break;
            }
            free_slots.pop();
            offset += block;
            submitted_now += 1;
        }

        let in_flight = args.queue_depth - free_slots.len();
        if in_flight == 0 {
            break; // deadline hit and nothing outstanding
        }
        peak_in_flight = peak_in_flight.max(in_flight);

        // Submit anything newly queued and block until at least one write lands.
        ring.submit_and_wait(1)?;

        // Reap every available completion, returning each slot to the pool.
        let completions: Vec<(u64, i32)> = ring
            .completion()
            .map(|cqe| (cqe.user_data(), cqe.result()))
            .collect();
        for (slot, res) in completions {
            if res < 0 {
                return Err(io::Error::from_raw_os_error(-res));
            }
            let n = res as u64;
            if n != block {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    format!("short write: {n} of {block} bytes"),
                ));
            }
            bytes_done += n;
            writes_done += 1;
            free_slots.push(slot as usize);
        }

        // Guard against a spin: if nothing was submitted and nothing completed
        // there is no progress to be made.
        debug_assert!(submitted_now > 0 || !free_slots.is_empty());
    }

    finish(args, file, start, bytes_done, writes_done, peak_in_flight)
}

/// Plain `write(2)` loop via `lseek` + `write`, for a syscall-per-write
/// baseline. `pwrite` is avoided for consistency with codebases that cannot
/// rely on it being declared.
fn run_buffered(args: &Args, file: &File) -> io::Result<Stats> {
    use std::io::{Seek, SeekFrom, Write};

    let buf = AlignedBuf::new(args.block_size)?;
    // SAFETY: ptr is valid for len bytes and outlives the slice.
    let slice = unsafe { std::slice::from_raw_parts(buf.ptr, buf.len) };
    let mut f = file.try_clone()?;

    let mut offset = 0u64;
    let mut bytes_done = 0u64;
    let mut writes_done = 0u64;
    let start = Instant::now();

    while start.elapsed() < args.duration && !args.max_bytes.is_some_and(|cap| offset >= cap) {
        f.seek(SeekFrom::Start(offset))?;
        f.write_all(slice)?;
        offset += args.block_size as u64;
        bytes_done += args.block_size as u64;
        writes_done += 1;
    }

    finish(args, file, start, bytes_done, writes_done, 1)
}

/// Optionally fsync, then stop the clock. When `--fsync` is set the flush is
/// inside the measured window, because a buffered run that skips it is only
/// reporting the rate of filling the page cache, not of reaching the device.
fn finish(
    args: &Args,
    file: &File,
    start: Instant,
    bytes: u64,
    writes: u64,
    peak_in_flight: usize,
) -> io::Result<Stats> {
    let mut fsync_time = None;
    if args.fsync {
        let t = Instant::now();
        file.sync_all()?;
        fsync_time = Some(t.elapsed());
    }
    Ok(Stats {
        bytes,
        writes,
        elapsed: start.elapsed(),
        fsync_time,
        peak_in_flight,
    })
}

fn report(args: &Args, s: &Stats) {
    let secs = s.elapsed.as_secs_f64();
    let bytes = s.bytes as f64;
    let gib_s = bytes / secs / (1024.0 * 1024.0 * 1024.0);
    let gb_s = bytes / secs / 1e9;
    let gbit_s = bytes * 8.0 / secs / 1e9;
    let iops = s.writes as f64 / secs;

    println!("results");
    println!("  elapsed      : {secs:.3} s");
    println!("  written      : {} ({} bytes)", human_bytes(s.bytes), s.bytes);
    println!("  writes       : {}", s.writes);
    println!("  throughput   : {gib_s:.2} GiB/s  ({gb_s:.2} GB/s, {gbit_s:.2} Gb/s)");
    println!("  IOPS         : {iops:.0}");
    println!(
        "  peak in-flgt : {} of {} requested",
        s.peak_in_flight, args.queue_depth
    );
    if args.mode.uses_uring() && args.queue_depth > 1 && s.peak_in_flight <= 1 {
        println!("  WARNING: writes never overlapped -- submissions are serializing");
    }
    println!(
        "  avg latency  : {:.3} ms/write (queue depth {})",
        secs * 1000.0 / s.writes.max(1) as f64,
        args.queue_depth
    );
    if let Some(f) = s.fsync_time {
        println!("  fsync        : {:.3} s (included above)", f.as_secs_f64());
    }
    if !args.mode.needs_direct_fd() && !args.fsync {
        println!(
            "\n  note: buffered mode without --fsync measures the rate of filling the\n\
             \x20       page cache, not of reaching the device. Add --fsync to compare\n\
             \x20       against O_DIRECT on equal terms."
        );
    }
}

/// Best-effort filesystem type for the path's mount point, read from
/// /proc/mounts by longest-prefix match. Reported because O_DIRECT support and
/// achievable throughput both depend on it (notably, tmpfs rejects O_DIRECT).
fn filesystem_hint(path: &std::path::Path) -> String {
    let target = path
        .parent()
        .and_then(|p| p.canonicalize().ok())
        .unwrap_or_else(|| PathBuf::from("/"));
    let mounts = match std::fs::read_to_string("/proc/mounts") {
        Ok(m) => m,
        Err(_) => return "unknown".to_string(),
    };
    let mut best: Option<(usize, String, String)> = None;
    for line in mounts.lines() {
        let mut f = line.split_whitespace();
        let dev = f.next().unwrap_or("");
        let mount = f.next().unwrap_or("");
        let fstype = f.next().unwrap_or("");
        if target.starts_with(mount) && best.as_ref().is_none_or(|(n, _, _)| mount.len() > *n) {
            best = Some((mount.len(), fstype.to_string(), dev.to_string()));
        }
    }
    match best {
        Some((_, fstype, dev)) => {
            let warn = if fstype == "tmpfs" {
                "  <-- tmpfs does not support O_DIRECT"
            } else {
                ""
            };
            format!("{fstype} on {dev}{warn}")
        }
        None => "unknown".to_string(),
    }
}

fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{n} {}", UNITS[0])
    } else {
        format!("{v:.2} {}", UNITS[i])
    }
}
