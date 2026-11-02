.PHONY: check reference fixtures adapters report qualify

check:
	CARGO_INCREMENTAL=0 cargo +nightly fmt --all --check
	CARGO_INCREMENTAL=0 cargo +nightly test --workspace --locked
	CARGO_INCREMENTAL=0 cargo +nightly clippy --workspace --all-targets --locked -- -D warnings
	CARGO_INCREMENTAL=0 cargo +nightly check --workspace --no-default-features --locked
	CARGO_INCREMENTAL=0 cargo +nightly build --workspace --release --locked
	python3 -m unittest harness.test_runner harness.test_timing harness.test_header harness.test_zlib harness.test_content harness.test_seek harness.test_checksum harness.test_comparison harness.test_reference_encodings harness.test_specs harness.test_colour
	python3 tools/check-safety.py
	python3 tools/check-design.py

reference:
	python3 tools/prepare-ffmpeg.py

fixtures:
	python3 tools/generate-corpus.py

adapters:
	python3 tools/build-harness.py

report:
	python3 tools/test-media.py --fate-cache .research/fate-samples

qualify:
	python3 tools/test-media.py --benchmark --memory --fate-cache .research/fate-samples --require-acceptance
