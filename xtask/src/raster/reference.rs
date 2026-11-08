//! The pinned rasterising reference: libass 0.17.5, built from its hash-checked
//! release tarball with recorded configuration into `.research/libass`, against the
//! FreeType build of `cargo xtask fonts prepare` and the FriBidi and HarfBuzz builds
//! of `cargo xtask shape prepare`, so each library has one build. No system font
//! provider is compiled in.
//!
//! The reference worker (`harness/reference_raster.h`) reaches libass's internal
//! outline, stroker, rasteriser, blur and composition stages through its private
//! headers in the unpacked source tree, compiled with the flags libass itself was
//! compiled with; `libass-internal.pc` records those flags for `cargo xtask
//! build-workers`.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::fetch::{Source, Transport};
use crate::json::Value;
use crate::prepare::{PinnedRelease, output, run, unpack_release, verify_xz_tree};
use crate::sha256;

pub const CACHE: &str = ".research/libass";

const LIBASS: PinnedRelease<'static> = PinnedRelease {
    file: "libass-0.17.5.tar.xz",
    directory: "libass-0.17.5",
    url: "https://github.com/libass/libass/releases/download/0.17.5/libass-0.17.5.tar.xz",
    mirrors: &[Source::GitHubAsset {
        repo: "libass/libass",
        id: 456_215_356,
    }],
    sha256: "2dca25c0e0c837ddf00b52011b3f82cac1e4ddd3ad018227806b0c2288864acc",
};

/// The builds libass links, each with the pkg-config files it installs.
const DEPENDENCIES: [(&str, &str, &[&str]); 2] = [
    (
        ".research/freetype",
        "cargo xtask fonts prepare",
        &["freetype2.pc"],
    ),
    (
        ".research/harfbuzz",
        "cargo xtask shape prepare",
        &["harfbuzz.pc", "fribidi.pc"],
    ),
];

pub fn configure_argv(prefix: &Path) -> Vec<String> {
    vec![
        "./configure".into(),
        format!("--prefix={}", prefix.display()),
        "--disable-shared".into(),
        "--enable-static".into(),
        "--disable-fontconfig".into(),
        "--disable-coretext".into(),
        "--disable-directwrite".into(),
        "--disable-libunibreak".into(),
        "--disable-require-system-font-provider".into(),
        // Like every reference library, without floating-point contraction.
        "CFLAGS=-O2 -ffp-contract=off".into(),
    ]
}

/// Unpacks libass and configures it with `argv` unless the recorded arguments
/// match. A changed configuration starts from a fresh copy of the pinned source.
fn configured(
    cache: &Path,
    argv: &[String],
    pkgconfig: &Path,
    net: &dyn Transport,
) -> Result<(PathBuf, Value), String> {
    let wanted = Value::strings(argv.iter().cloned());
    let record = cache.join("configure-argv.json");
    let current = crate::checks::read_json(&record).ok();
    let source = cache.join(LIBASS.directory);
    if current.as_ref() != Some(&wanted) && source.exists() {
        fs::remove_dir_all(&source).map_err(|e| format!("{}: {e}", source.display()))?;
    }
    let source = unpack_release(cache, &LIBASS, net)?;
    if !source.join("config.status").exists() || current.as_ref() != Some(&wanted) {
        run(Command::new(&argv[0])
            .args(&argv[1..])
            .env("PKG_CONFIG_LIBDIR", pkgconfig)
            .current_dir(&source))?;
        crate::corpus::write_json(&record, &wanted)?;
    }
    Ok((source, wanted))
}

/// The flags libass compiled its own sources with, from its Makefile: options and
/// definitions, without warnings and include paths, which the worker build sets.
fn library_cflags(source: &Path) -> Result<Vec<String>, String> {
    let makefile = source.join("Makefile");
    let text = fs::read_to_string(&makefile).map_err(|e| format!("{}: {e}", makefile.display()))?;
    let mut flags = Vec::new();
    for variable in ["CFLAGS = ", "AM_CFLAGS = ", "DEFS = "] {
        let line = text
            .lines()
            .find_map(|l| l.strip_prefix(variable))
            .ok_or_else(|| format!("libass Makefile without {}", variable.trim_end()))?;
        flags.extend(
            line.split_whitespace()
                .filter(|f| !f.starts_with("-W") && !f.starts_with("-I"))
                .map(str::to_owned),
        );
    }
    Ok(flags)
}

/// Builds and installs libass and records the build in `.research/libass/build.json`.
pub fn prepare(root: &Path, jobs: u32, net: &dyn Transport) -> Result<PathBuf, String> {
    if !(1..=8).contains(&jobs) {
        return Err("jobs must be 1..8".into());
    }
    let cache = root.join(CACHE);
    let prefix = cache.join("install");
    let pkgconfig = prefix.join("lib/pkgconfig");
    fs::create_dir_all(&pkgconfig).map_err(|e| format!("{}: {e}", pkgconfig.display()))?;
    // Static linking resolves libass's requirements from this one directory.
    let mut dependencies = Vec::new();
    for (profile, command, files) in DEPENDENCIES {
        let build = root.join(profile).join("build.json");
        let recorded = crate::checks::read_json(&build)
            .map_err(|_| format!("{profile}: not built; run `{command}` first"))?;
        for file in files {
            let from = root.join(profile).join("install/lib/pkgconfig").join(file);
            fs::copy(&from, pkgconfig.join(file))
                .map_err(|e| format!("{}: {e}", from.display()))?;
        }
        dependencies.push((profile.trim_start_matches(".research/"), recorded));
    }
    let (source, argv) = configured(&cache, &configure_argv(&prefix), &pkgconfig, net)?;
    for target in [&[][..], &["install"][..]] {
        run(Command::new("make")
            .arg(format!("-j{jobs}"))
            .args(target)
            .current_dir(&source))?;
    }
    verify_xz_tree(&cache.join(LIBASS.file), &cache)?;

    // The private headers and flags of the internal stages the worker calls.
    let cflags = library_cflags(&source)?;
    let tree = source.display();
    fs::write(
        pkgconfig.join("libass-internal.pc"),
        format!(
            "Name: libass-internal\nDescription: libass with its private headers\nVersion: 0.17.5\n\
             Requires: libass\nCflags: -isystem {tree} -isystem {tree}/libass {}\n",
            cflags.join(" ")
        ),
    )
    .map_err(|e| e.to_string())?;

    let library = prefix.join("lib/libass.a");
    let library = fs::read(&library).map_err(|e| format!("{}: {e}", library.display()))?;
    let manifest = Value::object([
        ("schema", Value::int(1)),
        (
            "release",
            Value::object([
                ("file", Value::str(LIBASS.file)),
                ("sha256", Value::str(LIBASS.sha256)),
            ]),
        ),
        ("configure_argv", argv),
        ("internal_cflags", Value::strings(cflags)),
        // The bitmap engine the reference worker selects for the gated features
        // (`ass_bitmap_engine_init(ASS_CPU_FLAG_NONE)`); `raster-simd-` operations
        // keep the one `ass_renderer_init` chooses for the host.
        (
            "engine",
            Value::object([
                ("name", Value::str("c")),
                ("tile_order", Value::int(4)),
                ("align_order", Value::int(4)),
            ]),
        ),
        ("library_sha256", Value::str(sha256::hex(&library))),
        ("dependencies", Value::object(dependencies)),
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
    use super::configure_argv;
    use std::path::Path;

    #[test]
    fn libass_builds_without_a_system_font_provider_or_contraction() {
        let argv = configure_argv(Path::new("/p"));
        assert!(argv.iter().any(|a| a.contains("-ffp-contract=off")));
        for flag in [
            "fontconfig",
            "coretext",
            "directwrite",
            "libunibreak",
            "require-system-font-provider",
        ] {
            assert!(argv.contains(&format!("--disable-{flag}")), "{flag}");
        }
    }
}
