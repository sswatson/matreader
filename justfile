# matreader — Justfile for common operations

# Show available recipes
default:
    @just --list

# Build in release mode
build:
    cargo build --release

# Open a .mat file in the TUI (debug build)
run file:
    cargo run -- {{file}}

# Open a .mat file in the TUI (release build)
run-release file:
    cargo run --release -- {{file}}

# Print the variables of a .mat file without starting the TUI
list file:
    cargo run --release -- --list {{file}}

# Time loading/windowed reads for every variable of a file
bench file:
    cargo run --release --example bench -- {{file}}

# Run all tests (fixture parsing + TUI smoke tests)
test:
    cargo test

# Print a few rendered screens from the TUI smoke test
screens:
    cargo test --test ui_smoke screenshots -- --nocapture

# Regenerate fixtures/zoo_*.mat (needs MATLAB on PATH)
fixtures:
    matlab -batch "addpath('scripts'); make_fixtures('fixtures')"

# Format, lint
fmt:
    cargo fmt

lint:
    cargo clippy -- -D warnings

# Install to ~/.local/bin
install: build
    install target/release/matreader ~/.local/bin/matreader
