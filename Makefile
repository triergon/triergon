.PHONY: check check-simd prepare-check core-oracle reference fixtures adapters report qualify

export PATH := $(CURDIR)/.research/reference/install/bin:$(PATH)

prepare-check:
	$(MAKE) reference
	$(MAKE) fixtures
	python3 tools/fetch_required_vectors.py
	$(MAKE) adapters

check:
	CARGO_INCREMENTAL=0 cargo fmt --all --check
	CARGO_INCREMENTAL=0 cargo test --workspace --locked
	CARGO_INCREMENTAL=0 cargo clippy --workspace --all-targets --locked -- -D warnings
	CARGO_INCREMENTAL=0 cargo check --workspace --no-default-features --locked
	CARGO_INCREMENTAL=0 cargo build --workspace --release --locked
	$(MAKE) check-simd
	python3 -m unittest discover -s harness -t .
	python3 tools/check_safety.py
	python3 tools/check_design.py
	python3 tools/test-core-time.py

# Opt-in nightly std::simd lane of the HEVC decoder; it must decode exactly as the
# scalar default does (the conformance tests compare both with FFmpeg digests).
check-simd:
	CARGO_INCREMENTAL=0 cargo +nightly clippy -p triergon-codec-hevc -p triergon-sim --features triergon-codec-hevc/simd,triergon-sim/codecs --all-targets --locked -- -D warnings
	CARGO_INCREMENTAL=0 cargo +nightly test -p triergon-codec-hevc --features simd --release --locked

core-oracle:
	python3 tools/test-core-time.py

reference:
	python3 tools/prepare_ffmpeg.py

fixtures:
	python3 tools/generate_corpus.py
	python3 tools/generate_header_corpus.py
	python3 tools/generate_zlib_corpus.py
	python3 tools/generate_seek_corpus.py
	python3 tools/generate_colour_corpus.py
	python3 tools/generate_catalogue_corpus.py

adapters:
	python3 tools/build_harness.py

report:
	python3 tools/test_media.py --fate-cache .research/fate-samples

qualify:
	python3 tools/test_media.py --benchmark --memory --fate-cache .research/fate-samples --require-acceptance
