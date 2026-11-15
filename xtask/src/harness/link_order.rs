//! Link order: a worker's functions laid out by what a training corpus runs, then
//! grouped by the item that owns them.
//!
//! A trained order lists the functions the training corpus ran in a worker, by
//! demangled name in first-use order ([`first_use`]); one is committed per worker
//! and target under `harness/link-order/TARGET/WORKER.txt`. The ordering file of a
//! binary ([`symbol_order`]) puts its functions that the trained order lists first,
//! in its order, and every other function after them in module order: keyed by its
//! owning path, a free function by its own path, a method by its type's path and
//! name, a closure by its function's, and drop glue by the type it drops, with
//! generic arguments left out. Sorting by that path puts each crate's functions
//! together, and within a crate each module's and each type's. A name the binary
//! does not hold is skipped and counted: a build whose binary holds fewer than
//! [`MIN_HELD_PERCENT`] of its trained order's names fails. The file is
//! computed from a first link of the worker and applied by linking again
//! ([`super::gate_build::ordered_link_arguments`]); ordering the second link
//! gives the same file.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

/// Where the trained order of `worker` for `target` is committed.
pub fn trained_path(root: &Path, target: &str, worker: &str) -> PathBuf {
    root.join("harness/link-order")
        .join(target)
        .join(format!("{worker}.txt"))
}

/// The function names of a trained order: one demangled name per line, in
/// first-use order, each once; a line starting with `#` is a comment.
pub fn parse_trained(text: &str) -> Result<Vec<String>, String> {
    let mut names = Vec::new();
    let mut seen = HashSet::new();
    for (index, line) in text.lines().enumerate() {
        if line.starts_with('#') {
            continue;
        }
        if line.is_empty() || line.trim() != line {
            return Err(format!("line {}: not a function name", index + 1));
        }
        if !seen.insert(line) {
            return Err(format!("line {}: {line} listed twice", index + 1));
        }
        names.push(line.to_owned());
    }
    Ok(names)
}

/// A demangled name without the suffix LLVM gives a local function it promotes,
/// which changes from build to build.
fn stable_name(name: &str) -> &str {
    name.find(".llvm.")
        .map_or(name, |end| name[..end].trim_end_matches(" ("))
}

/// A code symbol as `nm` lists it: its address, its size where the symbol table
/// gives one, and its name.
struct Symbol {
    address: u64,
    size: Option<u64>,
    name: String,
}

/// The code symbols `binary` defines, demangled or not, in symbol-table order.
fn code_symbols(binary: &Path, demangle: bool) -> Result<Vec<Symbol>, String> {
    let mut argv: Vec<String> = ["nm", "--defined-only", "-p", "-S"]
        .map(str::to_owned)
        .to_vec();
    if demangle {
        argv.push("-C".to_owned());
    }
    argv.push(binary.display().to_string());
    let text = crate::process::check_output(&argv)?;
    Ok(text.lines().filter_map(code_symbol).collect())
}

/// One line of `nm -S`: `ADDRESS [SIZE] KIND NAME`, the name possibly with spaces.
fn code_symbol(line: &str) -> Option<Symbol> {
    let (address, rest) = line.split_once(' ')?;
    let address = u64::from_str_radix(address, 16).ok()?;
    let (size, rest) = match rest.split_once(' ')? {
        (kind, _) if kind.len() == 1 => (None, rest),
        (size, rest) => (Some(u64::from_str_radix(size, 16).ok()?), rest),
    };
    let (kind, name) = rest.split_once(' ')?;
    matches!(kind, "t" | "T" | "w" | "W").then(|| Symbol {
        address,
        size,
        name: name.to_owned(),
    })
}

/// The code symbols of `binary` paired with their demangled names.
fn named_symbols(binary: &Path) -> Result<Vec<(Symbol, String)>, String> {
    let symbols = code_symbols(binary, false)?;
    let names = code_symbols(binary, true)?;
    if symbols.len() != names.len() {
        return Err(format!(
            "{}: nm lists {} symbols and {} demangled names",
            binary.display(),
            symbols.len(),
            names.len()
        ));
    }
    Ok(symbols
        .into_iter()
        .zip(names)
        .filter(|(symbol, _)| !linker_made(&symbol.name))
        .map(|(symbol, named)| (symbol, named.name))
        .collect())
}

/// Code the linker makes itself where the layout needs it: branch thunks for targets
/// out of a branch's range and patches for ARM core errata (lld's `__*Thunk_*`,
/// `__CortexA53843419_*` and `__CortexA8657417_*`). Which exist depends on where
/// functions lie, so they are left out of the order.
fn linker_made(symbol: &str) -> bool {
    symbol.starts_with("__")
        && (symbol.contains("Thunk_")
            || symbol.starts_with("__CortexA53843419_")
            || symbol.starts_with("__CortexA8657417_"))
}

/// The least share, in percent, of a trained order's names a binary must hold for
/// the order to lay it out.
pub const MIN_HELD_PERCENT: usize = 95;

/// A binary's symbol ordering file and how many of the trained order's names the
/// binary holds.
pub struct Ordering {
    pub text: String,
    pub held: usize,
}

/// The least share, in percent, of a trained order's names a worker built for another
/// target must hold: some functions exist on one architecture only.
pub const MIN_HELD_PERCENT_ACROSS_TARGETS: usize = 90;

/// Whether a binary that holds `held` of a trained order's `trained` names is laid out
/// by it.
pub fn holds_enough(held: usize, trained: usize) -> bool {
    holds_percent(held, trained, MIN_HELD_PERCENT)
}

/// Whether `held` of `trained` is at least `percent`.
pub fn holds_percent(held: usize, trained: usize, percent: usize) -> bool {
    held * 100 >= trained * percent
}

/// The symbol ordering file of `binary`: one symbol per line, those `trained` names
/// first in its order, then the rest by owning path, demangled name and symbol name,
/// without the code the linker makes itself.
pub fn symbol_order(binary: &Path, trained: &[String]) -> Result<Ordering, String> {
    let symbols = named_symbols(binary)?;
    let rank: HashMap<&str, usize> = trained
        .iter()
        .enumerate()
        .map(|(index, name)| (name.as_str(), index))
        .collect();
    let mut held = HashSet::new();
    let mut rows: Vec<(usize, String, &str, &str)> = symbols
        .iter()
        .map(|(symbol, name)| {
            let rank = rank.get(stable_name(name)).copied().unwrap_or(usize::MAX);
            if rank != usize::MAX {
                held.insert(rank);
            }
            (rank, owner(name), name.as_str(), symbol.name.as_str())
        })
        .collect();
    rows.sort_unstable();
    let mut text = String::new();
    let mut previous = None;
    for (_, _, _, symbol) in rows {
        // A local name the binary holds more than once is one line of the file.
        if previous != Some(symbol) {
            text.push_str(symbol);
            text.push('\n');
        }
        previous = Some(symbol);
    }
    Ok(Ordering {
        text,
        held: held.len(),
    })
}

/// A function of a binary: where its code lies and its stable demangled name.
struct Function {
    start: u64,
    end: u64,
    name: String,
}

/// The sized code symbols of `binary` with their stable demangled names, by address.
fn functions(binary: &Path) -> Result<Vec<Function>, String> {
    let mut functions: Vec<Function> = named_symbols(binary)?
        .into_iter()
        .filter_map(|(symbol, name)| {
            let size = symbol.size.filter(|&size| size > 0)?;
            // A Thumb function's symbol holds its address with the lowest bit set.
            let start = symbol.address & !1;
            Some(Function {
                start,
                end: start.checked_add(size)?,
                name: stable_name(&name).to_owned(),
            })
        })
        .collect();
    functions.sort_by(|a, b| (a.start, &a.name).cmp(&(b.start, &b.name)));
    Ok(functions)
}

/// The addresses the executable load segments of `binary` span in its ELF
/// program headers: the lowest start and the highest end of their file contents.
fn executable_span(binary: &Path) -> Result<(u64, u64), String> {
    use super::linux::{u16le, u32le, u64le};
    let image = std::fs::read(binary).map_err(|error| format!("{}: {error}", binary.display()))?;
    let malformed = |problem: &str| format!("{}: {problem}", binary.display());
    if image.get(0..4) != Some(b"\x7fELF") || image.get(5) != Some(&1) {
        return Err(malformed("not a little-endian ELF file"));
    }
    // (word reader, program header offset field, entry size field, count field,
    // flags offset, virtual address offset, file size offset) of ELF32 and ELF64.
    let word32 = |at: usize| u32le(&image, at).map(u64::from);
    let word64 = |at: usize| u64le(&image, at);
    let (word, table, size, count, flags, address, length): (
        &dyn Fn(usize) -> Result<u64, String>,
        _,
        _,
        _,
        _,
        _,
        _,
    ) = match image.get(4) {
        Some(1) => (&word32, 28, 42, 44, 24, 8, 16),
        Some(2) => (&word64, 32, 54, 56, 4, 16, 32),
        _ => return Err(malformed("unknown ELF class")),
    };
    let table =
        usize::try_from(word(table)?).map_err(|_| malformed("program headers out of range"))?;
    let size = usize::from(u16le(&image, size)?);
    let mut span: Option<(u64, u64)> = None;
    for index in 0..usize::from(u16le(&image, count)?) {
        let header = index
            .checked_mul(size)
            .and_then(|offset| offset.checked_add(table))
            .ok_or_else(|| malformed("program headers out of range"))?;
        let at = |field: usize| {
            header
                .checked_add(field)
                .ok_or_else(|| malformed("program headers out of range"))
        };
        // A loadable segment (PT_LOAD) that is executable (PF_X).
        if u32le(&image, header)? != 1 || u32le(&image, at(flags)?)? & 1 == 0 {
            continue;
        }
        let start = word(at(address)?)?;
        let end = start
            .checked_add(word(at(length)?)?)
            .ok_or_else(|| malformed("segment out of range"))?;
        span = Some(span.map_or((start, end), |(low, high)| (low.min(start), high.max(end))));
    }
    span.ok_or_else(|| malformed("no executable segment"))
}

/// A `NAME 0xADDRESS` line of QEMU's `page` log.
fn logged_address(text: &str, name: &str) -> Option<u64> {
    text.lines()
        .find_map(|line| line.strip_prefix(name))
        .and_then(|rest| rest.trim().strip_prefix("0x"))
        .and_then(|address| u64::from_str_radix(address, 16).ok())
}

/// The trained order of `binary` from logs of its runs under QEMU's user-mode
/// emulator with `-d page,in_asm`: the functions of the binary in the order the runs
/// first execute code in them, the runs in the order of `logs`.
///
/// The emulator translates each block of guest code when it first runs it and logs
/// the block's guest address then (`in_asm`); its `page` log gives where it loaded
/// the binary's code (`start_code` and `end_code`), from which the binary's own
/// addresses follow.
pub fn first_use(binary: &Path, logs: &[PathBuf]) -> Result<Vec<String>, String> {
    let functions = functions(binary)?;
    let (start, end) = executable_span(binary)?;
    let mut order = Vec::new();
    let mut seen = HashSet::new();
    for log in logs {
        let text =
            std::fs::read_to_string(log).map_err(|error| format!("{}: {error}", log.display()))?;
        let (Some(code), Some(code_end)) = (
            logged_address(&text, "start_code"),
            logged_address(&text, "end_code"),
        ) else {
            return Err(format!(
                "{}: no start_code and end_code; run with -d page,in_asm",
                log.display()
            ));
        };
        let bias = code.wrapping_sub(start);
        if code_end.wrapping_sub(bias) != end {
            return Err(format!(
                "{}: the logged code does not span {}'s executable segments",
                log.display(),
                binary.display()
            ));
        }
        for line in text.lines() {
            let Some(address) = line
                .strip_prefix("0x")
                .and_then(|rest| rest.split_once(':'))
                .and_then(|(address, _)| u64::from_str_radix(address, 16).ok())
            else {
                continue;
            };
            let address = address.wrapping_sub(bias);
            // The functions starting where the last one at or below the address
            // starts: one, or several names of the same code.
            let after = functions.partition_point(|function| function.start <= address);
            let Some(start) = after.checked_sub(1).map(|last| functions[last].start) else {
                continue;
            };
            let first = functions.partition_point(|function| function.start < start);
            for function in functions[first..after]
                .iter()
                .filter(|function| address < function.end)
            {
                if seen.insert(function.name.as_str()) {
                    order.push(function.name.clone());
                }
            }
        }
    }
    Ok(order)
}

/// The path of the item a function belongs to, from its demangled name.
fn owner(name: &str) -> String {
    for glue in ["core::ptr::drop_glue::", "core::ptr::drop_in_place::"] {
        if let Some(argument) = name.strip_prefix(glue).filter(|rest| rest.starts_with('<')) {
            let (dropped, _) = split_angle(argument);
            return format!("{}::{{drop}}", path_of(dropped));
        }
    }
    path_of(name)
}

/// The path a type or item name denotes: references, pointers, slices and trait
/// objects by what they point to, `<T as Trait>::item` by `T`'s path and the item,
/// and generic arguments left out.
fn path_of(name: &str) -> String {
    let mut name = name.trim();
    while let Some(rest) = ["&mut ", "&", "*const ", "*mut ", "dyn ", "[", "("]
        .iter()
        .find_map(|prefix| name.strip_prefix(prefix))
    {
        name = rest;
    }
    if name.starts_with('<') {
        let (inner, rest) = split_angle(name);
        return format!("{}{}", path_of(self_type(inner)), without_generics(rest));
    }
    let mut depth = 0usize;
    let end = name
        .char_indices()
        .find(|&(_, c)| {
            match c {
                '<' => depth += 1,
                '>' => depth = depth.saturating_sub(1),
                ';' | ',' | ')' | ']' | ' ' if depth == 0 => return true,
                _ => {}
            }
            false
        })
        .map_or(name.len(), |(index, _)| index);
    without_generics(&name[..end])
}

/// `<inner>rest` split at the `>` that closes the first `<`.
fn split_angle(name: &str) -> (&str, &str) {
    let mut depth = 0usize;
    for (index, c) in name.char_indices() {
        match c {
            '<' => depth += 1,
            '>' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return (&name[1..index], &name[index + 1..]);
                }
            }
            _ => {}
        }
    }
    (name.get(1..).unwrap_or_default(), "")
}

/// `T` of `T as Trait`, or the whole name of an inherent `<T>`.
fn self_type(inner: &str) -> &str {
    let mut depth = 0usize;
    for (index, c) in inner.char_indices() {
        match c {
            '<' | '(' | '[' => depth += 1,
            '>' | ')' | ']' => depth = depth.saturating_sub(1),
            ' ' if depth == 0 && inner[index..].starts_with(" as ") => return &inner[..index],
            _ => {}
        }
    }
    inner
}

/// `name` with every generic argument list left out.
fn without_generics(name: &str) -> String {
    let mut path = String::with_capacity(name.len());
    let mut depth = 0usize;
    for c in name.chars() {
        match c {
            '<' => depth += 1,
            '>' => depth = depth.saturating_sub(1),
            _ if depth == 0 => path.push(c),
            _ => {}
        }
    }
    while path.contains("::::") {
        path = path.replace("::::", "::");
    }
    path.trim_end_matches(':').to_owned()
}

#[cfg(test)]
mod tests {
    use super::{code_symbol, holds_enough, linker_made, owner, parse_trained, stable_name};

    #[test]
    fn a_trained_order_lists_each_function_once_and_may_hold_comments() {
        assert_eq!(
            parse_trained("# header\nstd::rt::lang_start\nplayback_worker::main\n").unwrap(),
            ["std::rt::lang_start", "playback_worker::main"]
        );
        for text in ["a\na\n", "a\n\nb\n", " a\n"] {
            assert!(parse_trained(text).is_err(), "{text:?}");
        }
    }

    #[test]
    fn a_promoted_local_function_keeps_its_name_without_the_build_suffix() {
        assert_eq!(
            stable_name("core::fmt::write (.llvm.1234567)"),
            "core::fmt::write"
        );
        assert_eq!(
            stable_name("core::fmt::write.llvm.1234567"),
            "core::fmt::write"
        );
        assert_eq!(
            stable_name("_Unwind_GetGR.constprop.0"),
            "_Unwind_GetGR.constprop.0"
        );
    }

    #[test]
    fn nm_lines_give_code_symbols_with_or_without_a_size() {
        let sized =
            code_symbol("0000000000012340 0000000000000024 T core::fmt::write (.llvm.1)").unwrap();
        assert_eq!((sized.address, sized.size), (0x12340, Some(0x24)));
        assert_eq!(sized.name, "core::fmt::write (.llvm.1)");
        let unsized_ = code_symbol("00012341 t $t").unwrap();
        assert_eq!((unsized_.address, unsized_.size), (0x12341, None));
        assert!(code_symbol("0000000000012340 0000000000000008 D data").is_none());
    }

    #[test]
    fn a_binary_must_hold_most_of_its_trained_order() {
        assert!(holds_enough(95, 100));
        assert!(!holds_enough(94, 100));
        assert!(holds_enough(0, 0));
    }

    #[test]
    fn code_the_linker_makes_for_the_layout_is_not_ordered() {
        for symbol in [
            "__CortexA53843419_C3008",
            "__ThumbV7PILongThunk_malloc",
            "__AArch64ADRPThunk_memcpy",
            "__ARMv7PILongThunk__RNvCs1_4core3fmt5write",
        ] {
            assert!(linker_made(symbol), "{symbol}");
        }
        for symbol in ["__aeabi_uidiv", "__libc_csu_init", "_RNvCs1_7example4main"] {
            assert!(!linker_made(symbol), "{symbol}");
        }
    }

    #[test]
    fn functions_belong_to_their_item_without_generic_arguments() {
        for (name, expected) in [
            (
                "playback_worker::ts_program::run",
                "playback_worker::ts_program::run",
            ),
            (
                "playback_worker::matroska_video::session::{closure#6}",
                "playback_worker::matroska_video::session::{closure#6}",
            ),
            (
                "gimli::read::line::parse_attribute::<gimli::read::EndianSlice<gimli::LittleEndian>>",
                "gimli::read::line::parse_attribute",
            ),
            (
                "<triergon_core::bytes::BudgetVec<triergon_isobmff::fragment::Trex>>::reallocate",
                "triergon_core::bytes::BudgetVec::reallocate",
            ),
            (
                "<triergon_isobmff::mov_cursor::PacketOccurrence as core::clone::Clone>::clone",
                "triergon_isobmff::mov_cursor::PacketOccurrence::clone",
            ),
            (
                "<triergon_playback::player::Player<triergon_playback::input::ByteSource>>::accept_event::<true>",
                "triergon_playback::player::Player::accept_event",
            ),
            (
                "core::ptr::drop_glue::<triergon_isobmff::encoded_stream::EncodedStreamJob>",
                "triergon_isobmff::encoded_stream::EncodedStreamJob::{drop}",
            ),
            (
                "core::ptr::drop_glue::<alloc::vec::Vec<addr2line::line::LineSequence>>",
                "alloc::vec::Vec::{drop}",
            ),
            (
                "<<std::sys::backtrace::BacktraceLock>::print::DisplayBacktrace as core::fmt::Display>::fmt",
                "std::sys::backtrace::BacktraceLock::print::DisplayBacktrace::fmt",
            ),
            ("<bool as core::fmt::Display>::fmt", "bool::fmt"),
            ("<&u8 as core::fmt::Debug>::fmt", "u8::fmt"),
            (
                "<[triergon_isobmff::mov_index::Entry]>::sort_unstable_by::<F>",
                "triergon_isobmff::mov_index::Entry::sort_unstable_by",
            ),
            ("__aeabi_uidiv", "__aeabi_uidiv"),
            ("_Unwind_GetGR.constprop.0", "_Unwind_GetGR.constprop.0"),
        ] {
            assert_eq!(owner(name), expected, "{name}");
        }
    }
}
