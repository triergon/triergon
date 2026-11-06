//! `cargo xtask codecs`: correctness grading and its summary of the codec crates against the pinned FFmpeg decoders.

pub mod correctness;
pub mod summary;

use std::path::{Path, PathBuf};

pub const USAGE: &str = "usage: cargo xtask codecs <command>

  manifest --out M.json LANE=DIR[,DIR...]...
                 hash every vector of each lane (h264, hevc, ac3, eac3, aac, dts)
  grade --manifest M.json --root R --bin DIR --ffmpeg FF --ffprobe FP --codec-worker W
        --reference R2 --out OUT.jsonl [--scratch DIR] [--only SUBSTR,...]
                 grade every vector by harness/codecs/numeric-rules.json; appends one row each
  summarize RUN.jsonl OUT.json
                 classify a grading run against the declared lane selection";

/// `--name value` and `--name=value` options and positional arguments, as
/// `argparse` accepts them; `switches` take no value.
pub struct Args {
    pub values: Vec<(String, String)>,
    pub positional: Vec<String>,
}

impl Args {
    pub fn parse(args: &[String], names: &[&str], switches: &[&str]) -> Result<Args, String> {
        let mut values = Vec::new();
        let mut positional = Vec::new();
        let mut rest = args.iter();
        while let Some(arg) = rest.next() {
            let Some(flag) = arg.strip_prefix("--") else {
                positional.push(arg.clone());
                continue;
            };
            let (name, inline) = match flag.split_once('=') {
                Some((name, value)) => (name, Some(value.to_owned())),
                None => (flag, None),
            };
            if switches.contains(&name) {
                values.push((name.to_owned(), String::new()));
                continue;
            }
            if !names.contains(&name) {
                return Err(format!("unrecognized argument: {arg}"));
            }
            let value = match inline {
                Some(value) => value,
                None => rest
                    .next()
                    .cloned()
                    .ok_or_else(|| format!("--{name} needs a value"))?,
            };
            values.push((name.to_owned(), value));
        }
        Ok(Args { values, positional })
    }

    /// The last value given for `name`.
    pub fn get(&self, name: &str) -> Option<&str> {
        self.values
            .iter()
            .rev()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }

    pub fn required(&self, name: &str) -> Result<&str, String> {
        self.get(name)
            .ok_or_else(|| format!("the following arguments are required: --{name}"))
    }

    pub fn path(&self, name: &str) -> Result<PathBuf, String> {
        self.required(name).map(PathBuf::from)
    }
}

/// Run one `codecs` command; the result is the process exit code.
pub fn main(root: &Path, args: &[String]) -> Result<i32, String> {
    let (command, rest) = args.split_first().ok_or(USAGE)?;
    match command.as_str() {
        "manifest" => {
            let a = Args::parse(rest, &["out"], &[])?;
            if a.positional.is_empty() {
                return Err(USAGE.to_owned());
            }
            correctness::manifest(&a.path("out")?, &a.positional)?;
        }
        "grade" => {
            let a = Args::parse(
                rest,
                &[
                    "manifest",
                    "root",
                    "bin",
                    "ffmpeg",
                    "ffprobe",
                    "codec-worker",
                    "reference",
                    "out",
                    "scratch",
                    "only",
                ],
                &[],
            )?;
            let grade = correctness::Grade {
                manifest: a.path("manifest")?,
                root: a.path("root")?,
                bin: a.path("bin")?,
                ffmpeg: a.required("ffmpeg")?.to_owned(),
                ffprobe: a.required("ffprobe")?.to_owned(),
                codec_worker: a.path("codec-worker")?,
                reference: a.required("reference")?.to_owned(),
                out: a.path("out")?,
                scratch: a.get("scratch").map(PathBuf::from),
                only: a.get("only").filter(|o| !o.is_empty()).map(str::to_owned),
                rules: correctness::Rules::load(root)?,
            };
            correctness::grade(&grade)?;
        }
        "summarize" => match rest {
            [run, out] => summary::summarize(run, Path::new(out))?,
            _ => return Err(USAGE.to_owned()),
        },
        _ => return Err(USAGE.to_owned()),
    }
    Ok(0)
}
