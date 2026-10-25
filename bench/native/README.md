# Native Decoder Benchmarks (`jmax-native-bench`)

Compares generated `jmax-native` AOT decoders against `serde_json`, `sonic-rs`, `simd-json`, `jiter`, and `ps-pikkr` across four Binance exchange payloads (`aggTrade`, `depth_20`, `depth_1000`, `trades_100`) and the six general corpora.

## Running

```sh
cd bench/native
cargo build --release -j 4
./target/release/time > run.tsv
./tables.sh run.tsv
./icount.sh 128 > instructions.tsv
```

To aggregate multiple rounds, pass multiple TSV files to `bun ../tables.ts run1.tsv run2.tsv run3.tsv`.

## Payloads and Routes

| Payload | Size | Description |
|---|---:|---|
| `aggTrade` | 137 B | Single aggregate trade event |
| `depth_20` | 1.2 KB | Order book depth update with 20 bid and 20 ask levels |
| `depth_1000` | 63.6 KB | Order book REST snapshot with 1,000 bid and 1,000 ask levels |
| `trades_100` | 14.6 KB | Batch of 100 aggregate trade records |

| Route | Description |
|---|---|
| `typed / jmax-native` | Generated zero-allocation AOT decoder with exact `Decimal128` parsing, strict unknown/duplicate key rejection, and default `VmLimits` |
| `typed / jmax-native budget` | Generated AOT decoder with `max_steps` raised to `2^28` |
| `typed / jmax vm+reparse` | Full `jmax-schema-vm` validation followed by native decoding |
| `typed / <rival> String` | `serde` deserialization keeping decimal fields as owned `String`s |
| `typed / <rival> fixed` | `serde` deserialization parsing decimal fields into `10^8`-scaled `i64` fixed-point values |

Set `CASES=general/twitter,general/citm_catalog,...` to run general corpus projections through the native benchmark binary.
