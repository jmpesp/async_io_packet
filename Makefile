
.PHONY: all clippy fmt test build tags clean

all: clippy

clippy: fmt
	cargo clippy
	cargo clippy --tests

fmt: test
	cargo fmt

test: build
	RUST_BACKTRACE=full cargo test

build: banner
	cargo build

banner:
	banner "a build!"

tags:
	ctags -R --exclude=target --exclude=aws_benchmark/.venv .

clean:
	cargo clean

