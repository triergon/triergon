"""Author the text, bitmap and caption subtitle bridge manifests.

Run from the repository root with the subtitle decoder corpora prepared in
.research/subtitle-corpus and the pinned subtitles ffprobe installed. The
default invocation verifies the committed manifest rows. Use --output to
write unpinned manifests for the playback-views recorder.
"""

import argparse
import concurrent.futures
import hashlib
import json
from pathlib import Path
import subprocess

GROUPS = ("bridge-text-subtitles", "bridge-bitmap-subtitles", "bridge-captions-teletext")
features = Path("harness/features")
parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("--output", type=Path, help="write unpinned manifests under this directory")
args = parser.parse_args()
fixture_root = Path("crates/playback/tests/fixtures")
prepared = Path(".research/subtitle-corpus")
tool = Path(".research/reference-subtitles/install/bin/ffprobe")
carriers = {".ts": "ts", ".m2ts": "m2ts", ".mkv": "mkv", ".mp4": "mov", ".mov": "mov"}
corpora = {group: [] for group in GROUPS}


def corpus(source):
    return json.loads((features / source / "corpus.json").read_text())["cases"]


def entry(group, case_id, path, bridge, codec, options=(), ordinal=0, from_case=None):
    container = carriers[path.suffix]
    raw = path.read_bytes()
    sha = hashlib.sha256(raw).hexdigest()
    if container == "ts" and len(raw) >= 389 and all(raw[4 + 192 * i] == 0x47 for i in range(3)):
        container = "m2ts"
    if from_case and sha != from_case["sha256"]:
        raise ValueError(f"{path}: source hash mismatch")
    rows = corpora[group]
    args = [bridge, container, codec, str(ordinal), "0" if container in ("mkv", "mov") else "1", *options]
    if any(c["file"] == str(path) and c["arguments"] == args for c in rows):
        return
    if any(c["id"] == case_id for c in rows):
        raise ValueError(f"duplicate case ID: {case_id}")
    rows.append(dict(id=case_id, profile=f"{bridge}-{container}-{codec}", file=str(path), sha256=sha,
                     rights=from_case["rights"] if from_case else "authored-synthetic",
                     source=from_case.get("source") if from_case else str(path), arguments=args))

text, bitmap, captions = GROUPS
for case in corpus("text-extraction"):
    entry(text, case["id"], Path(case["file"]), "text", case["arguments"][1], from_case=case)
for name in ("fragmented-subtitle-edit.mp4", "fragmented-subtitles.mp4", "r-subtitle-tx3g.mov"):
    options = ["opt:progressive=1"] if name.startswith("fragmented-") else []
    entry(text, Path(name).stem, fixture_root / "mp4" / name, "text", "tx3g", options)

for source, codec in (("subtitle-pgs", "pgs"), ("subtitle-dvb", "dvb"), ("subtitle-dvd", "dvd")):
    for case in corpus(source):
        path = prepared / case["file"]
        if path.suffix not in carriers:
            continue
        opts = [o for o in case.get("arguments", [])[1:] if o.startswith(("opt:forced_subs_only=", "opt:dvb_substream="))]
        # Matroska DVB and DVD expose their packet configuration directly.
        # Decoder-only knobs of the receiver corpus are outside this bridge.
        if path.suffix == ".mkv" and codec == "dvb":
            opts = []
        ordinal = next((int(o.partition("=")[2]) for o in case.get("arguments", []) if o.startswith("stream=")), 0)
        bridge = {"pgs": "pgs", "dvd": "dvd", "dvb": "mkv-bitmap"}[codec]
        if path.suffix in (".ts", ".m2ts"):
            bridge = f"ts-{codec}"
        if path.suffix in (".mov", ".mp4"):
            bridge = "mp4-dvd"
        entry(bitmap, f"{codec}-{case['id']}", path, bridge, codec, opts, ordinal, case)

for case in corpus("dvb-source"):
    path = Path(case["file"])
    bridge = "mkv-bitmap" if path.suffix == ".mkv" else "ts-dvb"
    entry(bitmap, f"dvb-source-{case['id']}", path, bridge, "dvb", from_case=case)
    if path.name in ("combined-services.ts", "separate-descriptors.ts"):
        for n in range(2):
            opts = [f"opt:dvb_substream={n}"] if path.name.startswith("combined") else [f"opt:descriptor={n}", "opt:dvb_substream=0"]
            entry(bitmap, f"dvb-source-{case['id']}-service-{n}", path, bridge, "dvb", opts, from_case=case)

entry(bitmap, "fixture-pgs-basic-m2ts", fixture_root / "subtitles/pgs-basic.m2ts", "ts-pgs", "pgs")
entry(bitmap, "fixture-dvd-commands", fixture_root / "subtitles/dvd-commands.mp4", "mp4-dvd", "dvd")
for path in sorted((fixture_root / "matroska-subtitle-probe").glob("*.mkv")) + [Path("harness/fixtures/playback/avc-pgs-early-canvas.mkv")]:
    entry(bitmap, f"probe-{path.stem}", path, "mkv-bitmap-probe", "pgs")
    entry(bitmap, f"mkv-bitmap-{path.stem}", path, "mkv-bitmap", "pgs")
entry(bitmap, "probe-observed-relative-live", fixture_root / "matroska-subtitle-probe/observed-relative.mkv",
      "mkv-bitmap-probe", "pgs", ["opt:probe_packets=2"])

for source, codec in (("subtitle-cc", "cc"), ("subtitle-teletext", "teletext")):
    for case in corpus(source):
        path = prepared / case["file"]
        if path.suffix not in carriers:
            continue
        opts = [o for o in case.get("arguments", [])[1:] if o.startswith("opt:") and not o.startswith("opt:txt_format=")]
        ordinal = next((int(o.partition("=")[2]) for o in case.get("arguments", []) if o.startswith("stream=")), 0)
        entry(captions, f"{codec}-{case['id']}", path, codec, codec, opts, ordinal, case)
entry(captions, "fixture-cc-h264-a53", fixture_root / "subtitles/cc-h264-a53.mp4", "cc", "cc")
entry(captions, "fixture-teletext-basic", fixture_root / "subtitles/teletext-basic.ts", "teletext", "teletext")

ts_paths = sorted({row["file"] for cs in corpora.values() for row in cs if row["arguments"][1] in ("ts", "m2ts")})


def probe(path):
    # Damaged fixtures can have no readable program. Preserve the fallback
    # selector on those inputs so the bridge still compares the refusal.
    p = subprocess.run([tool, "-v", "error", "-show_programs", "-show_entries",
                        "program=program_num:stream=index,codec_name", "-of", "json", path],
                       capture_output=True, timeout=15, check=False)
    return path, json.loads(p.stdout or b"{}").get("programs", [])

with concurrent.futures.ThreadPoolExecutor(max_workers=8) as pool:
    programs = dict(pool.map(probe, ts_paths))

matches = {"pgs": ("hdmv_pgs_subtitle",), "dvb": ("dvb_subtitle",), "teletext": ("dvb_teletext",),
           "cc": ("h264", "hevc", "mpeg1video", "mpeg2video")}
for rows in corpora.values():
    for row in rows:
        bridge, carrier, codec, ordinal, program, *opts = row["arguments"]
        if carrier not in ("ts", "m2ts"):
            continue
        available = [p for p in programs.get(row["file"], []) if any(s.get("codec_name") in matches[codec] for s in p["streams"])]
        if available:
            row["arguments"][4] = str(available[0]["program_num"])


def unique_operations(cases):
    """Keep the first input and invocation for each bridge operation."""
    seen = set()
    for case in cases:
        key = case["sha256"], tuple(case["arguments"])
        if key not in seen:
            seen.add(key)
            yield case


for group, rows in corpora.items():
    cases = list(unique_operations(rows))
    manifest = dict(schema=1, directory=".", cases=cases)
    if args.output:
        folder = args.output / group
        folder.mkdir(parents=True, exist_ok=True)
        (folder / "corpus.json").write_text(json.dumps(manifest, indent=2) + "\n")
    else:
        committed = json.loads((features / group / "corpus.json").read_text())
        committed_cases = committed["cases"]
        # The reference recorder owns the expected result fields. Check every
        # generated field independently of those pins.
        fields = ("id", "profile", "file", "sha256", "rights", "source", "arguments")
        actual = [{field: row.get(field) for field in fields} for row in committed_cases]
        if cases != actual:
            actual_by_id = {row["id"]: row for row in actual}
            generated_by_id = {row["id"]: row for row in cases}
            missing = sorted(actual_by_id.keys() - generated_by_id.keys())
            extra = sorted(generated_by_id.keys() - actual_by_id.keys())
            changed = sorted(
                case_id for case_id in actual_by_id.keys() & generated_by_id.keys()
                if actual_by_id[case_id] != generated_by_id[case_id]
            )
            raise ValueError(f"{group}: missing {missing}, extra {extra}, changed {changed}; "
                             "case order must also match")
    print(f"{group}: {len(cases)} cases")
