.PHONY: check check-simd prepare-check core-oracle reference fixtures adapters report qualify

export PATH := $(CURDIR)/.research/reference/install/bin:$(PATH)

prepare-check:
	$(MAKE) reference
	$(MAKE) fixtures
	cargo xtask fetch-vectors
	$(MAKE) adapters

check:
	cargo xtask check

check-simd:
	cargo xtask check-simd

core-oracle:
	python3 tools/test-core-time.py

reference:
	cargo xtask prepare-ffmpeg

fixtures:
	cargo xtask fixtures

adapters:
	python3 tools/build_harness.py

report:
	cargo xtask report --fate-cache .research/fate-samples

qualify:
	cargo xtask report --benchmark --memory --fate-cache .research/fate-samples --require-acceptance
