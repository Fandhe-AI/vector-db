//! ユーザーストア・Argon2id 照合・`engine::policy::PolicyContext` へのテナント導出を担う。
//!
//! `handshake.rs` の認証フロー（cleartext password 受理後）から [`verify`] が呼ばれ、
//! 成功時は `engine` クレートの `PolicyContext`（テナント境界・可視性判定の唯一の入力
//! 経路）を返す。テナントはこのユーザーストアの `tenant_id` フィールドからのみ導出し、
//! StartupMessage の `database` 等クライアント自己申告値は一切参照しない（WIRE-2）。
//! 対応: TASK-67（ポインタ: `docs/spec/05-tasks.md`。対象ビヘイビア WIRE-2, WIRE-3）。

pub mod argon2id;
pub mod blake2b;

use std::collections::HashMap;
use std::io::Read;
use std::path::Path;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use argon2id::Params;

/// 認証失敗時に課す固定遅延（WIRE-3）。
/// ポインタ: `docs/spec/04-behavior/wire-protocol.md` WIRE-3。
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
            // 構文検証（parse_phc）に加え、m_cost_kib 等の値がロード後の初回ログイン時に
            // OOM を招く極端な範囲でないかを起動時に検証する（fail-closed。実行時に
            // 初めて発覚させない。レビュー指摘: m が異常に大きい構文的に正しい PHC が
            // `hash_raw` 呼び出し時まで検出されないと、1 テナントの認証失敗ではなく
            // プロセス全体のクラッシュに波及しうる）。
            let (params, _, _) =
                argon2id::parse_phc(phc).map_err(|_| LoadError::InvalidPhc { line: line_no })?;
            argon2id::validate_params(&params)
                .map_err(|_| LoadError::InvalidPhc { line: line_no })?;
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

/// cleartext password 認証の失敗（WIRE-3: SQLSTATE `28P01`・接続切断のみで応答し、
/// 存在有無やテナント情報は含めない）。
#[derive(Debug)]
pub struct AuthFailure;

impl AuthFailure {
    /// クライアントへ返す ErrorResponse 本文（英語固定。プログラム出力文字列は
    /// 英語とする規約 [japanese-style.md] に従う）。
    pub const MESSAGE: &'static str = "password authentication failed";
}

/// 未知ユーザーに対しても実ユーザーと同一コスト・同一応答時間で照合を走らせるための
/// ダミー PHC レコード（列挙攻撃・タイミングオラクル対策。WIRE-2）。
/// salt は固定値でよい（値自体は秘密ではなく、真のユーザー kdf と同一パラメータで
/// 計算コストを揃えることだけが目的のため）。プロセス内で一度だけ生成して再利用する。
fn dummy_phc() -> &'static str {
    static DUMMY: OnceLock<String> = OnceLock::new();
    DUMMY.get_or_init(|| {
        // `RECOMMENDED_PARAMS`・固定 salt・固定パスワードはすべてコンパイル時定数
        // （untrusted 入力を一切経由しない）なので `encode_phc` は失敗しえない。
        argon2id::encode_phc(
            b"dummy-password-never-matches",
            b"0000000000000000",
            &argon2id::RECOMMENDED_PARAMS,
        )
        .expect("recommended params are always valid")
    })
}

/// cleartext password 認証を照合し、成功時は `engine::policy::PolicyContext` を返す。
/// `handshake.rs` の接続ハンドラから 1 接続につき 1 回だけ呼ばれる契約（WIRE-3:
/// 認証は 1 接続あたり 1 回のみ・失敗時に再試行させない）。
///
/// 失敗時（未知ユーザー・誤パスワードのいずれも）は固定遅延 [`AUTH_FAILURE_DELAY`]
/// を課してから `Err` を返す。未知ユーザーでも [`dummy_phc`] を用いて実ユーザーと
/// 同一コストの Argon2id 計算を必ず実行し、早期 return によるタイミング差を作らない。
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
    // `verify_phc` の失敗（構文エラー）は起動時検証済みの PHC では原理上起きないが、
    // 万一に備えて fail-closed（不一致扱い）にする。
    let password_matches = argon2id::verify_phc(phc_to_check, password).unwrap_or(false);

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

    /// 構文的には正しいが `m_cost_kib` が運用上限を超える PHC を起動時に拒否すること
    /// （レビュー指摘: `hash_raw` 呼び出し時まで検出されないと OOM でプロセス全体が
    /// 落ちるため、`load_from_file` の時点で fail-closed にする）。
    #[test]
    fn load_from_file_rejects_oversized_m_cost() {
        let dir = std::env::temp_dir().join(format!("wire-server-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let path = dir.join("users_oversized_m_cost.txt");
        std::fs::write(
            &path,
            "alice:tenant-a:$argon2id$v=19$m=999999999,t=1,p=1$c2FsdA$aGFzaA\n",
        )
        .expect("write fixture");
        let result = UserStore::load_from_file(&path);
        assert!(matches!(result, Err(LoadError::InvalidPhc { line: 1 })));
        let _ = std::fs::remove_file(&path);
    }

    /// レビュー指摘: `MAX_M_COST_KIB` を 2 GiB から 256 MiB へ引き下げた回帰確認。
    /// 新上限をわずかに超えるだけの PHC も起動時に拒否されること
    /// （旧上限では通過していた範囲）。
    #[test]
    fn load_from_file_rejects_m_cost_above_reduced_ceiling() {
        let dir = std::env::temp_dir().join(format!("wire-server-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let path = dir.join("users_reduced_m_cost_ceiling.txt");
        let over_ceiling = argon2id::MAX_M_COST_KIB as u64 + 1;
        std::fs::write(
            &path,
            format!("alice:tenant-a:$argon2id$v=19$m={over_ceiling},t=1,p=1$c2FsdA$aGFzaA\n"),
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
        // "c2FsdA" は "salt"（4 バイト）に decode される salt。"aA" は 1 バイトにしか
        // decode されない hash フィールド（`hash_raw` の下限 4 バイト未満）。
        std::fs::write(
            &path,
            "alice:tenant-a:$argon2id$v=19$m=8,t=1,p=1$c2FsdA$aA\n",
        )
        .expect("write fixture");
        let result = UserStore::load_from_file(&path);
        assert!(matches!(result, Err(LoadError::InvalidPhc { line: 1 })));
        let _ = std::fs::remove_file(&path);
    }
}
