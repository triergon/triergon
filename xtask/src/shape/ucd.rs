//! `cargo xtask shape ucd`: the Unicode property tables of triergon-shape.
//!
//! The shaping tables follow HarfBuzz 14.6.0, which carries Unicode 18.0.0, with
//! the rules of HarfBuzz's own generators. The bidi tables are Unicode 14.0.0's,
//! the data that libass's FriBidi 1.0.17 resolves text with. Every table is
//! derived from hash-pinned files on unicode.org cached under
//! `.research/ucd/<version>/`.

mod bidi;
mod cmap;
mod emit;
mod harfbuzz;
mod language;
mod pack;
mod source;
mod syllabic;

use std::fs;
use std::path::Path;

use crate::fetch::{self, Transport};
use crate::sha256;

const USAGE: &str = "usage: cargo xtask shape ucd [--check]

  Fetch the hash-pinned Unicode Character Database files into .research/ucd and
  regenerate the tables in crates/shape/src/unicode, and the language tags in
  crates/shape/src/language from HarfBuzz's tree; --check regenerates them in
  memory and fails if the committed files differ.";

/// One file of a Unicode release, by its path under `Public/<version>/ucd/`.
struct File {
    path: &'static str,
    sha256: &'static str,
}

/// The files of one Unicode release that a reference's tables are built from.
struct Release {
    version: &'static str,
    files: &'static [File],
}

/// HarfBuzz 14.6.0: `hb-ucd-table.hh` (generated from the UCD XML; these are the
/// same properties in the text files), `hb-unicode-emoji-table.hh` and
/// `hb-ot-shaper-arabic-table.hh`, all Unicode 18.0.0.
const HARFBUZZ: Release = Release {
    version: "18.0.0",
    files: &[
        File {
            path: "UnicodeData.txt",
            sha256: "0736451de439ae7baf1425136617da495e09ee5afbe6e394374db7009ea08950",
        },
        File {
            path: "Scripts.txt",
            sha256: "0071fd81b6aeae25f6e8bce8efec3066a6476a91b49bdb2f52dc76e817862a6a",
        },
        File {
            path: "PropertyValueAliases.txt",
            sha256: "06c4c8eaf7b0bf34abe73b113da1215bd784ac254d4c223600b90267caa4bbbd",
        },
        File {
            path: "BidiMirroring.txt",
            sha256: "cd54810ebf52f0e61a730c8b9cb25975de6c85f6d788a559b416afd548923fd6",
        },
        File {
            path: "DerivedNormalizationProps.txt",
            sha256: "98ac7f67d985fe781e317f6182e885e94cabb0c314769e6dd73e48b226931ccd",
        },
        File {
            path: "emoji/emoji-data.txt",
            sha256: "80d00f8e616a0ef27fd6b8de3b758c06383b5d917e2977709578e68baf733bf1",
        },
        File {
            path: "ArabicShaping.txt",
            sha256: "8ccde4ebd070500a68e8bcb9d5514522fb0bebe57b9ebad3ff925b8e1e669f83",
        },
    ],
};

/// HarfBuzz 14.6.0's `hb-ot-shaper-indic-table.cc`, `hb-ot-shaper-use-table.hh` and
/// `hb-ot-shaper-vowel-constraints.cc`, from Unicode 18.0.0 except for
/// [`USE_IGNORABLES`].
const SYLLABIC: Release = Release {
    version: "18.0.0",
    files: &[
        File {
            path: "IndicSyllabicCategory.txt",
            sha256: "a2b3aacf6b3e7bad4ca351ef985d9543825e20280ff280c25f646d9bc4ce304c",
        },
        File {
            path: "IndicPositionalCategory.txt",
            sha256: "c9ad44d267c317ee0e1ec66cbd4d15033c2964f905d11fc8905915f9a5a9ae52",
        },
        File {
            path: "Blocks.txt",
            sha256: "a58f8d322f3c5e254f9f97b1cbf76a454a7e02de6ac35619a5e4f27aaabfd553",
        },
        File {
            path: "ArabicShaping.txt",
            sha256: "8ccde4ebd070500a68e8bcb9d5514522fb0bebe57b9ebad3ff925b8e1e669f83",
        },
        File {
            path: "UnicodeData.txt",
            sha256: "0736451de439ae7baf1425136617da495e09ee5afbe6e394374db7009ea08950",
        },
        File {
            path: "Scripts.txt",
            sha256: "0071fd81b6aeae25f6e8bce8efec3066a6476a91b49bdb2f52dc76e817862a6a",
        },
        File {
            path: "PropertyValueAliases.txt",
            sha256: "06c4c8eaf7b0bf34abe73b113da1215bd784ac254d4c223600b90267caa4bbbd",
        },
    ],
};

/// The Default_Ignorable_Code_Point property HarfBuzz 14.6.0's USE table was
/// generated with, from Unicode 17.0.0.
const USE_IGNORABLES: Release = Release {
    version: "17.0.0",
    files: &[File {
        path: "DerivedCoreProperties.txt",
        sha256: "24c7fed1195c482faaefd5c1e7eb821c5ee1fb6de07ecdbaa64b56a99da22c08",
    }],
};

/// The bidi tables, from Unicode 14.0.0.
const BIDI: Release = Release {
    version: "14.0.0",
    files: &[
        File {
            path: "extracted/DerivedBidiClass.txt",
            sha256: "e5fe51acc74e3e83b4fb4c7b25f3c34491d6eb8095c9955d0712dafbca7b3c2b",
        },
        File {
            path: "UnicodeData.txt",
            sha256: "36018e68657fdcb3485f636630ffe8c8532e01c977703d2803f5b89d6c5feafb",
        },
        File {
            path: "BidiBrackets.txt",
            sha256: "24a74555f918bbe99f5b3f1b83cf36fc0e205bb8a600a6a3aa539c710a3dcf27",
        },
        File {
            path: "BidiMirroring.txt",
            sha256: "7a5c74cedc1616a9af0a9d22e108ae592d86fe93649c144ae6ba49f193a44122",
        },
        File {
            path: "ArabicShaping.txt",
            sha256: "c7698811e9adb6cc98fb996a7de4be2b6532f2ac67e76055cc8afdbf6ee18af3",
        },
    ],
};

/// Unicode 14.0.0's bidi conformance tests, which `cargo xtask shape conformance`
/// runs the bidi tables and resolver through.
const BIDI_TESTS: Release = Release {
    version: "14.0.0",
    files: &[
        File {
            path: "BidiTest.txt",
            sha256: "306a86c1c29458531e56c29ffde6badede65897275a5941ac7fc6985d6b0994f",
        },
        File {
            path: "BidiCharacterTest.txt",
            sha256: "7739891361250475fe55cfc7eeee7024ba2626bdce8c7db02bf7ae9d1bcdfb3f",
        },
    ],
};

/// The largest file, BidiTest.txt, is under 8 MiB.
const FILE_LIMIT: usize = 8 << 20;

const OUTPUT: &str = "crates/shape/src";

pub fn main(root: &Path, args: &[String]) -> Result<(), String> {
    let check = match args {
        [] => false,
        [flag] if flag == "--check" => true,
        _ => return Err(USAGE.to_owned()),
    };
    println!("{}", run(root, check, &fetch::Network)?);
    Ok(())
}

/// `cargo xtask shape conformance`: fetch the pinned conformance tests and run the
/// `bidi_conformance` example on them, which fails unless every case passes.
pub fn conformance(root: &Path, args: &[String]) -> Result<(), String> {
    if !args.is_empty() {
        return Err(super::USAGE.to_owned());
    }
    Loaded::fetch(root, &BIDI_TESTS, &fetch::Network)?;
    let cache = root.join(".research/ucd").join(BIDI_TESTS.version);
    let paths: Vec<String> = BIDI_TESTS
        .files
        .iter()
        .map(|file| cache.join(file.path).display().to_string())
        .collect();
    let mut args = vec![
        "run",
        "--release",
        "--locked",
        "-p",
        "triergon-shape",
        "--example",
        "bidi_conformance",
        "--",
    ];
    args.extend(paths.iter().map(String::as_str));
    crate::cargo(root, None, &args)
}

fn run(root: &Path, check: bool, net: &dyn Transport) -> Result<String, String> {
    let harfbuzz = Loaded::fetch(root, &HARFBUZZ, net)?;
    let bidi = Loaded::fetch(root, &BIDI, net)?;
    let shapers = Loaded::fetch(root, &SYLLABIC, net)?;
    let ignorables = Loaded::fetch(root, &USE_IGNORABLES, net)?;
    let tree = syllabic::TreeFiles::load(root)?;
    let languages = language::load(root)?;
    let remaps = cmap::load(root)?;
    let outputs = [
        ("unicode/tables.rs", harfbuzz::generate(&harfbuzz)?),
        ("unicode/bidi/tables.rs", bidi::generate(&bidi)?),
        (
            "unicode/syllabic_tables.rs",
            syllabic::generate(&shapers, &ignorables, &tree)?,
        ),
        ("language/tables.rs", language::generate(&languages)?),
        ("cmap/tables.rs", cmap::generate(&remaps)?),
    ];
    let mut stale = Vec::new();
    for (name, generated) in &outputs {
        let path = root.join(OUTPUT).join(name);
        if fs::read(&path).ok().as_deref() == Some(generated.text.as_bytes()) {
            continue;
        }
        if check {
            stale.push(format!("{OUTPUT}/{name}"));
        } else {
            fetch::publish(&path, generated.text.as_bytes())?;
        }
    }
    if !stale.is_empty() {
        return Err(format!(
            "{} differ from what `cargo xtask shape ucd` generates",
            stale.join(", ")
        ));
    }
    Ok(outputs
        .iter()
        .map(|(name, generated)| {
            format!(
                "{OUTPUT}/{name}: {} bytes of source, {} bytes of static data",
                generated.text.len(),
                generated.data_bytes
            )
        })
        .collect::<Vec<_>>()
        .join("\n"))
}

/// A generated table file.
struct Generated {
    text: String,
    /// The size of the static data it defines.
    data_bytes: usize,
}

/// The verified files of one release.
struct Loaded {
    release: &'static Release,
    texts: Vec<String>,
}

impl Loaded {
    /// Read every file of `release` from the cache, fetching what is missing; each
    /// must match its pinned hash, and a download is verified before it is cached.
    fn fetch(root: &Path, release: &'static Release, net: &dyn Transport) -> Result<Self, String> {
        let cache = root.join(".research/ucd").join(release.version);
        let mut texts = Vec::new();
        for file in release.files {
            let path = cache.join(file.path);
            let data = if path.exists() {
                let data = fetch::read_bounded(&path, FILE_LIMIT)?;
                if data.len() > FILE_LIMIT || sha256::hex(&data) != file.sha256 {
                    return Err(format!("{}: cached file changed", path.display()));
                }
                data
            } else {
                let url = release.url(file);
                let data = net.https(&url, "triergon-unicode-data", FILE_LIMIT)?;
                if data.len() > FILE_LIMIT {
                    return Err(format!("{url}: exceeds the download bound"));
                }
                if sha256::hex(&data) != file.sha256 {
                    return Err(format!("{url}: hash mismatch"));
                }
                let parent = path.parent().ok_or("cache path without a parent")?;
                fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
                fetch::publish(&path, &data)?;
                data
            };
            texts.push(String::from_utf8(data).map_err(|_| format!("{}: not UTF-8", file.path))?);
        }
        Ok(Self { release, texts })
    }

    /// The text of the file at `path`, one of the release's files.
    fn text(&self, path: &str) -> &str {
        let index = self
            .release
            .files
            .iter()
            .position(|file| file.path == path)
            .unwrap_or_else(|| panic!("{path} is not a file of Unicode {}", self.release.version));
        &self.texts[index]
    }

    /// The `@generated` header naming the command and every source with its hash.
    fn header(&self, what: &str) -> String {
        let mut header = format!(
            "// @generated by `cargo xtask shape ucd`; do not edit.\n\
             // {what}, from Unicode {}:\n",
            self.release.version
        );
        for file in self.release.files {
            header.push_str(&format!(
                "//   {}\n//     sha256 {}\n",
                self.release.url(file),
                file.sha256
            ));
        }
        header
    }
}

impl Release {
    fn url(&self, file: &File) -> String {
        format!(
            "https://www.unicode.org/Public/{}/ucd/{}",
            self.version, file.path
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    /// Serves `body` for every request and records the URLs asked for.
    struct Fake {
        body: Vec<u8>,
        calls: RefCell<Vec<String>>,
    }

    impl Transport for Fake {
        fn https(&self, url: &str, _: &str, _: usize) -> Result<Vec<u8>, String> {
            self.calls.borrow_mut().push(url.to_owned());
            Ok(self.body.clone())
        }

        fn rsync(&self, _: &str, _: &Path, _: usize) -> Result<(), String> {
            Err("not used".into())
        }

        fn github_asset(&self, _: &str, _: u64, _: &str, _: usize) -> Result<Vec<u8>, String> {
            Err("not used".into())
        }
    }

    const TINY: Release = Release {
        version: "0.0.0",
        files: &[File {
            path: "emoji/x.txt",
            // SHA-256 of "x".
            sha256: "2d711642b726b04401627ca9fbac32f5c8530fb1903cc4db02258717921a4881",
        }],
    };

    #[test]
    fn downloads_are_verified_before_they_are_cached() {
        let root = crate::walk::scratch("ucd");
        let wrong = Fake {
            body: b"y".to_vec(),
            calls: RefCell::default(),
        };
        let error = Loaded::fetch(&root, &TINY, &wrong).err().unwrap();
        assert!(error.ends_with("hash mismatch"), "{error}");
        assert!(!root.join(".research/ucd/0.0.0/emoji/x.txt").exists());

        let right = Fake {
            body: b"x".to_vec(),
            calls: RefCell::default(),
        };
        let loaded = Loaded::fetch(&root, &TINY, &right).unwrap();
        assert_eq!(loaded.text("emoji/x.txt"), "x");
        assert_eq!(
            *right.calls.borrow(),
            ["https://www.unicode.org/Public/0.0.0/ucd/emoji/x.txt"]
        );
        Loaded::fetch(&root, &TINY, &right).unwrap();
        assert_eq!(right.calls.borrow().len(), 1, "the cache is used");

        fs::write(root.join(".research/ucd/0.0.0/emoji/x.txt"), b"z").unwrap();
        let error = Loaded::fetch(&root, &TINY, &right).err().unwrap();
        assert!(error.ends_with("cached file changed"), "{error}");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn header_names_every_source_and_hash() {
        let loaded = Loaded {
            release: &TINY,
            texts: vec!["x".into()],
        };
        assert_eq!(
            loaded.header("Things"),
            "// @generated by `cargo xtask shape ucd`; do not edit.\n\
             // Things, from Unicode 0.0.0:\n\
             //   https://www.unicode.org/Public/0.0.0/ucd/emoji/x.txt\n\
             //     sha256 2d711642b726b04401627ca9fbac32f5c8530fb1903cc4db02258717921a4881\n"
        );
    }
}
