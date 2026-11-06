.PHONY: check check-simd prepare-check core-oracle reference fixtures adapters report qualify

export PATH := $(CURDIR)/.research/reference/install/bin:$(PATH)

prepare-check:
	$(MAKE) reference
	$(MAKE) fixtures
	python3 tools/fetch_required_vectors.py
	$(MAKE) adapters

check:
	cargo xtask check

check-simd:
	cargo xtask check-simd

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
