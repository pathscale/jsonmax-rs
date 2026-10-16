| Corpus | Operation | Implementation | MB/s; p50 / p95 ns/doc | allocs/op |
| --- | --- | --- | --- | --- |
| twitter | DOM Parse | jmax-dom-borrowed | 274.55; 2300209.0 / 4462542.0 | 44.0 |
| twitter | DOM Parse | serde-json-value | 465.12; 1357750.0 / 3093354.5 | 20845.0 |
| twitter | DOM Parse | simd-json-borrowed+copy | 1203.10; 524908.4 / 1097375.0 | 3889.0 |
| twitter | DOM Parse | simd-json-borrowed-prepared | 1262.37; 500260.5 / 1062474.0 | 3888.0 |
| twitter | DOM Parse | sonic-rs-value | 1897.36; 332839.3 / 555809.6 | 13.0 |
| twitter | Typed strict projection | jmax-vm-owned-extract | 277.65; 2274500.0 / 4590042.0 | 44.0 |
| twitter | Typed strict projection | serde-json-typed | 1571.91; 401750.0 / 569300.0 | 2.0 |
| twitter | Typed strict projection | simd-json-typed+copy | 1627.94; 387923.7 / 652007.0 | 8.0 |
| twitter | Typed strict projection | sonic-rs-typed | 1363.77; 463066.8 / 874883.4 | 1.0 |
| twitter | Serialize | jmax-native-tape-encode-sink | 399.34; 1581396.0 / 3298292.0 | 20.0 |
| twitter | Serialize | serde-json-to-vec | 1635.94; 386025.0 / 850138.8 | 13.0 |
| twitter | Serialize | simd-json-to-vec | 1541.22; 409750.0 / 626666.6 | 11.0 |
| twitter | Serialize | sonic-rs-to-vec | 2341.59; 269694.5 / 436798.7 | 12.0 |
| twitter | Query only | jmax-query-only-extract-borrowed | 1167.87; 540741.8 / 1047083.4 | 20.0 |
| twitter | Query only | ps-pikkr-trained-query | 2893.54; 218250.0 / 393783.3 | 1.0 |
| twitter | Query only | ps-pikkr-untrained-query | 2901.07; 217683.4 / 425629.7 | 1.0 |
| citm_catalog | DOM Parse | jmax-dom-borrowed | 366.08; 4718042.0 / 10310292.0 | 33.0 |
| citm_catalog | DOM Parse | serde-json-value | 752.02; 2296750.0 / 4656125.0 | 41010.0 |
| citm_catalog | DOM Parse | simd-json-borrowed+copy | 1123.99; 1536666.0 / 2888791.0 | 34086.0 |
| citm_catalog | DOM Parse | simd-json-borrowed-prepared | 1319.69; 1308791.8 / 2273203.1 | 34085.0 |
| citm_catalog | DOM Parse | sonic-rs-value | 2329.73; 741375.0 / 1499542.0 | 15.0 |
| citm_catalog | Typed strict projection | jmax-vm-owned-extract | 364.02; 4744834.0 / 8720667.0 | 32.0 |
| citm_catalog | Typed strict projection | serde-json-typed | 1494.34; 1155833.0 / 1750771.0 | 2.0 |
| citm_catalog | Typed strict projection | simd-json-typed+copy | 1880.38; 918541.5 / 1524722.3 | 8.0 |
| citm_catalog | Typed strict projection | sonic-rs-typed | 1829.61; 944027.7 / 1918264.0 | 1.0 |
| citm_catalog | Serialize | jmax-native-tape-encode-sink | 591.26; 2921250.0 / 4423792.0 | 19.0 |
| citm_catalog | Serialize | serde-json-to-vec | 3398.54; 508218.8 / 995791.8 | 13.0 |
| citm_catalog | Serialize | simd-json-to-vec | 3362.50; 513666.8 / 744843.8 | 11.0 |
| citm_catalog | Serialize | sonic-rs-to-vec | 3261.13; 529633.4 / 929541.8 | 13.0 |
| citm_catalog | Query only | jmax-query-only-extract-borrowed | 1133.06; 1524375.0 / 2824250.0 | 28.0 |
| citm_catalog | Query only | ps-pikkr-trained-query | 2782.87; 620656.2 / 1163041.7 | 1.0 |
| citm_catalog | Query only | ps-pikkr-untrained-query | 2731.70; 632281.2 / 1036062.5 | 1.0 |
| canada | DOM Parse | jmax-dom-borrowed | 161.98; 13896750.0 / 30485292.0 | 21.0 |
| canada | DOM Parse | serde-json-value | 435.31; 5171125.0 / 19902208.0 | 57683.0 |
| canada | DOM Parse | simd-json-borrowed+copy | 522.27; 4310167.0 / 11679000.0 | 112106.0 |
| canada | DOM Parse | simd-json-borrowed-prepared | 454.39; 4954057.2 / 9838911.5 | 112105.0 |
| canada | DOM Parse | sonic-rs-value | 1283.56; 1753750.0 / 4251458.0 | 17.0 |
| canada | Typed strict projection | jmax-vm-owned-extract | 151.08; 14900208.0 / 39331667.0 | 16.0 |
| canada | Typed strict projection | serde-json-typed | 1325.15; 1698708.0 / 3297167.0 | 2.0 |
| canada | Typed strict projection | simd-json-typed+copy | 711.19; 3165209.0 / 7504208.0 | 9.0 |
| canada | Typed strict projection | sonic-rs-typed | 1204.31; 1869166.0 / 3799208.0 | 1.0 |
| canada | Serialize | jmax-native-tape-encode-sink | 355.60; 6330208.0 / 16392708.0 | 21.0 |
| canada | Serialize | serde-json-to-vec | 945.29; 2381334.0 / 4726458.0 | 15.0 |
| canada | Serialize | simd-json-to-vec | 795.72; 2828958.0 / 6836375.0 | 13.0 |
| canada | Serialize | sonic-rs-to-vec | 936.09; 2404750.0 / 4762541.0 | 15.0 |
| canada | Query only | jmax-query-only-extract-borrowed | 1086.83; 2071209.0 / 4892291.0 | 13.0 |
| canada | Query only | ps-pikkr-trained-query | 3075.32; 731972.3 / 1358375.0 | 1.0 |
| canada | Query only | ps-pikkr-untrained-query | 3020.19; 745333.3 / 1579250.0 | 1.0 |
| github_events | DOM Parse | jmax-dom-borrowed | 370.70; 175700.0 / 236070.8 | 30.0 |
| github_events | DOM Parse | serde-json-value | 647.16; 100643.6 / 160608.8 | 2137.0 |
| github_events | DOM Parse | simd-json-borrowed+copy | 1389.73; 46866.7 / 75602.4 | 403.0 |
| github_events | DOM Parse | simd-json-borrowed-prepared | 1604.90; 40583.2 / 48677.1 | 402.0 |
| github_events | DOM Parse | sonic-rs-value | 2438.77; 26706.9 / 38508.0 | 9.0 |
| github_events | Typed strict projection | jmax-vm-owned-extract | 162.52; 400756.8 / 617632.0 | 578.0 |
| github_events | Typed strict projection | serde-json-typed | 1656.37; 39322.1 / 54894.1 | 65.0 |
| github_events | Typed strict projection | simd-json-typed+copy | 1737.15; 37493.6 / 55234.5 | 69.0 |
| github_events | Typed strict projection | sonic-rs-typed | 1493.48; 43610.8 / 59712.8 | 64.0 |
| github_events | Serialize | jmax-native-tape-encode-sink | 434.97; 149738.1 / 205464.7 | 16.0 |
| github_events | Serialize | serde-json-to-vec | 1492.56; 43637.7 / 59360.5 | 10.0 |
| github_events | Serialize | simd-json-to-vec | 1868.54; 34857.1 / 53381.9 | 8.0 |
| github_events | Serialize | sonic-rs-to-vec | 2564.19; 25400.6 / 41956.4 | 10.0 |
| github_events | Query only | jmax-query-only-extract-borrowed | 1106.44; 58866.4 / 85593.8 | 46.0 |
| github_events | Query only | ps-pikkr-trained-query | N/A | N/A |
| github_events | Query only | ps-pikkr-untrained-query | N/A | N/A |
| apache_builds | DOM Parse | jmax-dom-borrowed | 369.35; 344593.8 / 600468.8 | 31.0 |
| apache_builds | DOM Parse | serde-json-value | 575.99; 220966.7 / 334525.0 | 6189.0 |
| apache_builds | DOM Parse | simd-json-borrowed+copy | 1314.04; 96858.0 / 146298.1 | 1779.0 |
| apache_builds | DOM Parse | simd-json-borrowed-prepared | 1293.16; 98421.9 / 129406.2 | 1778.0 |
| apache_builds | DOM Parse | sonic-rs-value | 1874.88; 67884.4 / 130410.1 | 10.0 |
| apache_builds | Typed strict projection | jmax-vm-owned-extract | 324.08; 392722.2 / 782591.6 | 16.0 |
| apache_builds | Typed strict projection | serde-json-typed | 1393.85; 91311.6 / 157154.8 | 2.0 |
| apache_builds | Typed strict projection | simd-json-typed+copy | 1727.72; 73666.6 / 105313.3 | 9.0 |
| apache_builds | Typed strict projection | sonic-rs-typed | 1124.05; 113229.1 / 158581.2 | 1.0 |
| apache_builds | Serialize | jmax-native-tape-encode-sink | 352.64; 360923.7 / 457052.0 | 15.0 |
| apache_builds | Serialize | serde-json-to-vec | 1377.68; 92383.4 / 127265.0 | 11.0 |
| apache_builds | Serialize | simd-json-to-vec | 1631.45; 78013.4 / 97906.2 | 9.0 |
| apache_builds | Serialize | sonic-rs-to-vec | 2202.53; 57785.9 / 93121.4 | 9.0 |
| apache_builds | Query only | jmax-query-only-extract-borrowed | 1124.95; 113138.2 / 149596.5 | 27.0 |
| apache_builds | Query only | ps-pikkr-trained-query | 3021.49; 42123.3 / 62670.7 | 1.0 |
| apache_builds | Query only | ps-pikkr-untrained-query | 3010.79; 42273.0 / 69240.9 | 1.0 |
| endpoint_batch | DOM Parse | jmax-dom-borrowed | 143.81; 712680.3 / 2566500.0 | 37.0 |
| endpoint_batch | DOM Parse | serde-json-value | 309.57; 331083.3 / 1100750.0 | 9119.0 |
| endpoint_batch | DOM Parse | simd-json-borrowed+copy | 595.62; 172078.8 / 343743.0 | 3688.0 |
| endpoint_batch | DOM Parse | simd-json-borrowed-prepared | 620.98; 165052.0 / 216770.8 | 3687.0 |
| endpoint_batch | DOM Parse | sonic-rs-value | 1450.10; 70680.5 / 194680.5 | 11.0 |
| endpoint_batch | Typed strict projection | jmax-vm-owned-extract | 138.43; 740416.7 / 1832895.5 | 20.0 |
| endpoint_batch | Typed strict projection | serde-json-typed | 790.38; 129677.0 / 165875.0 | 3.0 |
| endpoint_batch | Typed strict projection | simd-json-typed+copy | 816.19; 125576.4 / 701348.1 | 11.0 |
| endpoint_batch | Typed strict projection | sonic-rs-typed | 708.89; 144583.3 / 189592.6 | 2.0 |
| endpoint_batch | Serialize | jmax-native-tape-encode-sink | 166.86; 614250.0 / 1272333.0 | 17.0 |
| endpoint_batch | Serialize | serde-json-to-vec | 826.70; 123979.0 / 292958.2 | 11.0 |
| endpoint_batch | Serialize | simd-json-to-vec | 757.17; 135364.5 / 344412.9 | 9.0 |
| endpoint_batch | Serialize | sonic-rs-to-vec | 936.65; 109425.9 / 210495.9 | 11.0 |
| endpoint_batch | Query only | jmax-query-only-extract-borrowed | 1110.52; 92293.4 / 447431.1 | 14.0 |
| endpoint_batch | Query only | ps-pikkr-trained-query | 2835.61; 36145.4 / 75594.6 | 1.0 |
| endpoint_batch | Query only | ps-pikkr-untrained-query | 2829.98; 36217.2 / 92243.4 | 1.0 |
