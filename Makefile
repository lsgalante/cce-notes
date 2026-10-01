.PHONY: build install run clean

build:
	cargo build --release

# Binaries and desktop entries are enumerated by ccebuild from cargo
# metadata and the crate root; never name them here.
install: build
	@command -v ccebuild >/dev/null || { echo "ccebuild not installed — run: make -C ../cce-compositor install"; exit 1; }
	ccebuild install --no-build cce-notes

run:
	cargo run

clean:
	cargo clean
