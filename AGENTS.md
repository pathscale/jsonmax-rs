# Working agreement: jsonmax

For any coding agent working here. `CLAUDE.md` imports this file; never fork these rules into a
per-vendor file.

## House rules

- No AI attribution anywhere: no `Co-Authored-By`, no session links, no "Generated with" lines
  in commits, pull requests, comments or code. Commit identity is `PathScale <no-reply@pathscale.com>`,
  never a Claude or Anthropic name or email. Tools that create or edit a pull request can append a
  footer; re-read the body after every create or edit and strip it.
- No Python. Scripts are shell or Rust.
- No em dashes in code, comments, docs or commit messages.
- Never `git stash`. Stage your own paths only, with `git commit --only <paths>`.
- Never commit a `Cargo.lock`. The workspace and the bench crates depend on sibling checkouts by
  relative path (`../ps-rusty_alloc`, `../nagoya`, `../ps-simdutf8`, `../ps-pikkr`, ...), so
  every lock carries local drift. They are gitignored; keep it that way.
- No Docker.
- No threads. jsonmax creates no threads and owns no pool. Parallel work goes through nagoya
  (`jmax-nagoya`) as typed `Arc<In>` tasks with real `Send + Sync + 'static` bounds and owned
  `Send + 'static` output. Never use thread creation or scoped parallel-for APIs. Never serialize
  work that is independent just to avoid that boundary.
- Crates that are `#![no_std]` today stay `no_std` (with `alloc`). Do not pull `std` into the
  core path (scanning, syntax, DOM, schema VM, native decoders). `std` belongs only in the
  facade, tools and benches that already use it.

## Correctness gates

Every change that touches parsing, validation, decoding or encoding must keep these byte for
byte:

```sh
cd bench/harness && cargo build --release -j 4
DIFF=30000 ./target/release/jmax-harness | md5sum          # 6715c04f984ed8838a4f1678ced61116
NODUP=1 DIFF=3000 ./target/release/jmax-harness | md5sum   # bb107953a10f4344c43003d1964bf341
```

Plus `cargo test`, `cargo clippy` and `cargo fmt --check` on the workspace. Fast paths are
conservative: they return `None` (or an equivalent refusal) and fall back to the exact path,
which owns every error. A fast path never invents a different error or accepts what the exact
path rejects.

## Measuring

- Instruction counts are the performance metric: `bench/icount/icount.sh` and
  `bench/icount/cmp.sh` (default 1% regression gate). Wall time comes from
  `bench/compare.sh` and `bench/native` on a quiet machine, never with another build running.
- Report jmax native and jmax non-native (VM and general routes) side by side, against every
  rival in the class (serde_json, sonic-rs, simd-json, jiter, json-rust, ps-pikkr). Never show
  one jmax route alone, never pick the corpus or class that flatters, and state the machine.
  A class where jmax loses is reported as a loss.
- Bench crates are outside the workspace and keep their own target dirs. Run one release build
  at a time; parallel release builds exhaust memory.

## Agent workflow

- Plan and review with the strongest model; hand bounded coding tasks to a coding agent that
  returns a patch or local commit plus a one-line summary. Do not read subagent transcripts.
- Be token efficient. Do not yield or stall while work remains.
- Dependents (endpoint-libs, endpointgen, fleet applications) are integrated only after jmax
  beats every rival on both the native and general routes.
