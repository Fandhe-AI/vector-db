//! ユーザーストア・Argon2id 照合・`engine::policy::PolicyContext` へのテナント導出を担う。
//!
//! `handshake.rs` の認証フローから [`verify`] が呼ばれ、成功時は `engine` クレートの
//! `PolicyContext`（テナント境界・可視性判定の唯一の入力経路）を返す。テナントは
//! このユーザーストアのフィールドからのみ導出し、クライアント自己申告値は参照しない
//! （fail-closed の設計判断）。
//! 対応: TASK-67（ポインタ: `docs/spec/05-tasks.md`。対象ビヘイビア WIRE-2, WIRE-3）。

pub mod argon2id;
pub mod blake2b;

use std::collections::HashMap;
use std::io::Read;
use std::path::Path;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use argon2id::Params;

/// 認証失敗時に課す固定遅延。ポインタ: TASK-67・WIRE-3（`docs/spec/04-behavior/wire-protocol.md`）。
const AUTH_FAILURE_DELAY: Duration = Duration::from_millis(200);

/// wire プロトコルの認証失敗応答が用いる SQLSTATE（invalid_password）。
pub const SQLSTATE_INVALID_PASSWORD: &str = "28P01";

/// ユーザーストア 1 行分のレコード（`username:tenant_id:phc` 形式）。
struct UserRecord {
    tenant_id: String,
    phc: String,
}

/// サーバー起動時に `--users` で読み込むユーザーストア。認証主体からテナントを
/// 一意に導出できる唯一の情報源であり、全レコードが必ず `tenant_id` を持つ
/// （テナント無所属の特権ログイン主体を構造的に作らない。WIRE-2）。
pub struct UserStore {
    users: HashMap<String, UserRecord>,
}

/// [`UserStore::load_from_file`] のロード時検証エラー（fail-closed。起動を中断する）。
#[derive(Debug)]
pub enum LoadError {
    Io(std::io::Error),
    /// `line`: 1 起算の行番号。util フィールド値そのもの（平文パスワード等）は
    /// 含まれないため、起動時診断としてそのまま表示してよい。
    MalformedLine {
        line: usize,
    },
    DuplicateUsername {
        line: usize,
    },
    EmptyUsername {
        line: usize,
    },
    EmptyTenantId {
        line: usize,
    },
    InvalidPhc {
        line: usize,
    },
    /// `tenant_id` が `engine::policy::PolicyContext` の構築時検証（長さ上限等）を
    /// 満たさない。
    InvalidTenantId {
        line: usize,
    },
}

impl std::fmt::Display for LoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LoadError::Io(e) => write!(f, "user store: I/O error: {e}"),
            LoadError::MalformedLine { line } => {
                write!(
                    f,
                    "user store: malformed line {line} (expected username:tenant_id:phc)"
                )
            }
            LoadError::DuplicateUsername { line } => {
                write!(f, "user store: duplicate username at line {line}")
            }
            LoadError::EmptyUsername { line } => {
                write!(f, "user store: empty username at line {line}")
            }
            LoadError::EmptyTenantId { line } => {
                write!(f, "user store: empty tenant_id at line {line}")
            }
            LoadError::InvalidPhc { line } => {
                write!(f, "user store: invalid PHC hash at line {line}")
            }
            LoadError::InvalidTenantId { line } => {
                write!(
                    f,
                    "user store: tenant_id rejected by policy context at line {line}"
                )
            }
        }
    }
}

impl std::error::Error for LoadError {}

impl From<std::io::Error> for LoadError {
    fn from(e: std::io::Error) -> Self {
        LoadError::Io(e)
    }
}

impl UserStore {
    /// `username:tenant_id:phc` 形式のファイルをロードする。重複 username・空
    /// username/tenant_id・不正 PHC・`PolicyContext` が拒否する tenant_id はすべて
    /// 起動失敗として扱う（fail-closed。実行時に初めて発覚させない）。
    ///
    /// PHC の Argon2id パラメータは [`argon2id::RECOMMENDED_PARAMS`]（[`dummy_phc`] が
    /// 使う値）への完全一致のみを受理する（P0 review 指摘: 個々のレコードが
    /// 「許容範囲内」の異なる m/t/p を持てると、レコードごとに実際の KDF 計算
    /// コストが変わり、未知ユーザー用のダミー PHC とのコスト差が
    /// `AUTH_FAILURE_DELAY` の固定遅延で吸収しきれない場合、応答時間からユーザーの
    /// 存在・レコードのコスト特性が列挙されうる。「認証エラー時に存在情報を
    /// 漏らさない」テナント境界基準に反するため、範囲受理をやめ単一の既定値へ
    /// 完全一致させることで既知・未知ユーザーの KDF コストを常に一致させる）。
    pub fn load_from_file(path: &Path) -> Result<Self, LoadError> {
        let content = std::fs::read_to_string(path)?;
        let mut users = HashMap::new();

        for (idx, raw_line) in content.lines().enumerate() {
            let line_no = idx + 1;
            let line = raw_line.trim();
            if line.is_empty() {
                continue;
            }
            // `:` をちょうど 2 個含む行のみ受理する。username・tenant_id に `:` が
            // 混入した行は 4 要素以上になり自動的に拒否される（余分なロジックなしで
            // 「username への `:` 混入を起動時に拒否」を満たす）。
            let parts: Vec<&str> = line.split(':').collect();
            if parts.len() != 3 {
                return Err(LoadError::MalformedLine { line: line_no });
            }
            let (username, tenant_id, phc) = (parts[0], parts[1], parts[2]);

            if username.is_empty() {
                return Err(LoadError::EmptyUsername { line: line_no });
            }
            if tenant_id.is_empty() {
                return Err(LoadError::EmptyTenantId { line: line_no });
            }
            // 構文検証（parse_phc）に加え、Argon2id パラメータが
            // `argon2id::RECOMMENDED_PARAMS` と完全一致することを検証する
            // （fail-closed。実行時に初めて発覚させない）。この完全一致検証は
            // 以下の両方を兼ねる:
            // (1) OOM 対策: m_cost_kib 等が極端な範囲の構文的に正しい PHC が
            //     `hash_raw` 呼び出し時まで検出されずプロセス全体のクラッシュに
            //     波及することを防ぐ（`RECOMMENDED_PARAMS` が `validate_params`
            //     の運用上限内であることは
            //     `argon2id::tests::validate_params_accepts_recommended_params`
            //     で回帰確認済みのため、完全一致は自動的に上限内を意味する）
            // (2) タイミング側チャネル対策（P0 review 指摘）: 個々のレコードが
            //     異なる m/t/p を持てると、既知ユーザーの KDF コストが未知
            //     ユーザー用ダミー PHC（常に `RECOMMENDED_PARAMS`）と食い違い、
            //     `AUTH_FAILURE_DELAY` の固定遅延で吸収しきれない差が応答時間に
            //     現れてユーザーの存在・レコードのコスト特性を列挙されうる
            let (params, _, _) =
                argon2id::parse_phc(phc).map_err(|_| LoadError::InvalidPhc { line: line_no })?;
            if params != argon2id::RECOMMENDED_PARAMS {
                return Err(LoadError::InvalidPhc { line: line_no });
            }
            engine::policy::PolicyContext::new(tenant_id)
                .map_err(|_| LoadError::InvalidTenantId { line: line_no })?;

            if users
                .insert(
                    username.to_string(),
                    UserRecord {
                        tenant_id: tenant_id.to_string(),
                        phc: phc.to_string(),
                    },
                )
                .is_some()
            {
                return Err(LoadError::DuplicateUsername { line: line_no });
            }
        }

        Ok(Self { users })
    }

    /// ロード済みレコード数。ユーザー名・テナント ID 等の存在情報は含まない
    /// （wire 経路からは呼ばない。テストのフィクスチャ診断専用。Issue #172）。
    pub fn len(&self) -> usize {
        self.users.len()
    }

    /// レコードが 1 件も無いか（`len() == 0`）。`clippy::len_without_is_empty` 対応。
    pub fn is_empty(&self) -> bool {
        self.users.is_empty()
    }

    #[cfg(test)]
    fn from_records(records: Vec<(&str, &str, &str)>) -> Self {
        let mut users = HashMap::new();
        for (username, tenant_id, phc) in records {
            users.insert(
                username.to_string(),
                UserRecord {
                    tenant_id: tenant_id.to_string(),
                    phc: phc.to_string(),
                },
            );
        }
        Self { users }
    }
}

/// cleartext password 認証の失敗を表す（ポインタ: TASK-67・WIRE-3）。
#[derive(Debug)]
pub struct AuthFailure;

impl AuthFailure {
    /// クライアントへ返す ErrorResponse 本文（英語固定。プログラム出力文字列は
    /// 英語とする規約 [japanese-style.md] に従う）。
    pub const MESSAGE: &'static str = "password authentication failed";
}

/// 未知ユーザーに対しても実ユーザーと同一コストで照合を走らせるためのダミー PHC
/// レコード（ポインタ: TASK-67・WIRE-2）。salt・hash は固定値でよい（値自体は
/// 秘密ではなく、計算コストを揃えることだけが目的のため）。プロセス内で一度だけ
/// 生成して再利用する。
///
/// [`argon2id::synthesize_phc_without_hashing`] で組み立て、`encode_phc` は使わない
/// （`encode_phc` は構築時に実際に Argon2id を計算してしまい、`verify()` 側の
/// `verify_phc` 呼び出しと合わせて計算コストが二重になるため。review 指摘）。
fn dummy_phc() -> &'static str {
    static DUMMY: OnceLock<String> = OnceLock::new();
    DUMMY.get_or_init(|| {
        argon2id::synthesize_phc_without_hashing(
            &argon2id::RECOMMENDED_PARAMS,
            b"0000000000000000",
            &[0u8; 32],
        )
    })
}

/// cleartext password 認証を照合し、成功時は `engine::policy::PolicyContext` を返す。
/// `handshake.rs` の接続ハンドラから呼ばれる（ポインタ: TASK-67・WIRE-3）。
///
/// 失敗時は未知ユーザー・誤パスワードのいずれも [`dummy_phc`] を用いて実ユーザーと
/// 同一コストの Argon2id 計算を必ず実行してから固定遅延 [`AUTH_FAILURE_DELAY`] 込みで
/// `Err` を返し、早期 return によるタイミング差を作らない（列挙攻撃対策。ポインタ:
/// WIRE-2, WIRE-3）。
///
/// `argon2id::MAX_CONCURRENT_ARGON2_KDF` による同時実行数の制限（review 指摘）は
/// `hash_raw` 内部でブロッキング待機するが、既知ユーザー・未知ユーザーいずれも同一の
/// `verify_phc` 経路（＝同一のセマフォ）を通るため、待機時間もタイミング対称性を
/// 崩さない。
///
/// この対称性は、既知ユーザーの実レコードと未知ユーザー用 [`dummy_phc`] とで
/// Argon2id パラメータが常に一致していることに依存する。`UserStore::load_from_file`
/// が `argon2id::RECOMMENDED_PARAMS` への完全一致以外のレコードを起動時に拒否する
/// ことでこれを保証する（P0 review 指摘: レコードごとのパラメータ差がタイミング
/// 側チャネルになり得た）。
pub fn verify(
    store: &UserStore,
    username: &str,
    password: &[u8],
) -> Result<engine::policy::PolicyContext, AuthFailure> {
    let start = Instant::now();

    let record = store.users.get(username);
    let phc_to_check: &str = match record {
        Some(r) => &r.phc,
        None => dummy_phc(),
    };
    // `verify_phc` の失敗（KDF 実行エラー・構文エラー）は起動時検証済みの PHC では
    // 原理上起きないが、万一に備えて fail-closed（不一致扱い）にする。応答・遅延
    // 契約（`AUTH_FAILURE_DELAY` 起点は `start` のまま）は変えず、原因追跡のために
    // stderr へのみ記録する（username・password・PHC は出力しない。Issue #172:
    // 高負荷時に稀発する非再現フレークの原因切り分け用診断）。
    let password_matches = match argon2id::verify_phc(phc_to_check, password) {
        Ok(matches) => matches,
        Err(e) => {
            eprintln!(
                "wire-server: password verification could not run the KDF ({e:?}); treating as authentication failure"
            );
            false
        }
    };

    let outcome = match (record, password_matches) {
        (Some(r), true) => Ok(r.tenant_id.clone()),
        _ => Err(()),
    };

    let elapsed = start.elapsed();
    if outcome.is_err() && elapsed < AUTH_FAILURE_DELAY {
        std::thread::sleep(AUTH_FAILURE_DELAY - elapsed);
    }

    match outcome {
        Ok(tenant_id) => {
            // tenant_id はロード時に `PolicyContext::new` で検証済みだが、契約変更に
            // 備えて再検証する（fail-closed。ここでの失敗も認証失敗として扱う）。
            engine::policy::PolicyContext::new(&tenant_id).map_err(|_| AuthFailure)
        }
        Err(()) => Err(AuthFailure),
    }
}

/// salt 生成用の CSPRNG。新規依存を追加せず `/dev/urandom` から読む（Unix 前提。
/// 読めない場合はエラー終了の fail-closed。WIRE-2: salt はユーザーごとにランダム
/// 生成する）。`main.rs` の `hash-password` サブコマンドから呼ばれる。
pub fn read_urandom(len: usize) -> std::io::Result<Vec<u8>> {
    let mut f = std::fs::File::open("/dev/urandom")?;
    let mut buf = vec![0u8; len];
    f.read_exact(&mut buf)?;
    Ok(buf)
}

/// [`read_urandom`] を既定の salt 長（16 バイト）で呼ぶ補助関数。
pub fn generate_salt() -> std::io::Result<Vec<u8>> {
    read_urandom(16)
}

/// `hash-password` サブコマンドの既定パラメータ（[`argon2id::RECOMMENDED_PARAMS`]
/// の再エクスポート）。
pub const DEFAULT_PARAMS: Params = argon2id::RECOMMENDED_PARAMS;

#[cfg(test)]
mod tests {
    use super::*;

    /// テスト実行時間短縮のための軽量パラメータ（`m` は許容最小値 `8*p`）。
    /// 本番既定値は [`argon2id::RECOMMENDED_PARAMS`]（`hash-password` サブコマンド）。
    const TEST_PARAMS: Params = Params {
        m_cost_kib: 64,
        t_cost: 1,
        p_cost: 1,
    };

    fn store_with_user(username: &str, tenant_id: &str, password: &[u8]) -> UserStore {
        let salt = b"0123456789abcdef";
        let phc = argon2id::encode_phc(password, salt, &TEST_PARAMS).expect("valid phc");
        UserStore::from_records(vec![(username, tenant_id, Box::leak(phc.into_boxed_str()))])
    }

    /// `dummy_phc()` が `synthesize_phc_without_hashing`（実ハッシュ計算なし）で
    /// 組み立てられた後も、構文的に有効な PHC として `parse_phc`/`verify_phc` を
    /// 通り、かつ任意のパスワードに対して必ず不一致になること。前者が崩れると
    /// `verify()` は `unwrap_or(false)` で不一致扱いにはなるものの
    /// `verify_phc` 自体を通らず、dummy 経路が担うコスト整合性（review 指摘）が
    /// 失われる。後者が崩れると固定の合成 hash が偶然一致し得る形になっていないかを
    /// 確認する回帰チェックになる。
    #[test]
    fn dummy_phc_parses_and_never_matches_any_password() {
        assert!(
            !argon2id::verify_phc(dummy_phc(), b"anything").expect("dummy phc must parse"),
            "dummy phc must never match a real password"
        );
    }

    /// 正しいユーザー名・パスワードでの照合成功と、tenant_id がユーザーストア由来で
    /// あること（WIRE-2 の中核契約）。
    #[test]
    fn verify_succeeds_and_derives_tenant_from_store() {
        let store = store_with_user("alice", "tenant-a", b"correct-horse");
        let ctx = verify(&store, "alice", b"correct-horse").expect("valid credentials");
        assert_eq!(ctx.tenant_id(), "tenant-a");
    }

    /// 誤りパスワードは拒否され、固定遅延が実際に課されること（下限のみ確認し
    /// フレークを避ける）。
    #[test]
    fn verify_rejects_wrong_password_with_min_delay() {
        let store = store_with_user("alice", "tenant-a", b"correct-horse");
        let start = Instant::now();
        let result = verify(&store, "alice", b"wrong-password");
        assert!(result.is_err());
        assert!(start.elapsed() >= AUTH_FAILURE_DELAY);
    }

    /// 未知ユーザーも同一の失敗応答・固定遅延であること（列挙対策）。
    #[test]
    fn verify_rejects_unknown_user_with_min_delay() {
        let store = store_with_user("alice", "tenant-a", b"correct-horse");
        let start = Instant::now();
        let result = verify(&store, "bob", b"anything");
        assert!(result.is_err());
        assert!(start.elapsed() >= AUTH_FAILURE_DELAY);
    }

    /// レビュー指摘（Argon2id KDF の同時実行数上限）の再確認: `hash_raw` 内部の
    /// セマフォで多数の `verify` 呼び出しが同時に待機しても、各呼び出しの
    /// `elapsed` が `AUTH_FAILURE_DELAY` を下回ることはないこと（下限保証は
    /// セマフォの待機時間が加算される方向にしか作用しないため、構造的に破れない
    /// はずだが、実際の並行実行下で回帰確認する）。
    #[test]
    fn verify_min_delay_holds_under_concurrent_kdf_contention() {
        use std::sync::Arc;

        let store = Arc::new(store_with_user("alice", "tenant-a", b"correct-horse"));
        // `argon2id::MAX_CONCURRENT_ARGON2_KDF`（8）を上回るスレッド数で
        // セマフォを実際に競合させる。
        let thread_count = 16;

        let handles: Vec<_> = (0..thread_count)
            .map(|_| {
                let store = Arc::clone(&store);
                std::thread::spawn(move || {
                    let start = Instant::now();
                    let result = verify(&store, "bob", b"anything");
                    assert!(result.is_err());
                    start.elapsed()
                })
            })
            .collect();

        for h in handles {
            let elapsed = h.join().expect("verify thread must not panic");
            assert!(
                elapsed >= AUTH_FAILURE_DELAY,
                "elapsed {elapsed:?} must not fall below the fixed failure delay even under \
                 KDF semaphore contention"
            );
        }
    }

    /// ユーザーストアロード時、重複 username を拒否すること。
    #[test]
    fn load_from_file_rejects_duplicate_username() {
        let dir = std::env::temp_dir().join(format!("wire-server-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let path = dir.join("users_dup.txt");
        let phc = dummy_phc();
        std::fs::write(
            &path,
            format!("alice:tenant-a:{phc}\nalice:tenant-b:{phc}\n"),
        )
        .expect("write user store fixture");
        let result = UserStore::load_from_file(&path);
        assert!(matches!(
            result,
            Err(LoadError::DuplicateUsername { line: 2 })
        ));
        let _ = std::fs::remove_file(&path);
    }

    /// `:` を含む username を含む行はフィールド数不一致として拒否されること。
    #[test]
    fn load_from_file_rejects_colon_in_username() {
        let dir = std::env::temp_dir().join(format!("wire-server-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let path = dir.join("users_colon.txt");
        let phc = dummy_phc();
        std::fs::write(&path, format!("ali:ce:tenant-a:{phc}\n")).expect("write fixture");
        let result = UserStore::load_from_file(&path);
        assert!(matches!(result, Err(LoadError::MalformedLine { line: 1 })));
        let _ = std::fs::remove_file(&path);
    }

    /// 空 tenant_id を含む行を拒否すること。
    #[test]
    fn load_from_file_rejects_empty_tenant_id() {
        let dir = std::env::temp_dir().join(format!("wire-server-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let path = dir.join("users_empty_tenant.txt");
        let phc = dummy_phc();
        std::fs::write(&path, format!("alice::{phc}\n")).expect("write fixture");
        let result = UserStore::load_from_file(&path);
        assert!(matches!(result, Err(LoadError::EmptyTenantId { line: 1 })));
        let _ = std::fs::remove_file(&path);
    }

    /// 不正な PHC ハッシュを含む行を拒否すること。
    #[test]
    fn load_from_file_rejects_invalid_phc() {
        let dir = std::env::temp_dir().join(format!("wire-server-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let path = dir.join("users_bad_phc.txt");
        std::fs::write(&path, "alice:tenant-a:not-a-valid-phc\n").expect("write fixture");
        let result = UserStore::load_from_file(&path);
        assert!(matches!(result, Err(LoadError::InvalidPhc { line: 1 })));
        let _ = std::fs::remove_file(&path);
    }

    /// 構文的には正しいが `m_cost_kib` が `RECOMMENDED_PARAMS` と一致しない PHC を
    /// 起動時に拒否すること（レビュー指摘: `hash_raw` 呼び出し時まで検出されないと
    /// OOM でプロセス全体が落ちるため、`load_from_file` の時点で fail-closed に
    /// する。現在は完全一致検証がこの範囲外検出も自動的に兼ねる）。
    #[test]
    fn load_from_file_rejects_oversized_m_cost() {
        let dir = std::env::temp_dir().join(format!("wire-server-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let path = dir.join("users_oversized_m_cost.txt");
        // salt は下限（8 バイト）以上の有効値にし、m_cost の検証だけを分離する。
        std::fs::write(
            &path,
            "alice:tenant-a:$argon2id$v=19$m=999999999,t=1,p=1$AAAAAAAAAAAAAAAAAAAAAA$aGFzaA\n",
        )
        .expect("write fixture");
        let result = UserStore::load_from_file(&path);
        assert!(matches!(result, Err(LoadError::InvalidPhc { line: 1 })));
        let _ = std::fs::remove_file(&path);
    }

    /// レビュー指摘: `MAX_M_COST_KIB` を 2 GiB から 256 MiB へ引き下げた回帰確認。
    /// 新上限をわずかに超える PHC も起動時に拒否されること（`RECOMMENDED_PARAMS`
    /// への完全一致検証により、範囲外はすべて自動的に拒否される）。salt は下限
    /// （8 バイト）以上の有効値にし、m_cost の検証だけを分離する。
    #[test]
    fn load_from_file_rejects_m_cost_above_reduced_ceiling() {
        let dir = std::env::temp_dir().join(format!("wire-server-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let path = dir.join("users_reduced_m_cost_ceiling.txt");
        let over_ceiling = argon2id::MAX_M_COST_KIB as u64 + 1;
        std::fs::write(
            &path,
            format!(
                "alice:tenant-a:$argon2id$v=19$m={over_ceiling},t=1,p=1$AAAAAAAAAAAAAAAAAAAAAA$aGFzaA\n"
            ),
        )
        .expect("write fixture");
        let result = UserStore::load_from_file(&path);
        assert!(matches!(result, Err(LoadError::InvalidPhc { line: 1 })));
        let _ = std::fs::remove_file(&path);
    }

    /// 構文的には正しいが decode 後の hash フィールドが 4 バイト未満（`hash_raw` が
    /// 要求する `out_len >= 4` の下限を満たさない）の PHC を起動時に拒否すること
    /// （レビュー指摘: この検証がないと load は通過し、`verify()` が常に
    /// `hash_raw` の `InvalidParam` で不一致扱いになり続け、恒久的なログイン不能に
    /// なるのを起動時に前倒しで検出する）。
    #[test]
    fn load_from_file_rejects_hash_shorter_than_4_bytes() {
        let dir = std::env::temp_dir().join(format!("wire-server-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let path = dir.join("users_short_hash.txt");
        // "AAAAAAAAAAAAAAAAAAAAAA" は 16 バイト（下限 8 バイト以上）の有効な salt。
        // "AA" は 1 バイトにしか decode されない hash フィールド
        // （`hash_raw` の下限 4 バイト未満）。salt は有効値にして hash 長の検証だけを
        // 分離する。
        std::fs::write(
            &path,
            "alice:tenant-a:$argon2id$v=19$m=8,t=1,p=1$AAAAAAAAAAAAAAAAAAAAAA$AA\n",
        )
        .expect("write fixture");
        let result = UserStore::load_from_file(&path);
        assert!(matches!(result, Err(LoadError::InvalidPhc { line: 1 })));
        let _ = std::fs::remove_file(&path);
    }

    /// 空ファイルは「レコード 0 件のストア」として受理される現行挙動を明示する
    /// 回帰テスト（Issue #172）。この挙動自体の是非は本 Issue のスコープ外だが、
    /// 結合テスト側フィクスチャが誤って空ファイルを読んだ場合に「既知ユーザー
    /// なし → 常に `28P01`」という非再現フレークの原因になり得るため、前提を
    /// 固定して診断（`UserStore::is_empty`）の土台にする。
    #[test]
    fn load_from_file_accepts_empty_file_as_empty_store() {
        let dir = std::env::temp_dir().join(format!(
            "wire-server-test-empty-store-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system clock must not be before UNIX_EPOCH")
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let path = dir.join("users_empty.txt");
        std::fs::write(&path, "").expect("write empty fixture");
        let store = UserStore::load_from_file(&path).expect("empty file must load as empty store");
        assert!(store.is_empty());
        assert_eq!(store.len(), 0);
        let _ = std::fs::remove_file(&path);
    }

    /// レビュー指摘の再現ケース: RFC 9106 の salt 下限（8 バイト以上）未満の salt を
    /// 含む PHC を起動時に拒否すること（短 hash と同じ fail-closed の起動時検証）。
    #[test]
    fn load_from_file_rejects_salt_shorter_than_8_bytes() {
        let dir = std::env::temp_dir().join(format!("wire-server-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let path = dir.join("users_short_salt.txt");
        // "AAAAAAAAAA" は 7 バイト（下限 8 バイト未満）に decode される salt。
        // "aGFzaA" は "hash"（4 バイト、下限以上）に decode される有効な hash
        // フィールド。hash は有効値にして salt 長の検証だけを分離する。
        std::fs::write(
            &path,
            "alice:tenant-a:$argon2id$v=19$m=8,t=1,p=1$AAAAAAAAAA$aGFzaA\n",
        )
        .expect("write fixture");
        let result = UserStore::load_from_file(&path);
        assert!(matches!(result, Err(LoadError::InvalidPhc { line: 1 })));
        let _ = std::fs::remove_file(&path);
    }

    /// レビュー指摘の再現ケース: hash フィールドが decode 後
    /// `argon2id::MAX_HASH_LEN`（64 バイト）を超える PHC を起動時に拒否すること
    /// （上限がないと、認証試行のたびに大きな出力バッファを確保し続ける経路になる）。
    #[test]
    fn load_from_file_rejects_hash_longer_than_max_hash_len() {
        let dir = std::env::temp_dir().join(format!("wire-server-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let path = dir.join("users_oversized_hash.txt");
        // "AAAAAAAAAAAAAAAAAAAAAA" は 16 バイト（下限 8 バイト以上）の有効な salt。
        // 87 文字の 'A' は 65 バイト（上限 64 バイトを 1 バイト超える）に decode
        // される hash フィールド。salt は有効値にして hash 上限の検証だけを分離する。
        let oversized_hash = "A".repeat(87);
        std::fs::write(
            &path,
            format!(
                "alice:tenant-a:$argon2id$v=19$m=8,t=1,p=1$AAAAAAAAAAAAAAAAAAAAAA${oversized_hash}\n"
            ),
        )
        .expect("write fixture");
        let result = UserStore::load_from_file(&path);
        assert!(matches!(result, Err(LoadError::InvalidPhc { line: 1 })));
        let _ = std::fs::remove_file(&path);
    }

    /// レビュー指摘の再現ケース: salt フィールドが decode 後
    /// `argon2id::MAX_SALT_LEN`（64 バイト）を超える PHC を起動時に拒否すること
    /// （hash 上限と同じ形のリソース消費経路。上限がないと `compute_h0` が認証試行の
    /// たびに大きな `Vec` を確保し続ける）。
    #[test]
    fn load_from_file_rejects_salt_longer_than_max_salt_len() {
        let dir = std::env::temp_dir().join(format!("wire-server-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let path = dir.join("users_oversized_salt.txt");
        // 87 文字の 'A' は 65 バイト（上限 64 バイトを 1 バイト超える）に decode
        // される salt フィールド。"aGFzaA" は "hash"（4 バイト、下限以上上限以下）に
        // decode される有効な hash。hash は有効値にして salt 上限の検証だけを
        // 分離する。
        let oversized_salt = "A".repeat(87);
        std::fs::write(
            &path,
            format!("alice:tenant-a:$argon2id$v=19$m=8,t=1,p=1${oversized_salt}$aGFzaA\n"),
        )
        .expect("write fixture");
        let result = UserStore::load_from_file(&path);
        assert!(matches!(result, Err(LoadError::InvalidPhc { line: 1 })));
        let _ = std::fs::remove_file(&path);
    }

    /// P0 review 指摘の再現ケース: 旧仕様では「運用上限内」なら受理していた
    /// m/t/p（ここでは m=8,t=1,p=1。`RECOMMENDED_PARAMS` とは異なる値）を、
    /// `RECOMMENDED_PARAMS` への完全一致要求により起動時に拒否すること。個々の
    /// レコードが異なる KDF コストを持てると、未知ユーザー用ダミー PHC との
    /// コスト差からユーザーの存在・レコードのコスト特性が列挙されうるため
    /// （タイミング側チャネル対策）。
    #[test]
    fn load_from_file_rejects_in_range_params_that_are_not_canonical() {
        let dir = std::env::temp_dir().join(format!("wire-server-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let path = dir.join("users_noncanonical_params.txt");
        std::fs::write(
            &path,
            "alice:tenant-a:$argon2id$v=19$m=8,t=1,p=1$AAAAAAAAAAAAAAAAAAAAAA$aGFzaA\n",
        )
        .expect("write fixture");
        let result = UserStore::load_from_file(&path);
        assert!(matches!(result, Err(LoadError::InvalidPhc { line: 1 })));
        let _ = std::fs::remove_file(&path);
    }

    /// PHC パラメータが `RECOMMENDED_PARAMS` に完全一致するレコードは受理される
    /// こと（拒否側だけでなく許容側の境界も明示する）。
    #[test]
    fn load_from_file_accepts_params_equal_to_recommended_params() {
        let dir = std::env::temp_dir().join(format!("wire-server-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let path = dir.join("users_canonical_accept.txt");
        let phc = dummy_phc();
        std::fs::write(&path, format!("alice:tenant-a:{phc}\n")).expect("write fixture");
        let result = UserStore::load_from_file(&path);
        assert!(
            result.is_ok(),
            "RECOMMENDED_PARAMS-equal PHC must be accepted"
        );
        let _ = std::fs::remove_file(&path);
    }

    /// P0 review 指摘の中核となる回帰確認: `load_from_file` を通過した実ユーザー
    /// レコードの Argon2id パラメータが、未知ユーザー用ダミー PHC（[`dummy_phc`]）の
    /// パラメータと常に一致すること。実時間の統計比較ではなく、両者が経由する
    /// PHC のパラメータそのものが一致することを検証することで、タイミング側
    /// チャネルの原因（レコードごとの KDF コスト差）を構造的に排除したことを
    /// 確認する。
    #[test]
    fn load_from_file_enforces_same_kdf_params_as_dummy_phc() {
        let dir = std::env::temp_dir().join(format!("wire-server-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let path = dir.join("users_matches_dummy_params.txt");
        let salt = b"0123456789abcdef";
        let real_phc = argon2id::encode_phc(b"correct-horse", salt, &argon2id::RECOMMENDED_PARAMS)
            .expect("valid recommended params");
        std::fs::write(&path, format!("alice:tenant-a:{real_phc}\n")).expect("write fixture");

        let store = UserStore::load_from_file(&path).expect("recommended params must be accepted");
        let real_record_phc = &store.users.get("alice").expect("alice loaded").phc;

        let (real_params, _, _) = argon2id::parse_phc(real_record_phc).expect("valid phc");
        let (dummy_params, _, _) = argon2id::parse_phc(dummy_phc()).expect("valid phc");
        assert_eq!(
            real_params, dummy_params,
            "known-user record params must equal the dummy PHC params (P0: no per-record KDF \
             cost variance that could leak user existence via timing)"
        );

        let _ = std::fs::remove_file(&path);
    }
}
