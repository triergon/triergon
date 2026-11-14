# Kernel RSS upper bound

An optional Linux kernel module that gives the memory gate a measured upper bound on
one process's resident set, instead of the observer's declared resident ceiling. It is
measurement tooling only: it is never built with or linked into the library, and it is
licensed GPL-2.0-only, as a module using GPL-only kernel symbols must be.

## What it measures

The observer opens `/dev/triergon_rss_bound` from its constructor, before the
worker's `main` and before any input is loaded. The module then sums the process's
RSS counters once, exactly, across all CPUs, and adds every positive file, anonymous
and shared-memory counter update until the observer's END ioctl. Releases are never
subtracted, so the result bounds the resident peak from above. Up to 32 processes are
measured side by side, each in a session of its own: a counter update counts only
toward the session of the memory map it belongs to, and a process holds one session
at a time. The bound holds only under the deployment conditions the module enforces
and reports:

- each session one single-threaded process, opened with transparent huge pages disabled;
- every probe stays armed with nothing missed while the session is open, whatever
  process the miss happened in;
- no updates from another process's memory map;
- no write to the kprobes control file during a session.

Any violation invalidates the session.

## Proof

`cargo xtask report --memory` uses the bound only with `TRIERGON_RSS_KERNEL_BOUND=1`
and a proof that `cargo xtask build-workers` pinned through
`TRIERGON_RSS_KERNEL_PROOF`. The report re-checks the proof before every memory
collection. It pins:

- the module, observer and probe sources and binaries, and the shared API header
  (`harness/rss-bound-api.h`);
- the running kernel release, architecture and page size;
- the boot, the kernel's build notes and the loaded module's build-id;
- a review of this exact module source for this kernel release.

Reviews are committed beside the module, as
`harness/rss-bound/reviews/SOURCE-RELEASE.json`: SOURCE is the first 12 hexadecimal
digits of the module source's SHA-256 and RELEASE the kernel release (`uname -r`). A
review is a reading of one module source for one kernel, so it is versioned with that
source; a review of a changed source names the review it follows by SHA-256
(`prior_review_sha256`). `prove.sh` and `cargo xtask preflight` take the review that
matches the tree's module source and the running kernel, and fail, naming the path
they looked for, when there is none; `--review FILE` names another.

A proof is valid only on the boot that produced it and is never reused.

The result holds only 64-bit fields, so a 32-bit process on a 64-bit kernel reads the
same layout through the module's 32-bit ioctl entry point. For such a user space,
`prove.sh` takes its root as its last argument and runs on the host (see
[32-bit ARMv7](../../docs/quality.md#32-bit-armv7)).

`prove.sh [--review REVIEW] OUT [USERSPACE]` builds the module against the running kernel's headers, then:

1. loads it and attests that a control-file write while armed invalidates the session
   (eight errors, none missed);
2. unloads it and loads it again;
3. hands the device to the measurement user and runs a positive check (no errors,
   nothing missed, at least 2,048 positive pages), then four such checks side by side,
   each holding its session open for a second;
4. writes `OUT/RSS-PROOF.json`.

The module must be unloaded during timing runs. It is loaded for memory collection
and for grading: `cargo xtask grade` checks every kernel-bound result against the
loaded module's build-id, and grades it unproven without it, so grade before unloading.
The same module binary can be loaded again on the same boot without a new proof
(`sudo insmod target/rss-bound/triergon_rss_bound.ko`, then
`sudo chown "$(id -u):$(id -g)" /dev/triergon_rss_bound`).

```sh
cargo xtask build-workers
taskset -c 1 cargo xtask report --benchmark --contract … --manifest … --corpus … --output TIMING
harness/rss-bound/prove.sh .research/rss-bound
TRIERGON_RSS_KERNEL_PROOF=.research/rss-bound/RSS-PROOF.json cargo xtask build-workers
TRIERGON_RSS_KERNEL_BOUND=1 taskset -c 1-15 cargo xtask report --memory --contract … --manifest … --corpus … --output MEMORY
cargo xtask grade TIMING/report.json MEMORY/report.json --contract … --manifest … --output GRADED
sudo rmmod triergon_rss_bound
```
