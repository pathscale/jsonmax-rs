Slot counts are capacities, not bytes or RSS. Per-depth entries differ in layout from the previous global table. Tape reservation trades fewer cold growth calls for possible retained slack.

| Workload | Before duplicate slots | After duplicate slots | Before tape capacity bytes | After tape capacity bytes |
| --- | --- | --- | --- | --- |
| NagoyaAuth | 16 | 16 | 128 | 128 |
| ExchangeOrder | 16 | 16 | 256 | 256 |
| RpcEnvelope | 16 | 32 | 256 | 256 |
| LargeCatalog | 8192 | 32 | 65536 | 68224 |
| VectorCoordinates | 16 | 16 | 2048 | 1760 |
| DeepNesting | 64 | 272 | 1024 | 1088 |
| EscapedUnicode | 16 | 16 | 128 | 2008 |
| small_object-seed-5354007379338153539-docs-128-items-32 | 64 | 64 | 1024 | 1120 |
| escaped_strings-seed-5354007379338153539-docs-128-items-32 | 64 | 64 | 2048 | 2800 |
| unicode-seed-5354007379338153539-docs-128-items-32 | 64 | 64 | 2048 | 1776 |
| number_array-seed-5354007379338153539-docs-128-items-32 | 0 | 0 | 1024 | 672 |
| depth_book-seed-5354007379338153539-docs-128-items-32 | 16 | 16 | 4096 | 6432 |
| agg_trade-seed-5354007379338153539-docs-128-items-32 | 32 | 32 | 512 | 288 |
| nested-seed-5354007379338153539-docs-128-items-32 | 32 | 256 | 1024 | 1152 |
| long-unescaped-ascii | 16 | 16 | 64 | 2056 |
| long-unescaped-unicode | 16 | 16 | 64 | 7688 |
| escape-dense | 16 | 16 | 64 | 7176 |
| long-backslash-runs | 16 | 16 | 64 | 4104 |
| wide-object | 2048 | 2048 | 32768 | 32416 |
| reordered-fields | 16 | 32 | 256 | 256 |
| sparse-large-subtree | 16384 | 48 | 524288 | 356160 |
