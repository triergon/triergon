# triergon-worker-entry

The measurement workers enter through C's `main`, as the C reference workers do,
instead of through the Rust standard library's start-up. A memory or runtime
comparison then measures the library's work against the reference, not the Rust
runtime's start-up: on glibc, that start-up reads `/proc/self/maps` through
`pthread_getattr_np` and `sscanf` to find the main thread's stack guard, which keeps
libc's stdio and scanf code resident that the reference never runs.

Each worker binary writes its entry itself, the one item its `deny(unsafe_code)`
allows:

```rust,ignore
#![deny(unsafe_code)]
#![cfg_attr(not(test), no_main)]

#[cfg_attr(test, allow(dead_code))]
fn worker_main() -> std::process::ExitCode {
    // The worker; `std::env::args` holds the command line as before.
    std::process::ExitCode::SUCCESS
}

#[cfg(not(test))]
#[allow(unsafe_code)]
#[unsafe(no_mangle)]
extern "C" fn main(_argc: std::ffi::c_int, _argv: *const *const std::ffi::c_char) -> std::ffi::c_int {
    triergon_worker_entry::enter(worker_main)
}
```

[`enter`] runs the worker, flushes standard output and standard error, and returns
its exit status; a panic still prints its message and ends with status 101. Test
builds keep the test harness's own entry. This crate itself is safe Rust.

What the standard start-up did and a worker no longer has:

- the message for a main-thread stack overflow: an overflow ends with `SIGSEGV`;
- `SIGPIPE` ignored: writing to a closed pipe ends the worker with `SIGPIPE`, as it
  ends the C reference;
- standard streams reopened on `/dev/null` when the worker starts with one closed;
- the flush of standard output at exit, which [`enter`] and `std::process::exit` do.
