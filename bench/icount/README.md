# `jmax-icount`: Hardware Instruction Counter Harness

Measures per-call retired instructions across the six general corpora and parser routes. On macOS, `jmax-icount` reads `proc_pid_rusage(RUSAGE_INFO_V4)` across `0` and `N` warmed iterations and divides the delta by `N`. On Linux, `icount.sh` runs the binary under Callgrind.

## Usage

```sh
bench/data/fetch.sh                                  # fetch external corpora once
bench/icount/icount.sh /tmp/base.tsv 128             # all parsers and classes
PARSERS=jmax bench/icount/icount.sh /tmp/new.tsv 128 # jmax rows only
bench/icount/cmp.sh /tmp/base.tsv /tmp/new.tsv       # compare before/after (default 1% gate)
```

Set `GATE_PCT` (default `1.0`) to adjust the maximum allowed instruction-count regression percentage in `cmp.sh`.
