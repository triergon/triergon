//! The pinned reference builds and their cache.
//!
//! Every prepare command (`prepare-ffmpeg`, `fonts prepare`, `shape prepare`,
//! `raster prepare`, `ass prepare`) derives a [`Key`] before it builds: the SHA-256
//! of everything the build depends on besides its own directory. That is the code
//! that defines it (its pinned source hashes, configure flags and link steps are
//! constants there), the directory it is built in (its configure prefix), the keys
//! of the builds it links, the C and C++ compilers and their target, the
//! installed package list (FFmpeg's default configuration detects optional
//! system libraries) and the compiler and `pkg-config` environment variables. The
//! build records its key in `reference-key.json` beside its `build.json`.
//!
//! A prepare command whose directory already records the key does nothing. One
//! whose key is packed in `.research/reference-cache/NAME/KEY.tar.zst` unpacks that
//! pack instead of building: the directory is restored as the build left it, so
//! `build-workers` links the same libraries and records the same identity. Only
//! on a miss does it build. `cargo xtask references` lists the keys, packs current
//! builds into the cache, and moves packs to and from the evidence archive, under
//! `qualification/reference-builds/`, which expires 90 days after upload like
//! every uncited upload.

use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::json::{self, Value};
use crate::sha256;

type R<T> = Result<T, String>;

pub const USAGE: &str = "usage: cargo xtask references keys
       cargo xtask references pack [NAME]...
       cargo xtask references fetch [NAME]...
       cargo xtask references upload [NAME]...

  keys    print each reference build's name, key and state: current (its
          directory holds the build of this key), cached (a pack of it is in
          .research/reference-cache) or missing
  pack    pack each current build (all by default) into
          .research/reference-cache/NAME/KEY.tar.zst
  fetch   download the archived pack of each build that is neither current nor
          cached, where the archive holds one (TRIERGON_EVIDENCE_ARCHIVE, through
          the aws CLI and its credentials)
  upload  upload the pack of each current build that the archive lacks, the same way

A gate host holds no credentials: tools/gate-aws.sh fetches its packs through
presigned addresses and uploads them through the workstation, by `keys` and `pack`.

The prepare commands restore a build from its pack in the cache instead of
building it, and record the key of what they build.";

const KEY_SCHEMA: &str = "reference-build-key-v1";
/// The file a build records its key in, beside its `build.json`.
const RECORD: &str = "reference-key.json";
/// Where packs are kept locally, by build name and key.
const CACHE: &str = ".research/reference-cache";
/// Where packs are kept in the evidence archive. Under `qualification/`, a pack
/// expires 90 days after its upload; the next prepare after that builds and
/// uploads it again.
const ARCHIVE_PREFIX: &str = "qualification/reference-builds";

/// Code every build depends on: the shared download and unpacking code, this
/// cache, and the worker link policy, so that no build made under another policy
/// is ever taken.
const COMMON_RECIPE: [&str; 3] = [
    "xtask/src/prepare.rs",
    "xtask/src/references.rs",
    "xtask/src/harness/gate_build.rs",
];

/// One pinned reference build.
pub struct Build {
    pub name: &'static str,
    /// Its directory under the repository root.
    pub directory: &'static str,
    /// The code that defines it, beyond [`COMMON_RECIPE`].
    recipe: &'static [&'static str],
    /// The builds it links, by name.
    links: &'static [&'static str],
}

pub const BUILDS: [Build; 10] = [
    ffmpeg("ffmpeg-matroska", ".research/reference"),
    ffmpeg("ffmpeg-formats", ".research/reference-formats"),
    ffmpeg("ffmpeg-video", ".research/reference-video"),
    ffmpeg("ffmpeg-subtitles", ".research/reference-subtitles"),
    ffmpeg("ffmpeg-decode", ".research/reference-decode"),
    ffmpeg("ffmpeg-capture", ".research/reference-capture-static"),
    ffmpeg(
        "ffmpeg-capture-small",
        ".research/reference-capture-small-static",
    ),
    Build {
        name: "freetype",
        directory: ".research/freetype",
        recipe: &["xtask/src/fonts/mod.rs"],
        links: &[],
    },
    Build {
        name: "harfbuzz",
        directory: ".research/harfbuzz",
        recipe: &["xtask/src/shape/reference.rs"],
        links: &[],
    },
    Build {
        name: "libass",
        directory: ".research/libass",
        recipe: &["xtask/src/raster/reference.rs"],
        links: &["freetype", "harfbuzz"],
    },
];

const fn ffmpeg(name: &'static str, directory: &'static str) -> Build {
    Build {
        name,
        directory,
        recipe: &[],
        links: &[],
    }
}

/// The build named `name`.
pub fn build(name: &str) -> R<&'static Build> {
    BUILDS
        .iter()
        .find(|build| build.name == name)
        .ok_or_else(|| format!("{name}: no such reference build"))
}

/// The build that `prepare-ffmpeg --profile PROFILE` makes.
pub fn ffmpeg_build(profile: crate::prepare::Profile) -> &'static Build {
    BUILDS
        .iter()
        .find(|build| build.directory == profile.cache())
        .expect("every FFmpeg profile is registered")
}

/// What a build depends on, and its SHA-256.
pub struct Key {
    pub sha256: String,
    pub components: Value,
}

impl Build {
    /// The key of this build in `root` on this host.
    pub fn key(&self, root: &Path) -> R<Key> {
        let recipe = COMMON_RECIPE
            .iter()
            .chain(self.recipe)
            .map(|file| {
                let path = root.join(file);
                let sha =
                    sha256::file_hex(&path).map_err(|e| format!("{}: {e}", path.display()))?;
                Ok(((*file).to_owned(), Value::str(sha)))
            })
            .collect::<R<Vec<_>>>()?;
        let links = self
            .links
            .iter()
            .map(|name| {
                Ok((
                    (*name).to_owned(),
                    Value::str(build(name)?.key(root)?.sha256),
                ))
            })
            .collect::<R<Vec<_>>>()?;
        let environment = [
            "CC",
            "CXX",
            "CFLAGS",
            "CXXFLAGS",
            "CPPFLAGS",
            "LDFLAGS",
            "LIBS",
            "PKG_CONFIG_PATH",
            "PKG_CONFIG_LIBDIR",
        ]
        .into_iter()
        .filter_map(|name| Some((name, Value::str(std::env::var(name).ok()?))))
        .collect::<Vec<_>>();
        let components = Value::object([
            ("schema", Value::str(KEY_SCHEMA)),
            ("name", Value::str(self.name)),
            (
                "directory",
                Value::str(root.join(self.directory).display().to_string()),
            ),
            ("recipe", Value::Object(recipe)),
            ("links", Value::Object(links)),
            (
                "toolchain",
                Value::object([
                    ("cc", Value::str(output("cc", &["--version"]))),
                    ("cc_target", Value::str(output("cc", &["-dumpmachine"]))),
                    ("cxx", Value::str(output("c++", &["--version"]))),
                    ("machine", Value::str(output("uname", &["-m"]))),
                ]),
            ),
            ("packages", packages()),
            ("environment", Value::object(environment)),
        ]);
        Ok(Key {
            sha256: sha256::hex(json::canonical(&components).as_bytes()),
            components,
        })
    }

    fn path(&self, root: &Path) -> PathBuf {
        root.join(self.directory)
    }

    /// Whether the build's directory holds the build of `key`.
    fn holds(&self, root: &Path, key: &Key) -> bool {
        let dir = self.path(root);
        dir.join("build.json").is_file()
            && crate::checks::read_json(&dir.join(RECORD))
                .is_ok_and(|record| record.get("key").and_then(Value::as_str) == Some(&key.sha256))
    }

    fn pack_path(&self, root: &Path, key: &Key) -> PathBuf {
        root.join(CACHE)
            .join(self.name)
            .join(format!("{}.tar.zst", key.sha256))
    }

    fn archive_key(&self, key: &Key) -> String {
        format!("{ARCHIVE_PREFIX}/{}/{}.tar.zst", self.name, key.sha256)
    }

    /// Makes the build current: nothing when its directory already holds it, the
    /// cached pack when there is one, `make` otherwise. Returns what `make`
    /// returns, or the build's `build.json`.
    pub fn prepare(&self, root: &Path, make: impl FnOnce() -> R<String>) -> R<String> {
        let key = self.key(root)?;
        let dir = self.path(root);
        let built = dir.join("build.json").display().to_string();
        if self.holds(root, &key) {
            println!("{}: current ({})", self.name, key.sha256);
            return Ok(built);
        }
        let pack = self.pack_path(root, &key);
        if pack.is_file() {
            self.restore(root, &key, &pack)?;
            println!("{}: restored from {}", self.name, pack.display());
            return Ok(built);
        }
        // A miss builds from nothing: no configure state, objects or prefix of an
        // earlier build survives into this one, only the pinned source archives.
        self.clear(root)?;
        let made = make()?;
        let record = Value::object([
            ("key", Value::str(&key.sha256)),
            ("installed_sha256", Value::str(installed_digest(&dir)?)),
            ("components", key.components),
        ]);
        crate::corpus::write_json(&dir.join(RECORD), &record)?;
        Ok(made)
    }

    /// Removes everything in the build's directory but the downloaded source
    /// archives, whose hashes the build checks.
    fn clear(&self, root: &Path) -> R<()> {
        let dir = self.path(root);
        let Ok(entries) = fs::read_dir(&dir) else {
            return Ok(());
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().into_owned();
            let archive = [".tar.gz", ".tar.xz"].iter().any(|s| name.ends_with(s));
            let kind = entry.file_type().map_err(|e| e.to_string())?;
            if kind.is_dir() {
                fs::remove_dir_all(&path).map_err(|e| format!("{}: {e}", path.display()))?;
            } else if !(archive && kind.is_file()) {
                fs::remove_file(&path).map_err(|e| format!("{}: {e}", path.display()))?;
            }
        }
        Ok(())
    }

    /// The SHA-256 of the installed tree its record names, read again from disk.
    fn recorded_install(dir: &Path) -> Option<String> {
        crate::checks::read_json(&dir.join(RECORD))
            .ok()
            .and_then(|record| {
                record
                    .get("installed_sha256")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            })
    }

    /// Replaces the build's directory with the contents of `pack`, which must
    /// record `key` and hold the installed tree its record names.
    fn restore(&self, root: &Path, key: &Key, pack: &Path) -> R<()> {
        let dir = self.path(root);
        let staged = dir.with_extension("restoring");
        if staged.exists() {
            fs::remove_dir_all(&staged).map_err(|e| format!("{}: {e}", staged.display()))?;
        }
        fs::create_dir_all(&staged).map_err(|e| format!("{}: {e}", staged.display()))?;
        let mut unzstd = Command::new("zstd")
            .args(["-dcq"])
            .arg(pack)
            .stdout(Stdio::piped())
            .spawn()
            .map_err(|e| format!("zstd: {e}"))?;
        let tar = Command::new("tar")
            .arg("-xf")
            .arg("-")
            .arg("-C")
            .arg(&staged)
            .stdin(unzstd.stdout.take().expect("piped"))
            .status()
            .map_err(|e| format!("tar: {e}"))?;
        let zstd = unzstd.wait().map_err(|e| format!("zstd: {e}"))?;
        if !tar.success() || !zstd.success() {
            let _ = fs::remove_dir_all(&staged);
            return Err(format!("{}: cannot unpack", pack.display()));
        }
        let recorded = crate::checks::read_json(&staged.join(RECORD))
            .ok()
            .and_then(|record| record.get("key").and_then(Value::as_str).map(str::to_owned));
        if recorded.as_deref() != Some(&key.sha256) {
            let _ = fs::remove_dir_all(&staged);
            return Err(format!("{}: packs another build", pack.display()));
        }
        if Self::recorded_install(&staged) != Some(installed_digest(&staged)?) {
            let _ = fs::remove_dir_all(&staged);
            return Err(format!(
                "{}: its installed tree is not the one its build recorded",
                pack.display()
            ));
        }
        if dir.exists() {
            fs::remove_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        }
        fs::rename(&staged, &dir).map_err(|e| format!("{}: {e}", dir.display()))
    }

    /// Packs the current build into the cache, unless it is there already, and
    /// returns the pack. Only a build whose installed tree is still the one its
    /// build recorded is packed.
    fn pack(&self, root: &Path, key: &Key) -> R<PathBuf> {
        if !self.holds(root, key) {
            return Err(format!("{}: not built with key {}", self.name, key.sha256));
        }
        let dir = self.path(root);
        if Self::recorded_install(&dir) != Some(installed_digest(&dir)?) {
            return Err(format!(
                "{}: its installed tree is not the one its build recorded; prepare it again",
                self.name
            ));
        }
        let pack = self.pack_path(root, key);
        if pack.is_file() {
            return Ok(pack);
        }
        let parent = pack.parent().expect("under the cache");
        fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
        let partial = pack.with_extension("part");
        let out = File::create(&partial).map_err(|e| format!("{}: {e}", partial.display()))?;
        let mut tar = Command::new("tar")
            .arg("-cf")
            .arg("-")
            .arg("-C")
            .arg(self.path(root))
            .arg(".")
            .stdout(Stdio::piped())
            .spawn()
            .map_err(|e| format!("tar: {e}"))?;
        let zstd = Command::new("zstd")
            .args(["-q", "-T0", "-6"])
            .stdin(tar.stdout.take().expect("piped"))
            .stdout(out)
            .status()
            .map_err(|e| format!("zstd: {e}"))?;
        let tarred = tar.wait().map_err(|e| format!("tar: {e}"))?;
        if !tarred.success() || !zstd.success() {
            let _ = fs::remove_file(&partial);
            return Err(format!("{}: cannot pack", self.name));
        }
        fs::rename(&partial, &pack).map_err(|e| format!("{}: {e}", pack.display()))?;
        Ok(pack)
    }
}

/// The SHA-256 of a build's `build.json` and of every file and link under its
/// `install` directory, by relative path.
fn installed_digest(dir: &Path) -> R<String> {
    fn visit(dir: &Path, base: &Path, lines: &mut Vec<String>) -> R<()> {
        let Ok(entries) = fs::read_dir(dir) else {
            return Ok(());
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let relative = path
                .strip_prefix(base)
                .expect("under the base")
                .display()
                .to_string();
            let kind = fs::symlink_metadata(&path)
                .map_err(|e| format!("{}: {e}", path.display()))?
                .file_type();
            if kind.is_symlink() {
                let target =
                    fs::read_link(&path).map_err(|e| format!("{}: {e}", path.display()))?;
                lines.push(format!("link {}  {relative}", target.display()));
            } else if kind.is_dir() {
                visit(&path, base, lines)?;
            } else {
                let sha =
                    sha256::file_hex(&path).map_err(|e| format!("{}: {e}", path.display()))?;
                lines.push(format!("{sha}  {relative}"));
            }
        }
        Ok(())
    }
    let mut lines = Vec::new();
    let build = dir.join("build.json");
    let sha = sha256::file_hex(&build).map_err(|e| format!("{}: {e}", build.display()))?;
    lines.push(format!("{sha}  build.json"));
    visit(&dir.join("install"), dir, &mut lines)?;
    lines.sort();
    Ok(sha256::hex(lines.join("\n").as_bytes()))
}

/// The installed packages and their versions, where `dpkg` keeps the list.
fn packages() -> Value {
    let list = output(
        "dpkg-query",
        &["-W", "-f", "${Package} ${Version} ${Architecture}\\n"],
    );
    if list.is_empty() {
        return Value::Null;
    }
    let mut lines: Vec<&str> = list.lines().collect();
    lines.sort_unstable();
    Value::str(sha256::hex(lines.join("\n").as_bytes()))
}

fn output(program: &str, args: &[&str]) -> String {
    Command::new(program)
        .args(args)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
        .unwrap_or_default()
}

fn archive_location() -> R<String> {
    std::env::var("TRIERGON_EVIDENCE_ARCHIVE")
        .map(|archive| archive.trim_end_matches('/').to_owned())
        .map_err(|_| "set TRIERGON_EVIDENCE_ARCHIVE to the evidence archive location".into())
}

fn aws(args: &[&str]) -> R<bool> {
    Command::new("aws")
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|status| status.success())
        .map_err(|e| format!("aws: {e}"))
}

fn selected(names: &[String]) -> R<Vec<&'static Build>> {
    if names.is_empty() {
        Ok(BUILDS.iter().collect())
    } else {
        names.iter().map(|name| build(name)).collect()
    }
}

pub fn main(root: &Path, args: &[String]) -> R<()> {
    let (command, names) = args.split_first().ok_or(USAGE)?;
    if matches!(command.as_str(), "--help" | "-h") {
        println!("{USAGE}");
        return Ok(());
    }
    let builds = selected(names)?;
    match command.as_str() {
        "keys" => {
            for build in builds {
                let key = build.key(root)?;
                let state = if build.holds(root, &key) {
                    "current"
                } else if build.pack_path(root, &key).is_file() {
                    "cached"
                } else {
                    "missing"
                };
                println!("{} {} {state}", build.name, key.sha256);
            }
        }
        "pack" => {
            for build in builds {
                let key = build.key(root)?;
                if build.holds(root, &key) {
                    println!("{}", build.pack(root, &key)?.display());
                }
            }
        }
        "fetch" => {
            let archive = archive_location()?;
            for build in builds {
                let key = build.key(root)?;
                let pack = build.pack_path(root, &key);
                if build.holds(root, &key) || pack.is_file() {
                    continue;
                }
                let parent = pack.parent().expect("under the cache");
                fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
                let partial = pack.with_extension("part");
                let from = format!("{archive}/{}", build.archive_key(&key));
                if aws(&[
                    "s3",
                    "cp",
                    "--only-show-errors",
                    &from,
                    &partial.display().to_string(),
                ])? {
                    fs::rename(&partial, &pack).map_err(|e| e.to_string())?;
                    println!("{}: fetched {}", build.name, pack.display());
                } else {
                    let _ = fs::remove_file(&partial);
                    println!("{}: not archived", build.name);
                }
            }
        }
        "upload" => {
            let archive = archive_location()?;
            for build in builds {
                let key = build.key(root)?;
                if !build.holds(root, &key) {
                    continue;
                }
                let to = format!("{archive}/{}", build.archive_key(&key));
                if aws(&["s3", "ls", &to])? {
                    println!("{}: archived", build.name);
                    continue;
                }
                let pack = build.pack(root, &key)?;
                if !aws(&[
                    "s3",
                    "cp",
                    "--only-show-errors",
                    &pack.display().to_string(),
                    &to,
                ])? {
                    return Err(format!("aws s3 cp {} {to} failed", pack.display()));
                }
                println!("{}: uploaded {to}", build.name);
            }
        }
        _ => return Err(USAGE.into()),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repository(root: &Path) {
        for file in COMMON_RECIPE
            .iter()
            .chain(BUILDS.iter().flat_map(|b| b.recipe))
        {
            let path = root.join(file);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, file.as_bytes()).unwrap();
        }
    }

    #[test]
    fn every_ffmpeg_profile_has_a_build_and_names_are_unique() {
        use crate::prepare::Profile;
        for name in [
            "matroska",
            "formats",
            "video",
            "subtitles",
            "decode",
            "capture",
            "capture-small",
        ] {
            let build = ffmpeg_build(Profile::parse(name).unwrap());
            assert_eq!(build.name, format!("ffmpeg-{name}"));
        }
        for (i, build) in BUILDS.iter().enumerate() {
            assert!(BUILDS[..i].iter().all(|b| b.name != build.name));
            assert!(build.links.iter().all(|name| super::build(name).is_ok()));
        }
    }

    #[test]
    fn a_recipe_change_changes_the_key_of_every_build_that_links_it() {
        let root = crate::walk::scratch("reference-keys");
        repository(&root);
        let keys = |root: &Path| -> Vec<String> {
            BUILDS.iter().map(|b| b.key(root).unwrap().sha256).collect()
        };
        let before = keys(&root);
        assert_eq!(before, keys(&root));
        fs::write(root.join("xtask/src/fonts/mod.rs"), b"changed").unwrap();
        let after = keys(&root);
        let changed: Vec<&str> = BUILDS
            .iter()
            .zip(before.iter().zip(&after))
            .filter(|(_, (a, b))| a != b)
            .map(|(build, _)| build.name)
            .collect();
        assert_eq!(changed, ["freetype", "libass"]);
        fs::write(root.join("xtask/src/harness/gate_build.rs"), b"policy").unwrap();
        assert!(keys(&root).iter().zip(&after).all(|(a, b)| a != b));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_build_is_made_once_then_packed_and_restored_unchanged() {
        let root = crate::walk::scratch("reference-cache");
        repository(&root);
        let freetype = build("freetype").unwrap();
        let dir = root.join(freetype.directory);
        let make = || {
            fs::create_dir_all(dir.join("install/lib")).unwrap();
            fs::write(dir.join("install/lib/libfreetype.a"), b"archive").unwrap();
            fs::write(dir.join("build.json"), b"{}").unwrap();
            Ok("made".to_owned())
        };
        assert_eq!(freetype.prepare(&root, make).unwrap(), "made");
        let unused = || -> R<String> { panic!("built twice") };
        freetype.prepare(&root, unused).unwrap();
        let key = freetype.key(&root).unwrap();
        let pack = freetype.pack(&root, &key).unwrap();
        fs::remove_dir_all(&dir).unwrap();
        freetype.prepare(&root, unused).unwrap();
        assert_eq!(
            fs::read(dir.join("install/lib/libfreetype.a")).unwrap(),
            b"archive"
        );
        assert!(freetype.holds(&root, &key));
        // A pack of another build is refused and leaves the directory alone.
        fs::write(root.join("xtask/src/fonts/mod.rs"), b"changed").unwrap();
        let other = freetype.key(&root).unwrap();
        fs::create_dir_all(freetype.pack_path(&root, &other).parent().unwrap()).unwrap();
        fs::copy(&pack, freetype.pack_path(&root, &other)).unwrap();
        assert!(freetype.prepare(&root, unused).is_err());
        assert!(dir.join("install/lib/libfreetype.a").is_file());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_miss_builds_from_nothing_and_only_a_clean_tree_is_packed() {
        let root = crate::walk::scratch("reference-clean");
        repository(&root);
        let freetype = build("freetype").unwrap();
        let dir = root.join(freetype.directory);
        fs::create_dir_all(dir.join("freetype-2.15.0")).unwrap();
        fs::write(dir.join("freetype-2.15.0/config.mk"), b"stale").unwrap();
        fs::write(dir.join("configure-argv.json"), b"[]").unwrap();
        fs::write(dir.join("freetype-2.15.0.tar.xz"), b"pinned").unwrap();
        let make = || {
            assert!(!dir.join("freetype-2.15.0").exists());
            assert!(!dir.join("configure-argv.json").exists());
            assert!(dir.join("freetype-2.15.0.tar.xz").is_file());
            fs::create_dir_all(dir.join("install/lib")).unwrap();
            fs::write(dir.join("install/lib/libfreetype.a"), b"archive").unwrap();
            fs::write(dir.join("build.json"), b"{}").unwrap();
            Ok("made".to_owned())
        };
        freetype.prepare(&root, make).unwrap();
        let key = freetype.key(&root).unwrap();
        fs::write(dir.join("install/lib/libfreetype.a"), b"changed").unwrap();
        assert!(freetype.pack(&root, &key).is_err());
        fs::write(dir.join("install/lib/libfreetype.a"), b"archive").unwrap();
        assert!(freetype.pack(&root, &key).is_ok());
        fs::remove_dir_all(root).unwrap();
    }
}
