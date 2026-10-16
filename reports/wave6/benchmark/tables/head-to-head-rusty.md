| Corpus | Operation | Implementation | MB/s; p50 / p95 ns/doc | allocs/op |
| --- | --- | --- | --- | --- |
| twitter | DOM Parse | jmax-dom-borrowed | 418.63; 1508542.0 / 5906125.0 | 41.0 |
| twitter | DOM Parse | serde-json-value | 446.39; 1414708.0 / 11045917.0 | 20845.0 |
| twitter | DOM Parse | simd-json-borrowed+copy | 1211.35; 521333.0 / 1702083.5 | 3889.0 |
| twitter | DOM Parse | simd-json-borrowed-prepared | 1407.34; 448729.1 / 3008635.4 | 3888.0 |
| twitter | DOM Parse | sonic-rs-value | 1940.49; 325441.6 / 2135000.0 | 13.0 |
| twitter | Typed strict projection | jmax-vm-owned-extract | 330.38; 1911459.0 / 14744104.0 | 38.0 |
| twitter | Typed strict projection | serde-json-typed | 1718.70; 367437.5 / 1678965.3 | 2.0 |
| twitter | Typed strict projection | simd-json-typed+copy | 1740.31; 362875.0 / 2663694.3 | 8.0 |
| twitter | Typed strict projection | sonic-rs-typed | 1376.65; 458733.2 / 3859718.8 | 1.0 |
| twitter | Serialize | jmax-native-tape-encode-sink | 718.89; 878459.0 / 7603597.0 | 2.0 |
| twitter | Serialize | serde-json-to-vec | 1723.00; 366521.0 / 1712896.0 | 13.0 |
| twitter | Serialize | simd-json-to-vec | 1513.82; 417167.0 / 3560975.0 | 11.0 |
| twitter | Serialize | sonic-rs-to-vec | 2585.03; 244296.9 / 1608343.8 | 12.0 |
| twitter | Query only | jmax-query-only-extract-borrowed | 1518.19; 415965.2 / 2333583.0 | 10.0 |
| twitter | Query only | ps-pikkr-trained-query | 3089.78; 204388.7 / 1332666.7 | 1.0 |
| twitter | Query only | ps-pikkr-untrained-query | 3097.40; 203885.5 / 1106025.0 | 1.0 |
| citm_catalog | DOM Parse | jmax-dom-borrowed | 530.91; 3253291.0 / 12027666.0 | 19.0 |
| citm_catalog | DOM Parse | serde-json-value | 820.15; 2105959.0 / 11372208.0 | 41010.0 |
| citm_catalog | DOM Parse | simd-json-borrowed+copy | 1400.34; 1233417.0 / 3787500.0 | 34086.0 |
| citm_catalog | DOM Parse | simd-json-borrowed-prepared | 1527.82; 1130505.2 / 2140338.6 | 34085.0 |
| citm_catalog | DOM Parse | sonic-rs-value | 2585.85; 667944.3 / 1786834.0 | 15.0 |
| citm_catalog | Typed strict projection | jmax-vm-owned-extract | 446.75; 3866167.0 / 16521792.0 | 26.0 |
| citm_catalog | Typed strict projection | serde-json-typed | 1710.74; 1009625.0 / 2564319.7 | 2.0 |
| citm_catalog | Typed strict projection | simd-json-typed+copy | 2062.95; 837250.0 / 2162333.0 | 8.0 |
| citm_catalog | Typed strict projection | sonic-rs-typed | 2022.73; 853896.0 / 2339062.5 | 1.0 |
| citm_catalog | Serialize | jmax-native-tape-encode-sink | 909.85; 1898333.0 / 6775000.0 | 2.0 |
| citm_catalog | Serialize | serde-json-to-vec | 3448.95; 500791.5 / 3373792.0 | 13.0 |
| citm_catalog | Serialize | simd-json-to-vec | 3617.97; 477396.0 / 2695604.0 | 11.0 |
| citm_catalog | Serialize | sonic-rs-to-vec | 3686.67; 468500.0 / 1013937.5 | 13.0 |
| citm_catalog | Query only | jmax-query-only-extract-borrowed | 1465.47; 1178597.3 / 4865854.0 | 18.0 |
| citm_catalog | Query only | ps-pikkr-trained-query | 3087.74; 559375.0 / 2519000.0 | 1.0 |
| citm_catalog | Query only | ps-pikkr-untrained-query | 3089.58; 559041.7 / 1739000.0 | 1.0 |
| canada | DOM Parse | jmax-dom-borrowed | 245.28; 9177542.0 / 25550125.0 | 8.0 |
| canada | DOM Parse | serde-json-value | 490.52; 4589083.0 / 20445292.0 | 57683.0 |
| canada | DOM Parse | simd-json-borrowed+copy | 465.95; 4831084.0 / 25405334.0 | 112106.0 |
| canada | DOM Parse | simd-json-borrowed-prepared | 462.22; 4870109.4 / 23590552.0 | 112105.0 |
| canada | DOM Parse | sonic-rs-value | 1466.09; 1535416.0 / 4956417.0 | 17.0 |
| canada | Typed strict projection | jmax-vm-owned-extract | 188.45; 11944791.0 / 40108666.0 | 12.0 |
| canada | Typed strict projection | serde-json-typed | 1530.91; 1470396.0 / 3227042.0 | 2.0 |
| canada | Typed strict projection | simd-json-typed+copy | 819.42; 2747125.0 / 7326584.0 | 9.0 |
| canada | Typed strict projection | sonic-rs-typed | 1392.91; 1616083.0 / 4122041.5 | 1.0 |
| canada | Serialize | jmax-native-tape-encode-sink | 400.93; 5614583.0 / 21920792.0 | 2.0 |
| canada | Serialize | serde-json-to-vec | 1092.85; 2059792.0 / 6730084.0 | 15.0 |
| canada | Serialize | simd-json-to-vec | 948.84; 2372417.0 / 5576666.0 | 13.0 |
| canada | Serialize | sonic-rs-to-vec | 1079.68; 2084917.0 / 4707083.0 | 15.0 |
| canada | Query only | jmax-query-only-extract-borrowed | 1372.71; 1639854.0 / 7121125.0 | 6.0 |
| canada | Query only | ps-pikkr-trained-query | 3538.85; 636097.3 / 1812472.0 | 1.0 |
| canada | Query only | ps-pikkr-untrained-query | 3508.02; 641687.5 / 1831604.2 | 1.0 |
| github_events | DOM Parse | jmax-dom-borrowed | 645.77; 100859.6 / 276669.6 | 22.0 |
| github_events | DOM Parse | serde-json-value | 731.87; 88994.1 / 361318.5 | 2137.0 |
| github_events | DOM Parse | simd-json-borrowed+copy | 1688.48; 38574.3 / 163497.8 | 403.0 |
| github_events | DOM Parse | simd-json-borrowed-prepared | 1898.77; 34302.1 / 100849.0 | 402.0 |
| github_events | DOM Parse | sonic-rs-value | 2915.49; 22340.0 / 63880.2 | 9.0 |
| github_events | Typed strict projection | jmax-vm-owned-extract | 243.80; 267156.2 / 1278434.6 | 450.0 |
| github_events | Typed strict projection | serde-json-typed | 1937.96; 33608.5 / 150514.9 | 65.0 |
| github_events | Typed strict projection | simd-json-typed+copy | 2085.04; 31237.7 / 81297.8 | 69.0 |
| github_events | Typed strict projection | sonic-rs-typed | 1773.06; 36734.3 / 260638.9 | 64.0 |
| github_events | Serialize | jmax-native-tape-encode-sink | 954.55; 68233.3 / 191492.0 | 2.0 |
| github_events | Serialize | serde-json-to-vec | 1825.51; 35678.9 / 203784.8 | 10.0 |
| github_events | Serialize | simd-json-to-vec | 2340.17; 27832.2 / 78900.7 | 8.0 |
| github_events | Serialize | sonic-rs-to-vec | 3144.57; 20712.6 / 165723.7 | 10.0 |
| github_events | Query only | jmax-query-only-extract-borrowed | 1652.35; 39417.7 / 324413.1 | 38.0 |
| github_events | Query only | ps-pikkr-trained-query | N/A | N/A |
| github_events | Query only | ps-pikkr-untrained-query | N/A | N/A |
| apache_builds | DOM Parse | jmax-dom-borrowed | 616.29; 206516.7 / 531270.9 | 14.0 |
| apache_builds | DOM Parse | serde-json-value | 719.69; 176847.2 / 554617.5 | 6189.0 |
| apache_builds | DOM Parse | simd-json-borrowed+copy | 1635.29; 77830.2 / 200992.6 | 1779.0 |
| apache_builds | DOM Parse | simd-json-borrowed-prepared | 1697.23; 74989.6 / 270333.2 | 1778.0 |
| apache_builds | DOM Parse | sonic-rs-value | 2263.95; 56218.2 / 167027.8 | 10.0 |
| apache_builds | Typed strict projection | jmax-vm-owned-extract | 422.98; 300898.9 / 817555.3 | 12.0 |
| apache_builds | Typed strict projection | serde-json-typed | 1665.28; 76428.6 / 202948.5 | 2.0 |
| apache_builds | Typed strict projection | simd-json-typed+copy | 2145.67; 59317.1 / 148787.9 | 9.0 |
| apache_builds | Typed strict projection | sonic-rs-typed | 1339.00; 95052.6 / 188614.6 | 1.0 |
| apache_builds | Serialize | jmax-native-tape-encode-sink | 758.59; 167778.8 / 463279.7 | 2.0 |
| apache_builds | Serialize | serde-json-to-vec | 1704.28; 74679.6 / 309701.1 | 11.0 |
| apache_builds | Serialize | simd-json-to-vec | 2016.55; 63115.2 / 162976.9 | 9.0 |
| apache_builds | Serialize | sonic-rs-to-vec | 2724.08; 46722.2 / 124581.2 | 9.0 |
| apache_builds | Query only | jmax-query-only-extract-borrowed | 1657.15; 76803.6 / 192064.1 | 13.0 |
| apache_builds | Query only | ps-pikkr-trained-query | 3658.82; 34785.8 / 73052.4 | 1.0 |
| apache_builds | Query only | ps-pikkr-untrained-query | 3644.73; 34920.3 / 80375.0 | 1.0 |
| endpoint_batch | DOM Parse | jmax-dom-borrowed | 274.56; 373303.6 / 924917.0 | 21.0 |
| endpoint_batch | DOM Parse | serde-json-value | 373.27; 274583.4 / 789146.0 | 9119.0 |
| endpoint_batch | DOM Parse | simd-json-borrowed+copy | 721.68; 142020.8 / 364138.7 | 3688.0 |
| endpoint_batch | DOM Parse | simd-json-borrowed-prepared | 706.17; 145140.6 / 656515.6 | 3687.0 |
| endpoint_batch | DOM Parse | sonic-rs-value | 1743.77; 58777.4 / 121369.1 | 11.0 |
| endpoint_batch | Typed strict projection | jmax-vm-owned-extract | 189.40; 541158.4 / 1203958.0 | 16.0 |
| endpoint_batch | Typed strict projection | serde-json-typed | 938.16; 109250.0 / 208625.0 | 3.0 |
| endpoint_batch | Typed strict projection | simd-json-typed+copy | 988.49; 103687.5 / 282031.2 | 11.0 |
| endpoint_batch | Typed strict projection | sonic-rs-typed | 868.81; 117969.9 / 283589.3 | 2.0 |
| endpoint_batch | Serialize | jmax-native-tape-encode-sink | 327.33; 313125.0 / 691479.0 | 2.0 |
| endpoint_batch | Serialize | serde-json-to-vec | 1007.69; 101712.1 / 373382.0 | 11.0 |
| endpoint_batch | Serialize | simd-json-to-vec | 928.25; 110416.7 / 183601.3 | 9.0 |
| endpoint_batch | Serialize | sonic-rs-to-vec | 1135.82; 90237.8 / 220125.0 | 11.0 |
| endpoint_batch | Query only | jmax-query-only-extract-borrowed | 1538.17; 66633.6 / 156271.0 | 7.0 |
| endpoint_batch | Query only | ps-pikkr-trained-query | 3393.09; 30206.7 / 60496.6 | 1.0 |
| endpoint_batch | Query only | ps-pikkr-untrained-query | 3426.24; 29914.4 / 64784.7 | 1.0 |
