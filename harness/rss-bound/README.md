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
subtracted, so the result bounds the resident peak from above. The bound holds only
under the deployment conditions the module enforces and reports:

- one single-threaded process, opened with transparent huge pages disabled;
- every probe stays armed with nothing missed;
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

A proof is valid only on the boot that produced it and is never reused.

`prove.sh REVIEW OUT` builds the module against the running kernel's headers, then:

1. loads it and attests that a control-file write while armed invalidates the session
   (eight errors, none missed);
2. unloads it and loads it again;
3. hands the device to the measurement user and runs a positive check (no errors,
   nothing missed, at least 2,048 positive pages);
4. writes `OUT/RSS-PROOF.json`.

The module must be unloaded during timing runs. It is loaded only for memory
collection.

```sh
cargo xtask build-workers
harness/rss-bound/prove.sh REVIEW .research/rss-bound
TRIERGON_RSS_KERNEL_PROOF=.research/rss-bound/RSS-PROOF.json cargo xtask build-workers
TRIERGON_RSS_KERNEL_BOUND=1 cargo xtask report --memory --contract … --manifest … --corpus …
sudo rmmod triergon_rss_bound
```
