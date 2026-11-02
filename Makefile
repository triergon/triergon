.PHONY: check prepare-check core-oracle reference fixtures adapters report qualify

export PATH := $(CURDIR)/.research/reference/install/bin:$(PATH)

prepare-check:
	$(MAKE) reference
	$(MAKE) fixtures
	python3 tools/fetch-required-vectors.py
	$(MAKE) adapters

check:
	CARGO_INCREMENTAL=0 cargo +nightly fmt --all --check
	CARGO_INCREMENTAL=0 cargo +nightly test --workspace --locked
	CARGO_INCREMENTAL=0 cargo +nightly clippy --workspace --all-targets --locked -- -D warnings
	CARGO_INCREMENTAL=0 cargo +nightly check --workspace --no-default-features --locked
	CARGO_INCREMENTAL=0 cargo +nightly build --workspace --release --locked
	python3 -m unittest harness.test_runner harness.test_timing harness.test_header harness.test_zlib harness.test_content harness.test_seek harness.test_checksum harness.test_comparison harness.test_reference_encodings harness.test_specs harness.test_colour harness.test_catalogue harness.test_provenance harness.test_fuzz harness.test_fuzz_integrity harness.test_reuse harness.test_linux_port harness.test_linux_guard harness.test_linux_kernel_bound harness.test_delivery_policy harness.test_required_vectors
	python3 tools/check-safety.py
	python3 tools/check-design.py
	python3 tools/test-core-time.py

core-oracle:
	python3 tools/test-core-time.py

reference:
	python3 tools/prepare-ffmpeg.py

fixtures:
	python3 tools/generate-corpus.py
	python3 tools/generate-header-corpus.py
	python3 tools/generate-zlib-corpus.py
	python3 tools/generate-seek-corpus.py
	python3 tools/generate-colour-corpus.py
	python3 tools/generate-catalogue-corpus.py

adapters:
	python3 tools/build-harness.py

report:
	python3 tools/test-media.py --fate-cache .research/fate-samples

qualify:
	python3 tools/test-media.py --benchmark --memory --fate-cache .research/fate-samples --require-acceptance
