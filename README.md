# matreader

A terminal UI for poking around MATLAB `.mat` files without loading them.

```sh
matreader results.mat          # browse interactively
matreader --list results.mat   # just print the top-level variables
```

matreader shows the top-level variables of a file and lets you drill into
structs, cells, and arrays with vim-style keys. It is built to be lazy: opening
a multi-gigabyte file is instant, and browsing a huge matrix only reads the
handful of chunks under the viewport.

## Supported formats

| Format | How MATLAB writes it | How matreader reads it |
|---|---|---|
| MAT v5 uncompressed (`save -v6`) | one binary element per variable | memory-mapped; nothing is copied until you look at values |
| MAT v5 compressed (`save -v7`, the default) | each variable is a separate zlib stream | only the first ~1 KB of each variable is inflated to list it; a variable is fully inflated (in a background thread, cancellable) when you open it |
| MAT v7.3 (`save -v7.3`, HDF5) | HDF5 with MATLAB conventions | streamed with the pure-Rust `hdf5-pure` crate; numeric data is read chunk-by-chunk from disk |

Supported MATLAB types: all numeric classes (including int64/uint64), logical,
char (UTF-16), complex, sparse (double, logical, complex), N-D arrays, cell
arrays, structs and struct arrays, function handles, and empty arrays.

MCOS objects (`string`, `table`, `datetime`, `categorical`, `containers.Map`,
timetables, user classes, …) are listed with their class name, but their
contents live in an undocumented "subsystem" blob and are **not decoded**.
matreader shows their raw object metadata instead.

MAT v4 files are not supported.

## Keys

| Key | Action |
|---|---|
| `j` / `k`, arrows | move |
| `Enter`, `l`, `→` | open the selected variable / field / cell |
| `Esc`, `Backspace`, `h`, `←` | back up one level |
| `g` / `G` | first / last |
| `PgUp` / `PgDn`, `Ctrl-u` / `Ctrl-d` | page |
| `/` | filter the current list (`Esc` clears) |
| `D` | toggle showing numbers as MATLAB datenums (`2024-02-29 13:45:30`) |
| `+` / `-` | more / fewer significant digits (default 6, up to 17) |
| `?` | help |
| `q` | quit |

In the numeric grid view, `h`/`l` move by column, `H`/`L` by five columns,
`0`/`$` jump to the first/last column, and `[`/`]` step through the pages of an
N-D array. The line under the grid shows the full-precision value of the
current cell and its subscripts.

Display options (`D`, `+`/`-`) apply everywhere values are shown and are
indicated at the right of the title bar when not at their defaults. A
non-integer value is never rounded to look like an integer: `738000.04` shows
as `738000.0` at 6 digits, not `738000`. In date mode, values that are not
plausible datenums (outside years 0–9999) are still shown as numbers, and the
detail line shows the date to the millisecond alongside the raw datenum.

The breadcrumb at the top shows where you are, e.g. `results{3}.values`.

## Building

```sh
cargo build --release
install target/release/matreader ~/.local/bin/
```

There are no native dependencies: the HDF5 reader is pure Rust.

## Testing

```sh
cargo test
```

The tests run against `fixtures/zoo_*.mat`, a zoo of MATLAB types saved by
MATLAB R2024b in `-v6`, `-v7`, `-v7.3`, and `-v7.3 -nocompression` form
(`scripts/make_fixtures.m`). `tests/ui_smoke.rs` drives the TUI through every
variable with a test backend. `scripts/make_big_fixtures.m` produces 0.5 GB
fixtures for timing with `cargo run --release --example bench -- FILE`.

## How the laziness works

*v5 files* are memory-mapped and indexed by walking the element tags; that
touches a few bytes per variable. For compressed variables a streaming inflate
is stopped as soon as the name, class, and dimensions have been decoded.
Opening a variable inflates it into memory, once, on a worker thread; the
parsed tree keeps byte offsets into that buffer rather than copies, so nested
cells and structs cost nothing until visited.

*v7.3 files* are HDF5. Variables are listed from the root group's metadata.
Each chunked dataset's chunk index is read once, and the grid view then reads,
decompresses, and caches only the chunks intersecting the visible window
(MATLAB chunks along its own rows, so a viewport of 40×12 doubles typically
touches a few chunks of tens of KB). Cell and struct-array elements are HDF5
object references, resolved once per container when it is opened.
