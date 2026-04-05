.PHONY: install build test clean

install:
	cargo install --path .

build:
	cargo build --release

test:
	cargo test

clean:
	cargo clean
