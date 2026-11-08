//! The pinned shaping reference: FriBidi 1.0.17 and HarfBuzz 14.6.0, built from
//! hash-checked release tarballs with recorded flags into `.research/harfbuzz`, as
//! the FreeType and FFmpeg references are. `cargo xtask build-workers` links the
//! reference worker (`harness/reference_shape.h`) against the installed libraries.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::fetch::Transport;
use crate::json::Value;
use crate::prepare::{PinnedRelease, output, run, unpack_release, verify_xz_tree};
use crate::sha256;

pub const CACHE: &str = ".research/harfbuzz";

const FRIBIDI: PinnedRelease<'static> = PinnedRelease {
    file: "fribidi-1.0.17.tar.xz",
    directory: "fribidi-1.0.17",
    url: "https://github.com/fribidi/fribidi/releases/download/v1.0.17/fribidi-1.0.17.tar.xz",
    sha256: "6949dcde27d41cebad1fd741fcafc36d55a1020d2d872d4a6eb3914caabbada2",
};
pub const HARFBUZZ: PinnedRelease<'static> = PinnedRelease {
    file: "harfbuzz-14.6.0.tar.xz",
    directory: "harfbuzz-14.6.0",
    url: "https://github.com/harfbuzz/harfbuzz/releases/download/14.6.0/harfbuzz-14.6.0.tar.xz",
    sha256: "d07a007327277708a2a73ae437887cdbaf282937f6d03ca5467723e9099af586",
};

/// HarfBuzz is compiled from its single-file amalgamation without optional
/// integrations (FreeType, GLib, ICU): libass uses only its core API. Like every
/// reference library it is built without floating-point contraction, so a fused
/// multiply-add never changes a rounding on any compiler or target.
const HARFBUZZ_FLAGS: [&str; 7] = [
    "-std=c++11",
    "-O2",
    "-ffp-contract=off",
    "-DNDEBUG",
    "-fno-exceptions",
    "-fno-rtti",
    "-c",
];

pub fn fribidi_argv(prefix: &Path) -> Vec<String> {
    vec![
        "./configure".into(),
        format!("--prefix={}", prefix.display()),
        "--disable-shared".into(),
        "--enable-static".into(),
        "--disable-docs".into(),
        "CFLAGS=-O2 -ffp-contract=off".into(),
    ]
}

/// What a C program linking the static HarfBuzz needs after it: the C++ runtime
/// and libm, which GNU ld does not add implicitly.
fn link_libraries() -> [&'static str; 2] {
    let cxx = if cfg!(target_os = "macos") {
        "-lc++"
    } else {
        "-lstdc++"
    };
    [cxx, "-lm"]
}

/// Runs `argv` as configure in `source` unless the recorded arguments match.
fn configure(source: &Path, argv: &[String], record: &Path) -> Result<Value, String> {
    let wanted = Value::strings(argv.iter().cloned());
    let configured = source.join("config.status").exists();
    if !configured || crate::checks::read_json(record).ok().as_ref() != Some(&wanted) {
        if configured {
            run(Command::new("make").arg("distclean").current_dir(source))?;
        }
        run(Command::new(&argv[0]).args(&argv[1..]).current_dir(source))?;
        crate::corpus::write_json(record, &wanted)?;
    }
    Ok(wanted)
}

/// Builds and installs FriBidi and HarfBuzz and records the build in
/// `.research/harfbuzz/build.json`.
pub fn prepare(root: &Path, jobs: u32, net: &dyn Transport) -> Result<PathBuf, String> {
    if !(1..=8).contains(&jobs) {
        return Err("jobs must be 1..8".into());
    }
    let cache = root.join(CACHE);
    let prefix = cache.join("install");
    let lib = prefix.join("lib");
    let pkgconfig = lib.join("pkgconfig");
    fs::create_dir_all(&pkgconfig).map_err(|e| format!("{}: {e}", pkgconfig.display()))?;
    let make = |dir: &Path, args: &[&str]| {
        run(Command::new("make")
            .arg(format!("-j{jobs}"))
            .args(args)
            .current_dir(dir))
    };

    // FriBidi: the library and its pkg-config file; the tools would need help2man.
    let fribidi = unpack_release(&cache, &FRIBIDI, net)?;
    let fribidi_argv = configure(
        &fribidi,
        &fribidi_argv(&prefix),
        &cache.join("fribidi-configure.json"),
    )?;
    make(&fribidi.join("lib"), &["install"])?;
    make(&fribidi, &["install-pkgconfigDATA"])?;

    // HarfBuzz from its amalgamation.
    let harfbuzz = unpack_release(&cache, &HARFBUZZ, net)?;
    let object = cache.join("harfbuzz.o");
    run(Command::new("c++")
        .args(HARFBUZZ_FLAGS)
        .arg(harfbuzz.join("src/harfbuzz.cc"))
        .arg("-o")
        .arg(&object))?;
    let archive = lib.join("libharfbuzz.a");
    let _ = fs::remove_file(&archive);
    run(Command::new("ar").arg("rcs").arg(&archive).arg(&object))?;
    let include = prefix.join("include/harfbuzz");
    fs::create_dir_all(&include).map_err(|e| format!("{}: {e}", include.display()))?;
    for entry in fs::read_dir(harfbuzz.join("src")).map_err(|e| e.to_string())? {
        let path = entry.map_err(|e| e.to_string())?.path();
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default();
        if name.starts_with("hb") && Path::new(name).extension().is_some_and(|e| e == "h") {
            fs::copy(&path, include.join(name)).map_err(|e| format!("{name}: {e}"))?;
        }
    }
    fs::write(
        pkgconfig.join("harfbuzz.pc"),
        format!(
            "prefix={}\nName: harfbuzz\nDescription: HarfBuzz amalgamation\nVersion: 14.6.0\n\
             Libs: -L${{prefix}}/lib -lharfbuzz\nLibs.private: {}\nCflags: -I${{prefix}}/include/harfbuzz\n",
            prefix.display(),
            link_libraries().join(" ")
        ),
    )
    .map_err(|e| e.to_string())?;
    for release in [&FRIBIDI, &HARFBUZZ] {
        verify_xz_tree(&cache.join(release.file), &cache)?;
    }

    let hash = |path: &Path| {
        fs::read(path)
            .map(|d| sha256::hex(&d))
            .map_err(|e| format!("{}: {e}", path.display()))
    };
    let releases = [&FRIBIDI, &HARFBUZZ].map(|r| {
        Value::object([
            ("file", Value::str(r.file)),
            ("sha256", Value::str(r.sha256)),
        ])
    });
    let libraries = ["libfribidi.a", "libharfbuzz.a"]
        .map(|name| Ok((name, Value::str(hash(&lib.join(name))?))));
    let libraries: Result<Vec<_>, String> = libraries.into_iter().collect();
    let manifest = Value::object([
        ("schema", Value::int(1)),
        ("releases", Value::Array(releases.into())),
        ("fribidi_configure_argv", fribidi_argv),
        ("harfbuzz_flags", Value::strings(HARFBUZZ_FLAGS)),
        ("harfbuzz_link_libraries", Value::strings(link_libraries())),
        ("libraries", Value::object(libraries?)),
        (
            "compiler",
            Value::str(output(Command::new("cc").arg("--version"))?),
        ),
    ]);
    let path = cache.join("build.json");
    crate::corpus::write_json(&path, &manifest)?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::{HARFBUZZ_FLAGS, fribidi_argv};
    use std::path::Path;

    #[test]
    fn the_references_compute_without_contraction() {
        assert!(HARFBUZZ_FLAGS.contains(&"-ffp-contract=off"));
        let argv = fribidi_argv(Path::new("/p"));
        assert!(argv.iter().any(|a| a.contains("-ffp-contract=off")));
        assert!(argv.contains(&"--disable-shared".to_owned()));
    }
}
