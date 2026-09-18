# crossdb 再計測 集計（Issue #848）

rounds=5 dir=/tmp/crossdb848

| phase | db/config | n | metric | min-of-N | median | run-to-run 幅 |
| --- | --- | --- | --- | --- | --- | --- |
| agg_count | elasticsearch/exact | 5 | p50_us | 734.21 | 798.75 | 95.9% |
| agg_count | elasticsearch/hnsw | 5 | p50_us | 497.33 | 529.54 | 55.5% |
| agg_count | lancedb/exact | 5 | p50_us | 365.17 | 384.96 | 54.8% |
| agg_count | lancedb/hnsw | 5 | p50_us | 373.67 | 377.67 | 51.8% |
| agg_count | mongodb/exact | 5 | p50_us | 2332.00 | 2436.96 | 6.2% |
| agg_count | mongodb/hnsw | 5 | p50_us | 2257.96 | 2415.13 | 11.4% |
| agg_count | mongodb_plain/exact | 5 | p50_us | 2286.83 | 2373.67 | 27.1% |
| agg_count | mysql/exact | 5 | p50_us | 2002.29 | 2108.21 | 9.4% |
| agg_count | pgvector/exact | 5 | p50_us | 1931.92 | 1956.33 | 31.9% |
| agg_count | pgvector/hnsw | 5 | p50_us | 1785.33 | 1886.04 | 44.9% |
| agg_count | qdrant/exact | 5 | p50_us | 2018.96 | 2153.21 | 13.9% |
| agg_count | qdrant/hnsw | 5 | p50_us | 1640.29 | 1666.46 | 10.8% |
| agg_count | redis/exact | 5 | p50_us | 649.75 | 689.25 | 56.6% |
| agg_count | redis/hnsw | 5 | p50_us | 596.54 | 665.04 | 24.1% |
| agg_count | self/exact | 5 | p50_us | 104.50 | 115.46 | 30.5% |
| agg_count | self/hnsw | 5 | p50_us | 86.29 | 106.58 | 41.8% |
| agg_count | self_nosql/exact | 5 | p50_us | 200.96 | 206.79 | 57.2% |
| agg_count | sqlite_vec/exact | 5 | p50_us | 982.42 | 997.79 | 29.4% |
| agg_multi | elasticsearch/exact | 5 | p50_us | 1052.79 | 1138.25 | 171.1% |
| agg_multi | elasticsearch/hnsw | 5 | p50_us | 637.50 | 652.08 | 58.1% |
| agg_multi | lancedb/exact | 5 | p50_us | 5576.46 | 5713.87 | 42.1% |
| agg_multi | lancedb/hnsw | 5 | p50_us | 5610.29 | 5952.71 | 36.5% |
| agg_multi | mongodb/exact | 5 | p50_us | 9859.96 | 9882.75 | 1.0% |
| agg_multi | mongodb/hnsw | 5 | p50_us | 9530.83 | 9846.58 | 4.6% |
| agg_multi | mongodb_plain/exact | 5 | p50_us | 9770.13 | 9932.58 | 21.8% |
| agg_multi | mysql/exact | 5 | p50_us | 2678.21 | 2718.63 | 3.3% |
| agg_multi | pgvector/exact | 5 | p50_us | 2120.04 | 2172.87 | 43.3% |
| agg_multi | pgvector/hnsw | 5 | p50_us | 2061.88 | 2115.75 | 46.2% |
| agg_multi | redis/exact | 5 | p50_us | 6403.42 | 6707.67 | 44.9% |
| agg_multi | redis/hnsw | 5 | p50_us | 6176.17 | 6585.33 | 8.1% |
| agg_multi | self/exact | 5 | p50_us | 248.96 | 281.29 | 33.4% |
| agg_multi | self/hnsw | 5 | p50_us | 238.21 | 264.33 | 12.7% |
| agg_multi | self_nosql/exact | 5 | p50_us | 356.12 | 365.00 | 16.4% |
| agg_multi | sqlite_vec/exact | 5 | p50_us | 1708.04 | 1751.17 | 28.8% |
| bulk_hybrid_k200 | lancedb/exact | 5 | p50_us | 5181.50 | 5309.17 | 34.4% |
| bulk_hybrid_k200 | lancedb/hnsw | 5 | p50_us | 3777.67 | 3830.63 | 51.1% |
| bulk_hybrid_k200 | mongodb/exact | 5 | p50_us | 10792.96 | 11129.17 | 4.6% |
| bulk_hybrid_k200 | mongodb/hnsw | 5 | p50_us | 9784.08 | 10402.25 | 12.5% |
| bulk_hybrid_k200 | pgvector/exact | 5 | p50_us | 4621.96 | 4848.04 | 33.4% |
| bulk_hybrid_k200 | pgvector/hnsw | 5 | p50_us | 4490.04 | 4574.46 | 7.1% |
| bulk_hybrid_k200 | redis/exact | 5 | p50_us | 1950.46 | 2146.17 | 47.1% |
| bulk_hybrid_k200 | redis/hnsw | 5 | p50_us | 2332.42 | 2438.46 | 8.1% |
| bulk_hybrid_k200 | self/exact | 5 | p50_us | 1628.29 | 1708.08 | 21.5% |
| bulk_hybrid_k200 | self/hnsw | 5 | p50_us | 1506.67 | 1538.75 | 24.7% |
| bulk_hybrid_k200 | self_nosql/exact | 5 | p50_us | 1725.67 | 1953.92 | 31.3% |
| bulk_hybrid_k200 | sqlite_vec/exact | 5 | p50_us | 6409.88 | 6648.17 | 34.4% |
| bulk_knn_k1000 | elasticsearch/exact | 5 | p50_us | 13334.75 | 13648.37 | 40.6% |
| bulk_knn_k1000 | elasticsearch/hnsw | 5 | p50_us | 12421.08 | 14235.33 | 46.5% |
| bulk_knn_k1000 | lancedb/exact | 5 | p50_us | 6429.83 | 6805.62 | 30.5% |
| bulk_knn_k1000 | lancedb/hnsw | 5 | p50_us | 3562.75 | 3900.00 | 44.1% |
| bulk_knn_k1000 | mongodb/exact | 5 | p50_us | 9364.29 | 9834.46 | 11.8% |
| bulk_knn_k1000 | mongodb/hnsw | 5 | p50_us | 11010.17 | 11237.67 | 6.7% |
| bulk_knn_k1000 | pgvector/exact | 5 | p50_us | 4504.79 | 4695.88 | 36.4% |
| bulk_knn_k1000 | pgvector/hnsw | 5 | p50_us | 4331.63 | 4661.63 | 39.3% |
| bulk_knn_k1000 | qdrant/exact | 5 | p50_us | 12176.33 | 12494.04 | 3.1% |
| bulk_knn_k1000 | qdrant/hnsw | 5 | p50_us | 11639.92 | 11949.33 | 5.1% |
| bulk_knn_k1000 | redis/exact | 5 | p50_us | 8525.21 | 8699.17 | 28.9% |
| bulk_knn_k1000 | redis/hnsw | 5 | p50_us | 8788.17 | 8992.13 | 4.2% |
| bulk_knn_k1000 | self/exact | 5 | p50_us | 1353.92 | 1438.58 | 32.5% |
| bulk_knn_k1000 | self/hnsw | 5 | p50_us | 1225.67 | 1232.17 | 45.4% |
| bulk_knn_k1000 | self_nosql/exact | 5 | p50_us | 1700.54 | 1840.46 | 31.5% |
| bulk_knn_k1000 | sqlite_vec/exact | 5 | p50_us | 18319.08 | 19221.29 | 21.5% |
| bulk_knn_k200 | elasticsearch/exact | 5 | p50_us | 4912.63 | 5093.46 | 87.4% |
| bulk_knn_k200 | elasticsearch/hnsw | 5 | p50_us | 3544.75 | 3877.92 | 52.4% |
| bulk_knn_k200 | lancedb/exact | 5 | p50_us | 4313.50 | 4516.04 | 35.1% |
| bulk_knn_k200 | lancedb/hnsw | 5 | p50_us | 2490.37 | 2673.46 | 45.1% |
| bulk_knn_k200 | mongodb/exact | 5 | p50_us | 3589.21 | 3665.79 | 8.5% |
| bulk_knn_k200 | mongodb/hnsw | 5 | p50_us | 3302.88 | 3508.71 | 29.6% |
| bulk_knn_k200 | pgvector/exact | 5 | p50_us | 3798.79 | 4024.25 | 39.7% |
| bulk_knn_k200 | pgvector/hnsw | 5 | p50_us | 3776.67 | 3796.54 | 35.9% |
| bulk_knn_k200 | qdrant/exact | 5 | p50_us | 3027.75 | 3129.54 | 4.8% |
| bulk_knn_k200 | qdrant/hnsw | 5 | p50_us | 2926.33 | 2974.58 | 4.8% |
| bulk_knn_k200 | redis/exact | 5 | p50_us | 2641.75 | 2689.46 | 26.1% |
| bulk_knn_k200 | redis/hnsw | 5 | p50_us | 2662.17 | 2788.42 | 11.3% |
| bulk_knn_k200 | self/exact | 5 | p50_us | 714.67 | 762.08 | 27.1% |
| bulk_knn_k200 | self/hnsw | 5 | p50_us | 587.92 | 644.92 | 49.4% |
| bulk_knn_k200 | self_nosql/exact | 5 | p50_us | 944.67 | 953.50 | 20.2% |
| bulk_knn_k200 | sqlite_vec/exact | 5 | p50_us | 5360.00 | 5938.12 | 27.9% |
| bulk_knn_where_k200 | elasticsearch/exact | 5 | p50_us | 3485.21 | 3942.83 | 53.8% |
| bulk_knn_where_k200 | elasticsearch/hnsw | 5 | p50_us | 3276.00 | 3618.00 | 66.1% |
| bulk_knn_where_k200 | lancedb/exact | 5 | p50_us | 4513.25 | 4751.71 | 40.8% |
| bulk_knn_where_k200 | lancedb/hnsw | 5 | p50_us | 3267.08 | 3613.38 | 43.0% |
| bulk_knn_where_k200 | mongodb/exact | 5 | p50_us | 3156.96 | 3397.25 | 19.4% |
| bulk_knn_where_k200 | mongodb/hnsw | 5 | p50_us | 3376.04 | 3503.38 | 12.6% |
| bulk_knn_where_k200 | pgvector/exact | 5 | p50_us | 2457.50 | 2529.21 | 39.5% |
| bulk_knn_where_k200 | pgvector/hnsw | 5 | p50_us | 2440.04 | 2533.25 | 71.8% |
| bulk_knn_where_k200 | qdrant/exact | 5 | p50_us | 3079.25 | 3123.25 | 10.8% |
| bulk_knn_where_k200 | qdrant/hnsw | 5 | p50_us | 2986.50 | 3079.04 | 9.0% |
| bulk_knn_where_k200 | redis/exact | 5 | p50_us | 2440.38 | 2447.33 | 49.7% |
| bulk_knn_where_k200 | redis/hnsw | 5 | p50_us | 2536.71 | 2626.50 | 7.2% |
| bulk_knn_where_k200 | self/exact | 5 | p50_us | 391.67 | 407.58 | 34.2% |
| bulk_knn_where_k200 | self/hnsw | 5 | p50_us | 2837.46 | 3064.62 | 42.8% |
| bulk_knn_where_k200 | self_nosql/exact | 5 | p50_us | 526.87 | 583.38 | 32.2% |
| bulk_knn_where_k200 | sqlite_vec/exact | 5 | p50_us | 6066.21 | 6597.13 | 33.7% |
| explain | elasticsearch/exact | 5 | p50_us | 2188.25 | 2262.21 | 121.8% |
| explain | elasticsearch/hnsw | 5 | p50_us | 1823.92 | 1990.04 | 75.3% |
| explain | lancedb/exact | 5 | p50_us | 596.04 | 678.12 | 61.0% |
| explain | lancedb/hnsw | 5 | p50_us | 309.79 | 320.17 | 75.9% |
| explain | mongodb/exact | 5 | p50_us | 3503.21 | 3755.21 | 9.1% |
| explain | mongodb/hnsw | 5 | p50_us | 1657.71 | 1825.00 | 31.0% |
| explain | mongodb_plain/exact | 5 | p50_us | 835.96 | 1056.12 | 50.8% |
| explain | mysql/exact | 5 | p50_us | 399.29 | 493.38 | 51.4% |
| explain | pgvector/exact | 5 | p50_us | 256.25 | 284.83 | 36.5% |
| explain | pgvector/hnsw | 5 | p50_us | 270.71 | 286.08 | 25.1% |
| explain | redis/exact | 5 | p50_us | 241.96 | 245.13 | 44.7% |
| explain | redis/hnsw | 5 | p50_us | 240.29 | 248.62 | 18.8% |
| explain | sqlite_vec/exact | 5 | p50_us | 3.38 | 3.46 | 3.7% |
| group_by_having | elasticsearch/exact | 5 | p50_us | 1065.67 | 1100.83 | 120.1% |
| group_by_having | elasticsearch/hnsw | 5 | p50_us | 650.79 | 721.29 | 90.7% |
| group_by_having | lancedb/exact | 5 | p50_us | 6542.58 | 6937.21 | 43.6% |
| group_by_having | lancedb/hnsw | 5 | p50_us | 6672.00 | 6736.46 | 88.5% |
| group_by_having | mongodb/exact | 5 | p50_us | 7972.75 | 8101.67 | 4.8% |
| group_by_having | mongodb/hnsw | 5 | p50_us | 7791.58 | 8175.67 | 7.2% |
| group_by_having | mongodb_plain/exact | 5 | p50_us | 4160.62 | 4217.88 | 21.2% |
| group_by_having | mysql/exact | 5 | p50_us | 11507.75 | 12141.62 | 16.0% |
| group_by_having | pgvector/exact | 5 | p50_us | 3027.42 | 3125.17 | 66.9% |
| group_by_having | pgvector/hnsw | 5 | p50_us | 2992.83 | 3297.21 | 32.2% |
| group_by_having | redis/exact | 5 | p50_us | 6461.67 | 6719.17 | 43.8% |
| group_by_having | redis/hnsw | 5 | p50_us | 6380.00 | 6653.25 | 10.2% |
| group_by_having | self/exact | 5 | p50_us | 70.58 | 81.04 | 51.1% |
| group_by_having | self/hnsw | 5 | p50_us | 51.46 | 75.46 | 68.9% |
| group_by_having | self_nosql/exact | 5 | p50_us | 126.92 | 172.87 | 181.5% |
| group_by_having | sqlite_vec/exact | 5 | p50_us | 2971.92 | 3150.00 | 30.3% |
| hybrid_rrf | lancedb/exact | 5 | p50_us | 3778.96 | 3929.96 | 43.1% |
| hybrid_rrf | lancedb/hnsw | 5 | p50_us | 2299.17 | 2397.54 | 52.9% |
| hybrid_rrf | mongodb/exact | 5 | p50_us | 5152.25 | 5727.29 | 12.5% |
| hybrid_rrf | mongodb/hnsw | 5 | p50_us | 3782.96 | 3999.87 | 11.3% |
| hybrid_rrf | pgvector/exact | 5 | p50_us | 4043.79 | 4437.37 | 40.5% |
| hybrid_rrf | pgvector/hnsw | 5 | p50_us | 4019.79 | 4057.75 | 27.0% |
| hybrid_rrf | redis/exact | 5 | p50_us | 1044.92 | 1272.42 | 37.7% |
| hybrid_rrf | redis/hnsw | 5 | p50_us | 1296.88 | 1326.17 | 11.3% |
| hybrid_rrf | self/exact | 5 | p50_us | 1467.88 | 1647.58 | 37.8% |
| hybrid_rrf | self/hnsw | 5 | p50_us | 1335.83 | 1430.50 | 41.0% |
| hybrid_rrf | self_nosql/exact | 5 | p50_us | 1600.12 | 1742.13 | 47.3% |
| hybrid_rrf | sqlite_vec/exact | 5 | p50_us | 3874.04 | 4237.54 | 34.4% |
| ingest_bulk | elasticsearch/exact | 5 | rows_per_sec | 12998.41 | 22316.41 | 73.9% |
| ingest_bulk | elasticsearch/hnsw | 5 | rows_per_sec | 9714.61 | 14767.54 | 54.1% |
| ingest_bulk | lancedb/exact | 5 | rows_per_sec | 116005.87 | 167883.73 | 87.0% |
| ingest_bulk | lancedb/hnsw | 5 | rows_per_sec | 167065.86 | 212732.61 | 36.7% |
| ingest_bulk | mongodb/exact | 5 | rows_per_sec | 59169.64 | 59339.22 | 2.0% |
| ingest_bulk | mongodb/hnsw | 5 | rows_per_sec | 42184.79 | 58238.56 | 42.2% |
| ingest_bulk | mongodb_plain/exact | 5 | rows_per_sec | 68377.95 | 76792.36 | 14.4% |
| ingest_bulk | mysql/exact | 5 | rows_per_sec | 18690.86 | 18933.94 | 4.9% |
| ingest_bulk | pgvector/exact | 5 | rows_per_sec | 46162.99 | 60261.90 | 33.7% |
| ingest_bulk | pgvector/hnsw | 5 | rows_per_sec | 46686.92 | 59096.82 | 31.0% |
| ingest_bulk | qdrant/exact | 5 | rows_per_sec | 16064.52 | 16154.43 | 2.2% |
| ingest_bulk | qdrant/hnsw | 5 | rows_per_sec | 16336.41 | 16446.52 | 1.7% |
| ingest_bulk | redis/exact | 5 | rows_per_sec | 38615.76 | 51691.17 | 36.4% |
| ingest_bulk | redis/hnsw | 5 | rows_per_sec | 24133.46 | 28111.87 | 22.7% |
| ingest_bulk | sqlite_vec/exact | 5 | rows_per_sec | 68832.94 | 89114.06 | 37.8% |
| ingest_single_stmt | elasticsearch/exact | 5 | rows_per_sec | 350.35 | 732.81 | 117.0% |
| ingest_single_stmt | elasticsearch/hnsw | 5 | rows_per_sec | 498.55 | 740.34 | 74.7% |
| ingest_single_stmt | lancedb/exact | 5 | rows_per_sec | 371.14 | 732.50 | 109.2% |
| ingest_single_stmt | lancedb/hnsw | 5 | rows_per_sec | 367.63 | 724.92 | 108.5% |
| ingest_single_stmt | mongodb/exact | 5 | rows_per_sec | 860.09 | 1057.05 | 25.5% |
| ingest_single_stmt | mongodb/hnsw | 5 | rows_per_sec | 1191.16 | 1244.04 | 11.4% |
| ingest_single_stmt | mongodb_plain/exact | 5 | rows_per_sec | 2274.26 | 3384.11 | 60.3% |
| ingest_single_stmt | mysql/exact | 5 | rows_per_sec | 739.79 | 900.86 | 30.8% |
| ingest_single_stmt | pgvector/exact | 5 | rows_per_sec | 1157.26 | 1902.76 | 109.5% |
| ingest_single_stmt | pgvector/hnsw | 5 | rows_per_sec | 665.45 | 695.91 | 5.6% |
| ingest_single_stmt | qdrant/exact | 5 | rows_per_sec | 1168.41 | 1396.57 | 37.6% |
| ingest_single_stmt | qdrant/hnsw | 5 | rows_per_sec | 603.55 | 675.12 | 27.6% |
| ingest_single_stmt | redis/exact | 5 | rows_per_sec | 2621.67 | 3760.53 | 53.7% |
| ingest_single_stmt | redis/hnsw | 5 | rows_per_sec | 2449.33 | 3561.69 | 52.6% |
| ingest_single_stmt | self/exact | 5 | rows_per_sec | 145.41 | 204.85 | 46.6% |
| ingest_single_stmt | self/hnsw | 5 | rows_per_sec | 182.46 | 201.46 | 19.5% |
| ingest_single_stmt | self_nosql/exact | 5 | rows_per_sec | 171.50 | 193.77 | 16.9% |
| ingest_single_stmt | sqlite_vec/exact | 5 | rows_per_sec | 793.11 | 1451.94 | 143.3% |
| mode_precision | self/exact | 5 | p50_us | 522.96 | 598.79 | 23.0% |
| mode_precision | self/hnsw | 5 | p50_us | 545.21 | 602.42 | 59.4% |
| mode_precision | self_nosql/exact | 5 | p50_us | 645.96 | 689.54 | 21.5% |
| mode_recall | self/exact | 5 | p50_us | 552.13 | 609.38 | 25.9% |
| mode_recall | self/hnsw | 5 | p50_us | 448.96 | 464.42 | 70.3% |
| mode_recall | self_nosql/exact | 5 | p50_us | 656.08 | 702.17 | 61.2% |
| point_where | elasticsearch/exact | 5 | p50_us | 1666.29 | 1703.67 | 89.4% |
| point_where | elasticsearch/hnsw | 5 | p50_us | 1144.67 | 1368.21 | 75.1% |
| point_where | lancedb/exact | 5 | p50_us | 3469.21 | 3679.08 | 37.9% |
| point_where | lancedb/hnsw | 5 | p50_us | 2311.29 | 2342.33 | 43.5% |
| point_where | mongodb/exact | 5 | p50_us | 1673.96 | 1837.04 | 11.7% |
| point_where | mongodb/hnsw | 5 | p50_us | 2530.63 | 2561.92 | 8.4% |
| point_where | pgvector/exact | 5 | p50_us | 2132.25 | 2241.71 | 114.8% |
| point_where | pgvector/hnsw | 5 | p50_us | 2126.92 | 2309.33 | 46.6% |
| point_where | qdrant/exact | 5 | p50_us | 650.17 | 654.62 | 3.2% |
| point_where | qdrant/hnsw | 5 | p50_us | 652.71 | 710.58 | 110.1% |
| point_where | redis/exact | 5 | p50_us | 849.83 | 901.00 | 33.6% |
| point_where | redis/hnsw | 5 | p50_us | 947.71 | 1060.42 | 49.3% |
| point_where | self/exact | 5 | p50_us | 246.17 | 267.08 | 34.8% |
| point_where | self/hnsw | 5 | p50_us | 2095.71 | 2425.29 | 57.8% |
| point_where | self_nosql/exact | 5 | p50_us | 331.96 | 357.08 | 74.7% |
| point_where | sqlite_vec/exact | 5 | p50_us | 2062.38 | 2144.25 | 28.1% |
| rls_isolation | elasticsearch/exact | 5 | p50_us | 601.79 | 641.83 | 66.5% |
| rls_isolation | elasticsearch/hnsw | 5 | p50_us | 479.79 | 490.71 | 55.9% |
| rls_isolation | lancedb/exact | 5 | p50_us | 370.75 | 407.33 | 62.0% |
| rls_isolation | lancedb/hnsw | 5 | p50_us | 380.67 | 385.96 | 68.5% |
| rls_isolation | mongodb/exact | 5 | p50_us | 2260.00 | 2414.33 | 9.3% |
| rls_isolation | mongodb/hnsw | 5 | p50_us | 2285.87 | 2321.58 | 5.7% |
| rls_isolation | mongodb_plain/exact | 5 | p50_us | 2254.04 | 2281.87 | 27.1% |
| rls_isolation | mysql/exact | 5 | p50_us | 2019.88 | 2115.88 | 7.6% |
| rls_isolation | pgvector/exact | 5 | p50_us | 1872.71 | 1986.88 | 35.5% |
| rls_isolation | pgvector/hnsw | 5 | p50_us | 1756.50 | 1907.67 | 10.9% |
| rls_isolation | qdrant/exact | 5 | p50_us | 1921.87 | 2083.21 | 10.3% |
| rls_isolation | qdrant/hnsw | 5 | p50_us | 1528.46 | 1605.42 | 7.8% |
| rls_isolation | redis/exact | 5 | p50_us | 661.83 | 664.13 | 56.9% |
| rls_isolation | redis/hnsw | 5 | p50_us | 655.58 | 676.00 | 50.6% |
| rls_isolation | self/exact | 5 | p50_us | 110.29 | 120.58 | 39.8% |
| rls_isolation | self/hnsw | 5 | p50_us | 110.92 | 119.17 | 31.1% |
| rls_isolation | self_nosql/exact | 5 | p50_us | 172.67 | 207.83 | 29.1% |
| rls_isolation | sqlite_vec/exact | 5 | p50_us | 998.42 | 1031.25 | 25.6% |
| scan_where_nosort_k500 | elasticsearch/exact | 5 | p50_us | 6505.08 | 6826.38 | 32.5% |
| scan_where_nosort_k500 | elasticsearch/hnsw | 5 | p50_us | 5486.37 | 6013.58 | 41.7% |
| scan_where_nosort_k500 | lancedb/exact | 5 | p50_us | 1382.46 | 1511.29 | 61.6% |
| scan_where_nosort_k500 | lancedb/hnsw | 5 | p50_us | 1390.17 | 1439.75 | 73.9% |
| scan_where_nosort_k500 | mongodb/exact | 5 | p50_us | 1183.67 | 1528.71 | 56.0% |
| scan_where_nosort_k500 | mongodb/hnsw | 5 | p50_us | 1190.67 | 1329.21 | 49.9% |
| scan_where_nosort_k500 | mongodb_plain/exact | 5 | p50_us | 1161.33 | 1340.67 | 52.7% |
| scan_where_nosort_k500 | mysql/exact | 5 | p50_us | 1582.46 | 1899.04 | 21.6% |
| scan_where_nosort_k500 | pgvector/exact | 5 | p50_us | 1074.83 | 1224.25 | 42.2% |
| scan_where_nosort_k500 | pgvector/hnsw | 5 | p50_us | 1029.63 | 1059.17 | 19.0% |
| scan_where_nosort_k500 | qdrant/exact | 5 | p50_us | 4401.00 | 4658.37 | 16.0% |
| scan_where_nosort_k500 | qdrant/hnsw | 5 | p50_us | 4402.71 | 4550.79 | 9.3% |
| scan_where_nosort_k500 | redis/exact | 5 | p50_us | 4262.42 | 4340.71 | 28.3% |
| scan_where_nosort_k500 | redis/hnsw | 5 | p50_us | 4132.17 | 4295.37 | 7.0% |
| scan_where_nosort_k500 | self/exact | 5 | p50_us | 438.29 | 462.58 | 28.0% |
| scan_where_nosort_k500 | self/hnsw | 5 | p50_us | 460.67 | 477.46 | 24.2% |
| scan_where_nosort_k500 | self_nosql/exact | 5 | p50_us | 589.54 | 655.17 | 68.0% |
| scan_where_nosort_k500 | sqlite_vec/exact | 5 | p50_us | 197.04 | 205.17 | 25.2% |
| udf_call | self/exact | 5 | p50_us | 546.46 | 597.87 | 38.7% |
| udf_call | self/hnsw | 5 | p50_us | 410.04 | 417.54 | 27.2% |
| vector_knn | elasticsearch/exact | 5 | p50_us | 3607.79 | 3768.96 | 137.4% |
| vector_knn | elasticsearch/hnsw | 5 | p50_us | 1730.92 | 1778.58 | 52.4% |
| vector_knn | lancedb/exact | 5 | p50_us | 3225.46 | 3348.92 | 44.1% |
| vector_knn | lancedb/hnsw | 5 | p50_us | 1544.21 | 1694.42 | 53.1% |
| vector_knn | mongodb/exact | 5 | p50_us | 2699.46 | 2827.08 | 8.6% |
| vector_knn | mongodb/hnsw | 5 | p50_us | 1538.75 | 1569.54 | 10.3% |
| vector_knn | pgvector/exact | 5 | p50_us | 3313.58 | 3515.00 | 47.3% |
| vector_knn | pgvector/hnsw | 5 | p50_us | 3139.46 | 3294.67 | 71.5% |
| vector_knn | qdrant/exact | 5 | p50_us | 707.58 | 769.33 | 12.6% |
| vector_knn | qdrant/hnsw | 5 | p50_us | 814.54 | 1148.67 | 87.6% |
| vector_knn | redis/exact | 5 | p50_us | 1011.37 | 1022.50 | 30.6% |
| vector_knn | redis/hnsw | 5 | p50_us | 1090.58 | 1109.79 | 2.3% |
| vector_knn | self/exact | 5 | p50_us | 573.33 | 593.00 | 21.1% |
| vector_knn | self/hnsw | 5 | p50_us | 422.13 | 457.58 | 27.0% |
| vector_knn | self_nosql/exact | 5 | p50_us | 668.17 | 707.71 | 9.7% |
| vector_knn | sqlite_vec/exact | 5 | p50_us | 2474.21 | 2714.00 | 27.4% |
| vector_knn_pipeline_bruteforce | mongodb_plain/exact | 5 | p50_us | 482406.37 | 494145.50 | 16.9% |
| vector_knn_where | elasticsearch/exact | 5 | p50_us | 1666.29 | 1703.67 | 89.4% |
| vector_knn_where | elasticsearch/hnsw | 5 | p50_us | 1144.67 | 1368.21 | 75.1% |
| vector_knn_where | lancedb/exact | 5 | p50_us | 3469.21 | 3679.08 | 37.9% |
| vector_knn_where | lancedb/hnsw | 5 | p50_us | 2311.29 | 2342.33 | 43.5% |
| vector_knn_where | mongodb/exact | 5 | p50_us | 1673.96 | 1837.04 | 11.7% |
| vector_knn_where | mongodb/hnsw | 5 | p50_us | 2530.63 | 2561.92 | 8.4% |
| vector_knn_where | pgvector/exact | 5 | p50_us | 2132.25 | 2241.71 | 114.8% |
| vector_knn_where | pgvector/hnsw | 5 | p50_us | 2126.92 | 2309.33 | 46.6% |
| vector_knn_where | qdrant/exact | 5 | p50_us | 650.17 | 654.62 | 3.2% |
| vector_knn_where | qdrant/hnsw | 5 | p50_us | 652.71 | 710.58 | 110.1% |
| vector_knn_where | redis/exact | 5 | p50_us | 849.83 | 901.00 | 33.6% |
| vector_knn_where | redis/hnsw | 5 | p50_us | 947.71 | 1060.42 | 49.3% |
| vector_knn_where | self/exact | 5 | p50_us | 246.17 | 267.08 | 34.8% |
| vector_knn_where | self/hnsw | 5 | p50_us | 2095.71 | 2425.29 | 57.8% |
| vector_knn_where | self_nosql/exact | 5 | p50_us | 331.96 | 357.08 | 74.7% |
| vector_knn_where | sqlite_vec/exact | 5 | p50_us | 2062.38 | 2144.25 | 28.1% |
| where_compound_count | elasticsearch/exact | 5 | p50_us | 875.42 | 925.92 | 97.5% |
| where_compound_count | elasticsearch/hnsw | 5 | p50_us | 530.04 | 537.17 | 33.9% |
| where_compound_count | lancedb/exact | 5 | p50_us | 614.71 | 657.71 | 51.0% |
| where_compound_count | lancedb/hnsw | 5 | p50_us | 608.04 | 643.96 | 51.7% |
| where_compound_count | mongodb/exact | 5 | p50_us | 3398.79 | 3481.04 | 5.0% |
| where_compound_count | mongodb/hnsw | 5 | p50_us | 3345.96 | 3558.13 | 8.8% |
| where_compound_count | mongodb_plain/exact | 5 | p50_us | 2791.08 | 2871.21 | 31.7% |
| where_compound_count | mysql/exact | 5 | p50_us | 3450.17 | 3579.75 | 5.2% |
| where_compound_count | pgvector/exact | 5 | p50_us | 1567.13 | 1608.17 | 62.0% |
| where_compound_count | pgvector/hnsw | 5 | p50_us | 1536.33 | 1554.50 | 31.8% |
| where_compound_count | qdrant/exact | 5 | p50_us | 7411.92 | 7811.71 | 8.7% |
| where_compound_count | qdrant/hnsw | 5 | p50_us | 7213.46 | 7450.46 | 4.7% |
| where_compound_count | redis/exact | 5 | p50_us | 1075.04 | 1154.58 | 48.5% |
| where_compound_count | redis/hnsw | 5 | p50_us | 1012.67 | 1198.71 | 22.6% |
| where_compound_count | self/exact | 5 | p50_us | 87.42 | 96.75 | 76.6% |
| where_compound_count | self/hnsw | 5 | p50_us | 78.58 | 107.83 | 69.4% |
| where_compound_count | sqlite_vec/exact | 5 | p50_us | 1298.33 | 1333.83 | 23.5% |

## self との比較（固定 ±5% 帯かつ両 arm の run-to-run 幅を超える場合のみ win/loss。それ以外は僅差）

| phase | 対照 db/config | self min-of-N | 対照 min-of-N | 比(対照/self) | 判定 |
| --- | --- | --- | --- | --- | --- |
| agg_count | elasticsearch/exact | 104.50 | 734.21 | 7.026 | self win |
| agg_count | elasticsearch/hnsw | 104.50 | 497.33 | 4.759 | self win |
| agg_count | lancedb/exact | 104.50 | 365.17 | 3.494 | self win |
| agg_count | lancedb/hnsw | 104.50 | 373.67 | 3.576 | self win |
| agg_count | mongodb/exact | 104.50 | 2332.00 | 22.316 | self win |
| agg_count | mongodb/hnsw | 104.50 | 2257.96 | 21.607 | self win |
| agg_count | mongodb_plain/exact | 104.50 | 2286.83 | 21.884 | self win |
| agg_count | mysql/exact | 104.50 | 2002.29 | 19.161 | self win |
| agg_count | pgvector/exact | 104.50 | 1931.92 | 18.487 | self win |
| agg_count | pgvector/hnsw | 104.50 | 1785.33 | 17.085 | self win |
| agg_count | qdrant/exact | 104.50 | 2018.96 | 19.320 | self win |
| agg_count | qdrant/hnsw | 104.50 | 1640.29 | 15.697 | self win |
| agg_count | redis/exact | 104.50 | 649.75 | 6.218 | self win |
| agg_count | redis/hnsw | 104.50 | 596.54 | 5.709 | self win |
| agg_count | self/hnsw | 104.50 | 86.29 | 0.826 | 僅差 |
| agg_count | self_nosql/exact | 104.50 | 200.96 | 1.923 | self win |
| agg_count | sqlite_vec/exact | 104.50 | 982.42 | 9.401 | self win |
| agg_multi | elasticsearch/exact | 248.96 | 1052.79 | 4.229 | self win |
| agg_multi | elasticsearch/hnsw | 248.96 | 637.50 | 2.561 | self win |
| agg_multi | lancedb/exact | 248.96 | 5576.46 | 22.399 | self win |
| agg_multi | lancedb/hnsw | 248.96 | 5610.29 | 22.535 | self win |
| agg_multi | mongodb/exact | 248.96 | 9859.96 | 39.605 | self win |
| agg_multi | mongodb/hnsw | 248.96 | 9530.83 | 38.283 | self win |
| agg_multi | mongodb_plain/exact | 248.96 | 9770.13 | 39.244 | self win |
| agg_multi | mysql/exact | 248.96 | 2678.21 | 10.758 | self win |
| agg_multi | pgvector/exact | 248.96 | 2120.04 | 8.516 | self win |
| agg_multi | pgvector/hnsw | 248.96 | 2061.88 | 8.282 | self win |
| agg_multi | redis/exact | 248.96 | 6403.42 | 25.721 | self win |
| agg_multi | redis/hnsw | 248.96 | 6176.17 | 24.808 | self win |
| agg_multi | self/hnsw | 248.96 | 238.21 | 0.957 | 僅差 |
| agg_multi | self_nosql/exact | 248.96 | 356.12 | 1.430 | self win |
| agg_multi | sqlite_vec/exact | 248.96 | 1708.04 | 6.861 | self win |
| bulk_hybrid_k200 | lancedb/exact | 1628.29 | 5181.50 | 3.182 | self win |
| bulk_hybrid_k200 | lancedb/hnsw | 1628.29 | 3777.67 | 2.320 | self win |
| bulk_hybrid_k200 | mongodb/exact | 1628.29 | 10792.96 | 6.628 | self win |
| bulk_hybrid_k200 | mongodb/hnsw | 1628.29 | 9784.08 | 6.009 | self win |
| bulk_hybrid_k200 | pgvector/exact | 1628.29 | 4621.96 | 2.839 | self win |
| bulk_hybrid_k200 | pgvector/hnsw | 1628.29 | 4490.04 | 2.758 | self win |
| bulk_hybrid_k200 | redis/exact | 1628.29 | 1950.46 | 1.198 | 僅差 |
| bulk_hybrid_k200 | redis/hnsw | 1628.29 | 2332.42 | 1.432 | self win |
| bulk_hybrid_k200 | self/hnsw | 1628.29 | 1506.67 | 0.925 | 僅差 |
| bulk_hybrid_k200 | self_nosql/exact | 1628.29 | 1725.67 | 1.060 | 僅差 |
| bulk_hybrid_k200 | sqlite_vec/exact | 1628.29 | 6409.88 | 3.937 | self win |
| bulk_knn_k1000 | elasticsearch/exact | 1353.92 | 13334.75 | 9.849 | self win |
| bulk_knn_k1000 | elasticsearch/hnsw | 1353.92 | 12421.08 | 9.174 | self win |
| bulk_knn_k1000 | lancedb/exact | 1353.92 | 6429.83 | 4.749 | self win |
| bulk_knn_k1000 | lancedb/hnsw | 1353.92 | 3562.75 | 2.631 | self win |
| bulk_knn_k1000 | mongodb/exact | 1353.92 | 9364.29 | 6.916 | self win |
| bulk_knn_k1000 | mongodb/hnsw | 1353.92 | 11010.17 | 8.132 | self win |
| bulk_knn_k1000 | pgvector/exact | 1353.92 | 4504.79 | 3.327 | self win |
| bulk_knn_k1000 | pgvector/hnsw | 1353.92 | 4331.63 | 3.199 | self win |
| bulk_knn_k1000 | qdrant/exact | 1353.92 | 12176.33 | 8.993 | self win |
| bulk_knn_k1000 | qdrant/hnsw | 1353.92 | 11639.92 | 8.597 | self win |
| bulk_knn_k1000 | redis/exact | 1353.92 | 8525.21 | 6.297 | self win |
| bulk_knn_k1000 | redis/hnsw | 1353.92 | 8788.17 | 6.491 | self win |
| bulk_knn_k1000 | self/hnsw | 1353.92 | 1225.67 | 0.905 | 僅差 |
| bulk_knn_k1000 | self_nosql/exact | 1353.92 | 1700.54 | 1.256 | 僅差 |
| bulk_knn_k1000 | sqlite_vec/exact | 1353.92 | 18319.08 | 13.530 | self win |
| bulk_knn_k200 | elasticsearch/exact | 714.67 | 4912.63 | 6.874 | self win |
| bulk_knn_k200 | elasticsearch/hnsw | 714.67 | 3544.75 | 4.960 | self win |
| bulk_knn_k200 | lancedb/exact | 714.67 | 4313.50 | 6.036 | self win |
| bulk_knn_k200 | lancedb/hnsw | 714.67 | 2490.37 | 3.485 | self win |
| bulk_knn_k200 | mongodb/exact | 714.67 | 3589.21 | 5.022 | self win |
| bulk_knn_k200 | mongodb/hnsw | 714.67 | 3302.88 | 4.622 | self win |
| bulk_knn_k200 | pgvector/exact | 714.67 | 3798.79 | 5.315 | self win |
| bulk_knn_k200 | pgvector/hnsw | 714.67 | 3776.67 | 5.285 | self win |
| bulk_knn_k200 | qdrant/exact | 714.67 | 3027.75 | 4.237 | self win |
| bulk_knn_k200 | qdrant/hnsw | 714.67 | 2926.33 | 4.095 | self win |
| bulk_knn_k200 | redis/exact | 714.67 | 2641.75 | 3.696 | self win |
| bulk_knn_k200 | redis/hnsw | 714.67 | 2662.17 | 3.725 | self win |
| bulk_knn_k200 | self/hnsw | 714.67 | 587.92 | 0.823 | 僅差 |
| bulk_knn_k200 | self_nosql/exact | 714.67 | 944.67 | 1.322 | self win |
| bulk_knn_k200 | sqlite_vec/exact | 714.67 | 5360.00 | 7.500 | self win |
| bulk_knn_where_k200 | elasticsearch/exact | 391.67 | 3485.21 | 8.898 | self win |
| bulk_knn_where_k200 | elasticsearch/hnsw | 391.67 | 3276.00 | 8.364 | self win |
| bulk_knn_where_k200 | lancedb/exact | 391.67 | 4513.25 | 11.523 | self win |
| bulk_knn_where_k200 | lancedb/hnsw | 391.67 | 3267.08 | 8.342 | self win |
| bulk_knn_where_k200 | mongodb/exact | 391.67 | 3156.96 | 8.060 | self win |
| bulk_knn_where_k200 | mongodb/hnsw | 391.67 | 3376.04 | 8.620 | self win |
| bulk_knn_where_k200 | pgvector/exact | 391.67 | 2457.50 | 6.274 | self win |
| bulk_knn_where_k200 | pgvector/hnsw | 391.67 | 2440.04 | 6.230 | self win |
| bulk_knn_where_k200 | qdrant/exact | 391.67 | 3079.25 | 7.862 | self win |
| bulk_knn_where_k200 | qdrant/hnsw | 391.67 | 2986.50 | 7.625 | self win |
| bulk_knn_where_k200 | redis/exact | 391.67 | 2440.38 | 6.231 | self win |
| bulk_knn_where_k200 | redis/hnsw | 391.67 | 2536.71 | 6.477 | self win |
| bulk_knn_where_k200 | self/hnsw | 391.67 | 2837.46 | 7.245 | self win |
| bulk_knn_where_k200 | self_nosql/exact | 391.67 | 526.87 | 1.345 | self win |
| bulk_knn_where_k200 | sqlite_vec/exact | 391.67 | 6066.21 | 15.488 | self win |
| group_by_having | elasticsearch/exact | 70.58 | 1065.67 | 15.098 | self win |
| group_by_having | elasticsearch/hnsw | 70.58 | 650.79 | 9.220 | self win |
| group_by_having | lancedb/exact | 70.58 | 6542.58 | 92.693 | self win |
| group_by_having | lancedb/hnsw | 70.58 | 6672.00 | 94.527 | self win |
| group_by_having | mongodb/exact | 70.58 | 7972.75 | 112.956 | self win |
| group_by_having | mongodb/hnsw | 70.58 | 7791.58 | 110.389 | self win |
| group_by_having | mongodb_plain/exact | 70.58 | 4160.62 | 58.946 | self win |
| group_by_having | mysql/exact | 70.58 | 11507.75 | 163.038 | self win |
| group_by_having | pgvector/exact | 70.58 | 3027.42 | 42.892 | self win |
| group_by_having | pgvector/hnsw | 70.58 | 2992.83 | 42.402 | self win |
| group_by_having | redis/exact | 70.58 | 6461.67 | 91.547 | self win |
| group_by_having | redis/hnsw | 70.58 | 6380.00 | 90.390 | self win |
| group_by_having | self/hnsw | 70.58 | 51.46 | 0.729 | 僅差 |
| group_by_having | self_nosql/exact | 70.58 | 126.92 | 1.798 | 僅差 |
| group_by_having | sqlite_vec/exact | 70.58 | 2971.92 | 42.105 | self win |
| hybrid_rrf | lancedb/exact | 1467.88 | 3778.96 | 2.574 | self win |
| hybrid_rrf | lancedb/hnsw | 1467.88 | 2299.17 | 1.566 | self win |
| hybrid_rrf | mongodb/exact | 1467.88 | 5152.25 | 3.510 | self win |
| hybrid_rrf | mongodb/hnsw | 1467.88 | 3782.96 | 2.577 | self win |
| hybrid_rrf | pgvector/exact | 1467.88 | 4043.79 | 2.755 | self win |
| hybrid_rrf | pgvector/hnsw | 1467.88 | 4019.79 | 2.739 | self win |
| hybrid_rrf | redis/exact | 1467.88 | 1044.92 | 0.712 | 僅差 |
| hybrid_rrf | redis/hnsw | 1467.88 | 1296.88 | 0.884 | 僅差 |
| hybrid_rrf | self/hnsw | 1467.88 | 1335.83 | 0.910 | 僅差 |
| hybrid_rrf | self_nosql/exact | 1467.88 | 1600.12 | 1.090 | 僅差 |
| hybrid_rrf | sqlite_vec/exact | 1467.88 | 3874.04 | 2.639 | self win |
| ingest_single_stmt | elasticsearch/exact | 145.41 | 350.35 | 2.409 | self loss |
| ingest_single_stmt | elasticsearch/hnsw | 145.41 | 498.55 | 3.428 | self loss |
| ingest_single_stmt | lancedb/exact | 145.41 | 371.14 | 2.552 | self loss |
| ingest_single_stmt | lancedb/hnsw | 145.41 | 367.63 | 2.528 | self loss |
| ingest_single_stmt | mongodb/exact | 145.41 | 860.09 | 5.915 | self loss |
| ingest_single_stmt | mongodb/hnsw | 145.41 | 1191.16 | 8.191 | self loss |
| ingest_single_stmt | mongodb_plain/exact | 145.41 | 2274.26 | 15.640 | self loss |
| ingest_single_stmt | mysql/exact | 145.41 | 739.79 | 5.087 | self loss |
| ingest_single_stmt | pgvector/exact | 145.41 | 1157.26 | 7.958 | self loss |
| ingest_single_stmt | pgvector/hnsw | 145.41 | 665.45 | 4.576 | self loss |
| ingest_single_stmt | qdrant/exact | 145.41 | 1168.41 | 8.035 | self loss |
| ingest_single_stmt | qdrant/hnsw | 145.41 | 603.55 | 4.151 | self loss |
| ingest_single_stmt | redis/exact | 145.41 | 2621.67 | 18.029 | self loss |
| ingest_single_stmt | redis/hnsw | 145.41 | 2449.33 | 16.844 | self loss |
| ingest_single_stmt | self/hnsw | 145.41 | 182.46 | 1.255 | 僅差 |
| ingest_single_stmt | self_nosql/exact | 145.41 | 171.50 | 1.179 | 僅差 |
| ingest_single_stmt | sqlite_vec/exact | 145.41 | 793.11 | 5.454 | self loss |
| mode_precision | self/hnsw | 522.96 | 545.21 | 1.043 | 僅差 |
| mode_precision | self_nosql/exact | 522.96 | 645.96 | 1.235 | self win |
| mode_recall | self/hnsw | 552.13 | 448.96 | 0.813 | 僅差 |
| mode_recall | self_nosql/exact | 552.13 | 656.08 | 1.188 | 僅差 |
| point_where | elasticsearch/exact | 246.17 | 1666.29 | 6.769 | self win |
| point_where | elasticsearch/hnsw | 246.17 | 1144.67 | 4.650 | self win |
| point_where | lancedb/exact | 246.17 | 3469.21 | 14.093 | self win |
| point_where | lancedb/hnsw | 246.17 | 2311.29 | 9.389 | self win |
| point_where | mongodb/exact | 246.17 | 1673.96 | 6.800 | self win |
| point_where | mongodb/hnsw | 246.17 | 2530.63 | 10.280 | self win |
| point_where | pgvector/exact | 246.17 | 2132.25 | 8.662 | self win |
| point_where | pgvector/hnsw | 246.17 | 2126.92 | 8.640 | self win |
| point_where | qdrant/exact | 246.17 | 650.17 | 2.641 | self win |
| point_where | qdrant/hnsw | 246.17 | 652.71 | 2.651 | self win |
| point_where | redis/exact | 246.17 | 849.83 | 3.452 | self win |
| point_where | redis/hnsw | 246.17 | 947.71 | 3.850 | self win |
| point_where | self/hnsw | 246.17 | 2095.71 | 8.513 | self win |
| point_where | self_nosql/exact | 246.17 | 331.96 | 1.349 | 僅差 |
| point_where | sqlite_vec/exact | 246.17 | 2062.38 | 8.378 | self win |
| rls_isolation | elasticsearch/exact | 110.29 | 601.79 | 5.456 | self win |
| rls_isolation | elasticsearch/hnsw | 110.29 | 479.79 | 4.350 | self win |
| rls_isolation | lancedb/exact | 110.29 | 370.75 | 3.362 | self win |
| rls_isolation | lancedb/hnsw | 110.29 | 380.67 | 3.451 | self win |
| rls_isolation | mongodb/exact | 110.29 | 2260.00 | 20.491 | self win |
| rls_isolation | mongodb/hnsw | 110.29 | 2285.87 | 20.726 | self win |
| rls_isolation | mongodb_plain/exact | 110.29 | 2254.04 | 20.437 | self win |
| rls_isolation | mysql/exact | 110.29 | 2019.88 | 18.314 | self win |
| rls_isolation | pgvector/exact | 110.29 | 1872.71 | 16.979 | self win |
| rls_isolation | pgvector/hnsw | 110.29 | 1756.50 | 15.926 | self win |
| rls_isolation | qdrant/exact | 110.29 | 1921.87 | 17.425 | self win |
| rls_isolation | qdrant/hnsw | 110.29 | 1528.46 | 13.858 | self win |
| rls_isolation | redis/exact | 110.29 | 661.83 | 6.001 | self win |
| rls_isolation | redis/hnsw | 110.29 | 655.58 | 5.944 | self win |
| rls_isolation | self/hnsw | 110.29 | 110.92 | 1.006 | 僅差 |
| rls_isolation | self_nosql/exact | 110.29 | 172.67 | 1.566 | self win |
| rls_isolation | sqlite_vec/exact | 110.29 | 998.42 | 9.052 | self win |
| scan_where_nosort_k500 | elasticsearch/exact | 438.29 | 6505.08 | 14.842 | self win |
| scan_where_nosort_k500 | elasticsearch/hnsw | 438.29 | 5486.37 | 12.518 | self win |
| scan_where_nosort_k500 | lancedb/exact | 438.29 | 1382.46 | 3.154 | self win |
| scan_where_nosort_k500 | lancedb/hnsw | 438.29 | 1390.17 | 3.172 | self win |
| scan_where_nosort_k500 | mongodb/exact | 438.29 | 1183.67 | 2.701 | self win |
| scan_where_nosort_k500 | mongodb/hnsw | 438.29 | 1190.67 | 2.717 | self win |
| scan_where_nosort_k500 | mongodb_plain/exact | 438.29 | 1161.33 | 2.650 | self win |
| scan_where_nosort_k500 | mysql/exact | 438.29 | 1582.46 | 3.611 | self win |
| scan_where_nosort_k500 | pgvector/exact | 438.29 | 1074.83 | 2.452 | self win |
| scan_where_nosort_k500 | pgvector/hnsw | 438.29 | 1029.63 | 2.349 | self win |
| scan_where_nosort_k500 | qdrant/exact | 438.29 | 4401.00 | 10.041 | self win |
| scan_where_nosort_k500 | qdrant/hnsw | 438.29 | 4402.71 | 10.045 | self win |
| scan_where_nosort_k500 | redis/exact | 438.29 | 4262.42 | 9.725 | self win |
| scan_where_nosort_k500 | redis/hnsw | 438.29 | 4132.17 | 9.428 | self win |
| scan_where_nosort_k500 | self/hnsw | 438.29 | 460.67 | 1.051 | 僅差 |
| scan_where_nosort_k500 | self_nosql/exact | 438.29 | 589.54 | 1.345 | 僅差 |
| scan_where_nosort_k500 | sqlite_vec/exact | 438.29 | 197.04 | 0.450 | self loss |
| udf_call | self/hnsw | 546.46 | 410.04 | 0.750 | 僅差 |
| vector_knn | elasticsearch/exact | 573.33 | 3607.79 | 6.293 | self win |
| vector_knn | elasticsearch/hnsw | 573.33 | 1730.92 | 3.019 | self win |
| vector_knn | lancedb/exact | 573.33 | 3225.46 | 5.626 | self win |
| vector_knn | lancedb/hnsw | 573.33 | 1544.21 | 2.693 | self win |
| vector_knn | mongodb/exact | 573.33 | 2699.46 | 4.708 | self win |
| vector_knn | mongodb/hnsw | 573.33 | 1538.75 | 2.684 | self win |
| vector_knn | pgvector/exact | 573.33 | 3313.58 | 5.780 | self win |
| vector_knn | pgvector/hnsw | 573.33 | 3139.46 | 5.476 | self win |
| vector_knn | qdrant/exact | 573.33 | 707.58 | 1.234 | self win |
| vector_knn | qdrant/hnsw | 573.33 | 814.54 | 1.421 | 僅差 |
| vector_knn | redis/exact | 573.33 | 1011.37 | 1.764 | self win |
| vector_knn | redis/hnsw | 573.33 | 1090.58 | 1.902 | self win |
| vector_knn | self/hnsw | 573.33 | 422.13 | 0.736 | 僅差 |
| vector_knn | self_nosql/exact | 573.33 | 668.17 | 1.165 | 僅差 |
| vector_knn | sqlite_vec/exact | 573.33 | 2474.21 | 4.315 | self win |
| vector_knn_where | elasticsearch/exact | 246.17 | 1666.29 | 6.769 | self win |
| vector_knn_where | elasticsearch/hnsw | 246.17 | 1144.67 | 4.650 | self win |
| vector_knn_where | lancedb/exact | 246.17 | 3469.21 | 14.093 | self win |
| vector_knn_where | lancedb/hnsw | 246.17 | 2311.29 | 9.389 | self win |
| vector_knn_where | mongodb/exact | 246.17 | 1673.96 | 6.800 | self win |
| vector_knn_where | mongodb/hnsw | 246.17 | 2530.63 | 10.280 | self win |
| vector_knn_where | pgvector/exact | 246.17 | 2132.25 | 8.662 | self win |
| vector_knn_where | pgvector/hnsw | 246.17 | 2126.92 | 8.640 | self win |
| vector_knn_where | qdrant/exact | 246.17 | 650.17 | 2.641 | self win |
| vector_knn_where | qdrant/hnsw | 246.17 | 652.71 | 2.651 | self win |
| vector_knn_where | redis/exact | 246.17 | 849.83 | 3.452 | self win |
| vector_knn_where | redis/hnsw | 246.17 | 947.71 | 3.850 | self win |
| vector_knn_where | self/hnsw | 246.17 | 2095.71 | 8.513 | self win |
| vector_knn_where | self_nosql/exact | 246.17 | 331.96 | 1.349 | 僅差 |
| vector_knn_where | sqlite_vec/exact | 246.17 | 2062.38 | 8.378 | self win |
| where_compound_count | elasticsearch/exact | 87.42 | 875.42 | 10.014 | self win |
| where_compound_count | elasticsearch/hnsw | 87.42 | 530.04 | 6.063 | self win |
| where_compound_count | lancedb/exact | 87.42 | 614.71 | 7.032 | self win |
| where_compound_count | lancedb/hnsw | 87.42 | 608.04 | 6.956 | self win |
| where_compound_count | mongodb/exact | 87.42 | 3398.79 | 38.880 | self win |
| where_compound_count | mongodb/hnsw | 87.42 | 3345.96 | 38.276 | self win |
| where_compound_count | mongodb_plain/exact | 87.42 | 2791.08 | 31.928 | self win |
| where_compound_count | mysql/exact | 87.42 | 3450.17 | 39.468 | self win |
| where_compound_count | pgvector/exact | 87.42 | 1567.13 | 17.927 | self win |
| where_compound_count | pgvector/hnsw | 87.42 | 1536.33 | 17.575 | self win |
| where_compound_count | qdrant/exact | 87.42 | 7411.92 | 84.788 | self win |
| where_compound_count | qdrant/hnsw | 87.42 | 7213.46 | 82.518 | self win |
| where_compound_count | redis/exact | 87.42 | 1075.04 | 12.298 | self win |
| where_compound_count | redis/hnsw | 87.42 | 1012.67 | 11.584 | self win |
| where_compound_count | self/hnsw | 87.42 | 78.58 | 0.899 | 僅差 |
| where_compound_count | sqlite_vec/exact | 87.42 | 1298.33 | 14.852 | self win |
