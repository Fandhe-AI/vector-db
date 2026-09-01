# KNN 経路の段別内訳プロファイル

- ステータス: Accepted（本コミットで計測ベンチ・ADR を追加。段別内訳は本ベンチの
  実測値であり、production コード〔`crates/engine/src/`〕は無変更）
- 対応: Issue #362（`test(engine): KNN 経路の段別内訳プロファイル（走査・デコード・
  arena 構築・距離計算の切り分け）`）。親: #361「ベクトル検索・ストレージレイアウト
  最適化」Phase 5 の起点タスク
- 後続: #363（arena のテーブル世代整合キャッシュ化）・#364（連続格納レイアウト
  ADR）・#365（dot カーネル複数アキュムレータ化）・#366（距離計算/Top-k の 2 段
  分離）・#367（ANN 採否検討）の優先判断材料

## 背景

親 Issue #361 の起点として、SQL 表層経由の KNN（`SELECT id FROM docs ORDER BY
embedding <=> '<vec>' LIMIT 10`。25,000 行・dim128）のレイテンシについて、
どの段が支配的かの実測内訳が存在しなかった。先行分析では「redb per-entry 走査＋
クエリ毎の `VectorArena::build_filtered` 再構築が主因」という仮説（Issue 本文由来。
以下「先行仮説」と呼ぶ）が提示されていたが、実測の裏付けは無かった。本ベンチは
段別（redb 走査・ヘッダデコード・f32 デコード・arena 構築・距離計算・Top-k 選出）
の実測を行い、後続 Issue の優先判断材料とする。

## 計測条件（受け入れ条件 2）

- **SQL 表層経路（S0/S0'）は `PrefilterCache`（`core.rs`。`EngineCore::search`
  専用・Rust API 直呼び用）を経由しない**。`sql::exec::execute_statement` は
  クエリ毎に候補行を redb から再デコードする（`sql_c1_bench.rs` 冒頭コメントの
  既存事実と同一。`crates/engine/benches/sql_c1_bench.rs` 参照）。
- **既定 provider は `ParallelSearchProvider`**（`search_engine::default_engine()`）。
  対照として単線 `CpuScalarProvider` も測定する。
- コーパス: dim=128・テナント A 20,000 行（Public）＋テナント B 5,000 行
  （Public）＝ 計 25,000 行。RLS 述語は `PolicyContext::is_visible` をそのまま
  使う（S4/S5 のクエリはテナント A のコンテキストで実行するが、両テナントとも
  `Visibility::Public` のため全行が可視集合に含まれる）。
- 測定入口: `crates/engine/benches/knn_profile_bench.rs`（時間依存の実測本体）・
  `crates/engine/benches/harness/knn_profile.rs`（段差分・ns/行換算・行バイト列
  デコード再実装などの時間非依存ロジック）・`crates/engine/tests/
  knn_profile_accept.rs`（`make ci` からの回帰検証）。`make bench-knn-profile`
  から実行する。spec 由来の pass/fail 閾値を持たない情報提供専用のため
  `.github/workflows/*` へは配線しない（`GITHUB_ACTIONS` 環境下では起動直後に
  fail-closed で拒否する）。

## 測定設計

`crates/engine/src/` の `pub(crate)` API（`storage::decode_row_header` 等）は
ベンチ（独立コンパイル単位）から直接呼べないため、段の分離は次の 3 通りの組み合わせ
で行った:

1. pub API の差分測定（`VectorArena::build_filtered`・`EngineCore::execute_sql`・
   `SearchProvider::search`）
2. 生 `redb::Database` の直接走査（`redb` は既存 dev-dependency `=4.2.0`。新規依存
   追加なし）
3. 行レイアウト（`storage.rs` の行フォーマット v2）のベンチ内再実装
   （`harness/knn_profile.rs::decode_header_reimpl`・`decode_row_reimpl`）

行レイアウト再実装のドリフト対策として、`tests/knn_profile_accept.rs` が
`Storage::put`/`Storage::scan()`（pub API・正本）との突き合わせを回帰テスト化し、
本ベンチ自体も実行のたびに 1 件を `VectorArena::build_filtered`（S4 で構築した
「正本」デコード結果）と突き合わせる（`Storage::scan()` は `ROWS_TABLE`＝`"rows"`
テーブルのみを走査対象とし、本ベンチが `tenant::insert_rows` 経由で書き込む
カタログテーブル `user_rows/docs` は対象外のため、実行時クロスチェックの相手を
arena 側に変更している）。

DB ファイルを一度構築したのち、フェーズごとに開き直して測定した（`tests/
persistence.rs` と同じ「一度 drop してから生 `redb::Database` を再オープンする」
手順）。各段は `harness::protocol::run`（warmup 20・計測 20・中央値採用）で計測する
（ただし S4 のみ `harness::protocol::run_bounded_retain`〔`retain_capacity=0`〕を
使う。下記「実測結果」節の注記参照）。

| 段 | 内容 |
| --- | --- |
| S0 | `EngineCore::execute_sql` 経由の SQL 表層 KNN（e2e） |
| S0' | `SELECT COUNT(*) FROM docs`（走査＋ヘッダ＋RLS の SQL 経由クロスチェック） |
| S1 | 生 `redb::Database` 再オープンでの per-entry 走査のみ |
| S2 | S1 ＋ ヘッダデコード |
| S3 | S2 ＋ f32 デコード |
| S4 | `VectorArena::build_filtered`（pub API。クエリ毎再構築の実コスト） |
| S5 | 距離計算＋Top-k（`ParallelSearchProvider`・`CpuScalarProvider`） |
| S5' | 距離計算のみ（`isa::current().dot` を全行へ適用して総和。Top-k なし） |

## 実測結果

開発環境（Linux・x86_64・12 論理コア・ISA 検出 `Avx2Fma`）で `make
bench-knn-profile` を 3 回実行し、各段の中央値（p50）の変動を確認した（3 回とも
同一プロセス内・同一コーパスでの再実行。専有環境の宣言は行っていないため、
共有 CPU/IO による測定ノイズを含みうる）。3 回の実測はいずれも同一オーダーで
安定しており（S1: 47.7〜48.8 ns/行、S3: 110.8〜120.3 ns/行、S4: 174.2〜181.2 ns/行
の範囲）、代表値として直近実行分を採録する（codex-review 指摘・PR #378 追記
対応〔S4 の測定を `harness::protocol::run_bounded_retain`（`retain_capacity=0`）
へ切り替え、構築済み `VectorArena` を `elapsed()` 取得後・計測区間の外側で
毎回即座に drop するよう変更〕後の再実測。旧実測値〔S3→S4: 68.2 ns/行 等〕は
リングバッファ入れ替え（`mem::replace`）による古い arena の解放が計測区間の
内側で発生していたためのバイアスを含んでおり、本表の値へ置き換える）。

| 段 | rows | median | ns/行 |
| --- | --- | --- | --- |
| S1_redb_scan | 25,000 | 1.193ms | 47.7 |
| S2_header_decode | 25,000 | 1.455ms | 58.2 |
| S3_f32_decode | 25,000 | 3.007ms | 120.3 |
| S4_arena_build | 25,000 | 4.355ms | 174.2 |
| S5_search_parallel | 25,000 | 0.219ms | 8.8 |
| S5_search_scalar | 25,000 | 0.221ms | 8.9 |
| S5prime_distance_only | 25,000 | 0.215ms | 8.6 |
| S0_sql_e2e | 25,000 | 0.650ms | 26.0 |
| S0prime_count_star | 25,000 | 1.907ms | 76.3 |

段間差分（ns/行）:

| 差分 | ns/行 | 意味 |
| --- | --- | --- |
| S1→S2 | 10.5 | ヘッダデコード（tenant_id・visibility）分 |
| S2→S3 | 62.1 | f32 デコード（embedding 展開）分 |
| S3→S4 | 53.9 | arena への push・tenant_id 所有化・容量検証分（解放コストを含まない） |
| S5'→S5_scalar | 0.3 | Top-k を含む検索ループ全体の追加コスト分（単線・逐次走査条件下。Top-k 選出のみではなく次元検証・有限値検査・範囲取得等も含む） |

**S0・S0'・残差に関する注記（本 PR 追記）**: 本ブランチは Issue #363
（`sql/arena_cache.rs::SqlArenaCache`。SQL 表層 `SELECT` の `VectorArena` を
テーブル単位世代整合キャッシュ化）をベースへ取り込んでいる。S0（`EngineCore::
execute_sql` 経由）は本ベンチの warmup・計測フェーズを通じて同一テーブル・同一
世代のクエリを 40 回以上繰り返すため、初回以降のほぼ全呼び出しが
`SqlArenaCache` にヒットし `VectorArena` の再構築（redb 全行走査・デコード）を
スキップする。上表の S0（26.0 ns/行）・S0'（76.3 ns/行）は旧実測（それぞれ
248.3 ns/行・186.0 ns/行）から大幅に縮小しており、**「SQL 表層経由・クエリ毎に
候補行を再デコードする e2e コスト」という本ベンチが当初測定対象としていた値を
もはや表していない**（キャッシュヒット時のオーバーヘッドを測っている）。この
ため「S0 ≈ S4 + S5 + 残差」という下記「考察」節の分解は S0 に対して成立せず、
残差は実測上 0ms に丸め込まれる。S0/S0'/残差の値は参考情報として残すが、
**Issue #363 適用後の真の SQL 表層 e2e コストを測るには、キャッシュを無効化する
か世代を反復ごとに進める等の別の測定設計が必要**であり、本 PR のスコープ外
とする。S1〜S5（arena 構築・検索カーネルの内訳）は `SqlArenaCache` を経由しない
pub API 直呼びのため、この影響を受けない。

### 考察: 内訳の構成

**上記注記のとおり S0（SQL 表層 e2e）は本 PR の時点で `SqlArenaCache`
（Issue #363）のキャッシュヒットを主に測っており「S0 ≈ S4 + S5 + 残差」という
分解はもはや成立しない。以下は arena 構築（S1〜S4。`SqlArenaCache` を経由しない
pub API 直呼び）内部の内訳のみを対象とする。**

arena 構築コスト（S4、174.2 ns/行）の内訳をさらに分解すると:

- 走査そのもの（S1）: 47.7 ns/行（arena 構築の約 27%）
- ヘッダデコード追加分（S1→S2）: 10.5 ns/行（約 6%）
- f32 デコード追加分（S2→S3）: 62.1 ns/行（約 36%）
- push・所有化・容量検証分（S3→S4）: 53.9 ns/行（約 31%。本 PR で解放コストの
  混入を排除した後の値）

f32 デコード追加分（S2→S3、62.1 ns/行）が push・所有化・容量検証分
（S3→S4、53.9 ns/行）を上回り、**f32 デコードが arena 構築コストの単独最大の
内訳である**ことが、解放コストの混入を排除した実測から確認できた（旧実測では
この比較が S3→S4 側のバイアスにより断定できなかった。上記注記参照）。走査＋
ヘッダデコードのみが支配的とする先行仮説とは異なり、f32 デコード以降
（デコード・格納）の処理が arena 構築コストの過半（62.1 + 53.9 = 116.0 ns/行、
約 67%）を占めるという結論が、より確度の高い実測で裏付けられた。

arena 構築（S4）そのものが e2e に占める比率は、S0 が `SqlArenaCache` の影響を
受けるようになったため本ベンチの実測からは直接求められない。ただし
`SqlArenaCache`（Issue #363）が「クエリ毎の候補行再デコード（本ベンチが
S1〜S4 として分解した処理そのもの）を同一テーブル世代内で回避する」実装である
ことから、この構成比の分解自体が、なぜ #363 のキャッシュ導入が S0 を大幅に
縮小させたのか（248.3 ns/行 → 26.0 ns/行）を裏付ける傍証にはなっている。

### COUNT(*) クロスチェック（S0'）

`SELECT COUNT(*) FROM docs`（S0'）は走査＋ヘッダデコード＋RLS 判定までを SQL
経由で行い embedding のデコードは行わない経路である。S0' は `Ranking::Distance`
系の `SELECT` ではなく集計クエリのため `SqlArenaCache`（`VectorArena` 専用）の
対象外であり、上記の S0 に関する注記は適用されない——ただし本表の S0'
（76.3 ns/行）は旧実測（186.0 ns/行）と比べて縮小している。これは
`SqlArenaCache` 自体の影響ではなく、S4 の測定方法変更（`run_bounded_retain`
導入）が同じプロセス内の他の計測フェーズへ与えた副次的な環境差（ページ
キャッシュ・アロケータ状態等）の可能性があり、本 PR の再測定では要因の切り分け
を行っていない。ベンチ内再実装の S2（走査＋ヘッダデコード、58.2 ns/行）と
比べて S0' が依然として大きいのは、SQL 表層（パース・束縛・集計処理・
トランザクション境界等）のオーバーヘッドが乗るためで、「SQL 経由の走査＋
ヘッダデコード相当コスト」は素の redb 直接走査（S1/S2）より高くつくことを
示す一貫した結果である。S0' の計測クロージャは戻り値（`QueryResult`）を
検証せず破棄するため、SQL 経路が誤った件数を返しても計測自体は成功して
しまう。これを防ぐため、計測外で `COUNT(*)` をもう一度実行し、返却値が
`TOTAL_ROWS`（25,000）と一致することを確認したうえで不一致ならベンチを
エラー終了する fail-closed 検証を追加した（S0 の `TOP_K` 検証と同様。
codex-review 指摘・PR #378）。

### 先行仮説との突き合わせ

先行仮説では KNN 全体で約 468 ns/行、うち約 225 ns/行が COUNT(*) と共通の
走査・ヘッダデコード分とされていた。Issue #362 の当初実測（`SqlArenaCache`
〔Issue #363〕未導入時点・同一コーパス・同一環境）では S0（e2e）が
248.3 ns/行、S0'（COUNT(*)）が 186.0 ns/行であり、**先行仮説の絶対値
（468 ns/行・225 ns/行）とは概ね半分程度の乖離がある**（本 PR で `SqlArenaCache`
がベースへ取り込まれたことにより、現在の表の S0 はこの当初実測とはもはや比較
可能な値ではない。上記「S0・S0'・残差に関する注記」参照）。これは以下の
いずれか、または複合が要因と考えられる（本ベンチの結果からは切り分けられない）:

- 先行仮説の測定条件（コーパス規模・環境・埋め込み次元等）が本ベンチと異なる
- 本開発環境（NVIDIA GPU 搭載機・Vulkan backend・12 論理コア）が先行仮説の想定
  環境より高速
- 先行仮説自体が実測に基づかない見積もりだった

**絶対値は環境依存で再現性が低いため、後続 Issue の優先判断には「相対的な内訳の
構成比」（f32 デコードが arena 構築の過半・arena 構築が e2e の約 3/4）を用いる
べきである**、というのが本 Issue の結論である。

## `dot_lanes` の実アセンブリ確認（受け入れ条件 3）

`cargo bench --bench knn_profile_bench -p engine --no-run` でビルドしたバイナリ
（production コード無変更。`knn_profile_bench.rs::dot_wrapper` は
`engine::isa::current().dot(a, b)` を呼ぶだけの `#[inline(never)]` ラッパー）を
`objdump -d -M intel` で逆アセンブルした。開発環境の検出 ISA は `Avx2Fma`
（`EnvReport` 出力）のため、`engine::isa::dot_avx2_fma`（`SimdKernel::dot` から
tail-call される実体）を確認した。

観測結果:

- **FMA 命令を実際に使用している**: メインループは `vfmadd132ps`/`vfmadd231ps`
  （AVX2 の 256-bit YMM レジスタ、8 レーン f32）で、ループ 1 反復あたり YMM
  4 本（32 要素）を処理する 4 段階の unroll になっている。
- **アキュムレータは実質 1 系統（単一の依存チェーン）**: 1 反復内で
  `ymm1 = fma(a0, b0, ymm1)` → `ymm1 = fma(a1, b1, ymm1)` →
  `ymm1 = fma(a2, b2, ymm1)` → `ymm0 = ymm1`（コピー）→
  `ymm0 = fma(a3, b3, ymm0)` という順に、4 回の FMA が前段の結果に
  直接依存する形で連鎖している（`vfmadd231ps ymm1,ymm2,...` は
  `ymm1 += ymm2 * mem` で書き込み先が読み取り元でもある）。次の反復も
  この `ymm0`（前反復の最終結果）を初期値として `vfmadd132ps` するため、
  **反復をまたいでも単一の依存チェーンが継続する**。独立した複数の
  アキュムレータレジスタへ分散する最適化（FMA のレイテンシを隠す一般的な
  手法）は、この逆アセンブル結果からは確認できなかった。
- 水平和（reduction）部分（`vmovshdup`/`vshufpd`/`vshufps`/`vextractf128` の
  組み合わせ）は 256-bit → スカラーへの標準的な木構造縮約になっている。

**示唆**: FMA レイテンシ（Haswell 以降の一般的な x86_64 で概ね 4〜5 サイクル）に
対し、依存チェーンが 1 本しかないため、CPU のスーパースカラー実行資源
（複数の FMA 実行ポートを持つマイクロアーキテクチャでは並列発行が可能）を
活かしきれていない可能性がある。複数の独立したアキュムレータ（例: 2〜4 本の
YMM を並行して更新し、ループの最後に合算する設計）へ変更すれば、依存チェーンの
レイテンシをスループットで隠蔽できる余地がある——**#365（dot カーネル複数
アキュムレータ化）の実測動機として妥当な仮説**であることが本確認で裏付けられた。
ただし本 Issue はプロファイル専任であり、`isa.rs` への変更は行わない
（production コード無変更）。

## 後続 Issue への示唆

（本節は Issue #362 当初実測時点での優先判断材料。以降の状況は下記
「S0・S0'・残差に関する注記」を参照——#363 は本 PR の取り込み元ブランチで
既に実装済み〔`sql/arena_cache.rs::SqlArenaCache`〕、#367 は ADR
〔`docs/design/ann-index-adoption.md`〕として実施済みだが、いずれも本 doc の
以下の記述自体は当時の判断根拠として更新していない。）

1. **#363（arena のテーブル世代整合キャッシュ化）**: S4（arena 構築、181.2 ns/行、
   e2e の約 73%）が支配的段であることが実測で確認された。クエリ毎の再構築を
   キャッシュで回避できれば、この段のコストをほぼ丸ごと削減できる可能性がある。
   優先度は高いと考えられる。
2. **#364（連続格納レイアウト ADR）**: S2→S3 の f32 デコード分（arena 構築の
   約 3〜4 割）は依然として無視できない内訳であり、行バイト列から `Vec<f32>`
   へ展開するコスト自体が一定の比重を占める（本 PR で解放コストの混入を排除した
   実測により、f32 デコードが push・所有化・容量検証を上回る単独最大の内訳
   であることが確認できた。上記「考察: 内訳の構成」参照）。連続格納（embedding
   をあらかじめデコード済みの形でディスク上に保持する等）により f32 デコード段を
   縮小できれば、arena 構築コストの一定割合を削減できる可能性がある。
3. **#365（dot カーネル複数アキュムレータ化）**: 上記アセンブリ確認により、
   単一依存チェーンであることが確認された。ただし S5（距離計算＋Top-k、
   8.8 ns/行）は e2e 全体（248.3 ns/行）の約 4%に過ぎず、**このコーパス規模
   （25,000 行・dim128・k=10）では最適化の絶対効果は限定的**と見込まれる。
   行数・次元がより大きい場合や、S5 単体の相対比率が上がる構成（arena キャッシュ
   化〔#363〕により S4 が削減された後）では相対的に重要度が上がりうる。
4. **#366（距離計算/Top-k の 2 段分離）**: 検索ループの追加コストの分離には
   S5_scalar − S5' を用いる（S5_parallel − S5' は使わない。**訂正**:
   当初 S5_parallel と S5' の差を Top-k 選出コストの目安としていたが、
   `ParallelSearchProvider::search` にはワーカースレッド生成・行範囲分割・
   部分 Top-k・結果マージ・入力検証も含まれており、この差分には並列化コストが
   混在していて Top-k 選出（`BinaryHeap` 部分ソート）のみへは帰属できない
   〔codex-review 指摘・PR #378〕）。S5'（単線・逐次で `isa::current().dot` を
   適用するのみ）と同じ単線・逐次条件の S5_scalar（`CpuScalarProvider`、
   11.8 ns/行）との差（3.1 ns/行）であれば、並列化コストは混在しない。ただし
   `CpuScalarProvider::search` は Top-k 選出（`BinaryHeap` 部分ソート）のほかにも
   次元検証・クエリの有限値検査・行ごとの範囲取得（境界チェック付き slice）・
   スコアの有限値検査・候補構築を行うため、この差分は「Top-k 選出のみ」のコスト
   ではなく「**Top-k を含む検索ループ全体の追加コスト**」である〔codex-review
   指摘・PR #378〕。距離計算自体（8.7 ns/行）が S5_scalar の大部分を占め、Top-k
   を含む検索ループの残り部分は相対的にさらに小さい。なお S5' と S5_scalar は
   入れ子ではなく独立に計測した別経路のため、この差分（3.1 ns/行）は測定ノイズを
   含み得る〔cursor 指摘・PR #378〕。
5. **#367（ANN 採否検討）**: 本ベンチは総当たり（brute-force）経路のみを対象と
   しており、ANN との比較材料は提供しない。ただし本測定により「総当たりの
   ボトルネックは距離計算ではなく arena 構築（特に f32 デコード）にある」ことが
   分かったため、ANN 導入の効果を検討する際は「距離計算の高速化」ではなく
   「候補集合構築コストの削減」という観点を優先すべきという示唆になる。

## 再現手順

```sh
make bench-knn-profile
```

`GITHUB_ACTIONS` 環境下では拒否される（誤って CI 経由で実行された場合の
defense-in-depth）。専有環境の宣言・複数回実行による中央値の確定は運用者が
手動で行う（`docs/design/hybrid-refetch-latency.md`・`docs/design/
c1-p95-dedicated-env-reverification.md` と同じ運用方針）。

`dot_lanes` の逆アセンブル確認は次の手順で再現できる:

```sh
cargo bench --bench knn_profile_bench -p engine --no-run
nm target/release/deps/knn_profile_bench-*  | grep dot_avx2_fma
objdump -d -M intel --start-address=<addr> --stop-address=<addr+len> \
  target/release/deps/knn_profile_bench-*
```

## スコープ外（Issue #362 の対象外）

- `crates/engine/src/` への変更（本 Issue はプロファイル専任。#363〜#367 の
  各後続 Issue が個別に対応する）
- 大規模コーパス（数十万行以上）・複数 ISA（AVX-512・NEON）での実測
  （本ベンチは AVX2FMA 環境での 1 コーパス規模のみを対象とする）
- 専有環境での確定測定（本 doc の実測値は共有 CPU/IO 環境での参考値）
