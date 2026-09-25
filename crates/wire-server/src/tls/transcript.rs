//! TLS 1.3 の transcript hash（RFC 8446 §4.4.1。TASK-228・WIRE-9・HTTP-10
//! ポインタ。Issue #964・親 #941）。
//!
//! ハンドシェイクメッセージ列（ClientHello から Finished まで）の累積
//! SHA-256 を、受信側の実データ（[`super::handshake::HandshakeBuffer`] が
//! 返す [`super::handshake::RawHandshake`]）から直接構成する。メッセージ
//! 本体をバッファせず [`engine::crypto::sha256::Sha256`]（ストリーミング・
//! `Clone` 可）へ逐次投入するため、Certificate 長に依存しない O(1) メモリで
//! 動作する。
//!
//! **更新順序の単一情報源**: [`Transcript`] はサーバー側で受理し得る
//! ハンドシェイクメッセージの並び（ClientHello →
//! ［HelloRetryRequest → ClientHello］→ ServerHello → EncryptedExtensions →
//! Certificate → CertificateVerify → server Finished → client Finished）を
//! 唯一保持する。ハンドシェイク状態機械（#965）は独自の順序表を持たず、
//! [`Transcript::expected_next`] と各 `append_*` の `Result` のみを頼りに
//! 遷移を判断する契約とする（poison 後は `expected_next` も `Err` を返す）。クライアント証明書・PSK／0-RTT・KeyUpdate は
//! 親 Issue #941 の方針により対象外。
//!
//! **HelloRetryRequest 時の transcript 再構成**（RFC 8446 §4.4.1）:
//! HRR を経た接続では、1 回目の ClientHello を合成メッセージ
//! `message_hash`（`msg_type=254`・3 バイト長 `0x000020`・
//! `Hash(ClientHello1)` の 32 バイト）へ置き換えてから HRR 以降を続ける。
//! `254` は [`super::handshake::HandshakeType`] の閉じた語彙（受信側では
//! 拒否対象）に追加しない合成値であり、[`Transcript`] が内部でのみ書く。
//!
//! 順序外の append・期待外の `msg_type`・チェックポイントの時点違いは
//! いずれも [`TranscriptError`] を返し、以後この [`Transcript`] は
//! poison（[`super::handshake::HandshakeBuffer`] と同じ fail-closed 流儀。
//! 一度エラーを返したら以後の呼び出しも同じ理由のエラーを返し続ける）。

use super::client_hello::HELLO_RETRY_REQUEST_RANDOM;
use super::handshake::{HandshakeType, RawHandshake};
use super::record::AlertDescription;
use engine::crypto::sha256::Sha256;
use std::fmt;

/// 合成メッセージ `message_hash` の `HandshakeType`（RFC 8446 §4.4.1）。
/// 受信側の語彙（[`HandshakeType::try_from`]）には含めない合成値のため、
/// ここでのみ生バイトとして扱う。
const MESSAGE_HASH_TYPE: u8 = 254;

/// [`Transcript`] が受理してきたメッセージ列の現在位置。次に受理できる
/// メッセージ種別は [`Transcript::expected_next`] が返す。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    /// まだ何も投入していない（ClientHello 待ち）。
    Start,
    /// 直近に ClientHello を投入した（HelloRetryRequest 経由なら 2 回目）。
    ClientHello,
    /// 直近に HelloRetryRequest を投入した（2 回目の ClientHello 待ち）。
    HelloRetryRequest,
    ServerHello,
    EncryptedExtensions,
    Certificate,
    CertificateVerify,
    ServerFinished,
    /// client Finished まで投入済み（ハンドシェイク完了。以後の append は拒否）。
    ClientFinished,
}

/// [`Transcript::expected_next`] が返す、次に受理できるメッセージ種別。
/// ClientHello 直後だけ HelloRetryRequest／ServerHello の 2 択になる
/// （A4 受け入れ条件）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ExpectedNext {
    ClientHello,
    HelloRetryRequestOrServerHello,
    ServerHello,
    EncryptedExtensions,
    Certificate,
    CertificateVerify,
    ServerFinished,
    ClientFinished,
    /// ハンドシェイク完了。以後の append はすべて拒否される。
    Complete,
}

/// [`Transcript`] の失敗理由。受信バイト列・ハッシュ値そのものは保持しない
/// （エラー経由の情報漏えい防止）。いずれも状態外・想定外のメッセージであり、
/// alert としては `unexpected_message`（10）へ一律写像する。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TranscriptError {
    /// 順序外の append、または完了後・poison 後の append。
    OutOfOrder,
    /// 期待した `msg_type` と異なる、または HelloRetryRequest の
    /// `random` が [`HELLO_RETRY_REQUEST_RANDOM`] と一致しない。
    UnexpectedMessage,
    /// HelloRetryRequest の `random` を読み取るための最小長（34 バイト。
    /// legacy_version 2 バイト + random 32 バイト）に本文が満たない。
    Malformed,
}

impl TranscriptError {
    pub fn alert_description(self) -> AlertDescription {
        AlertDescription::UnexpectedMessage
    }
}

impl fmt::Display for TranscriptError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TranscriptError::OutOfOrder => write!(f, "handshake message out of order"),
            TranscriptError::UnexpectedMessage => write!(f, "unexpected handshake message"),
            TranscriptError::Malformed => write!(f, "handshake message too short to inspect"),
        }
    }
}

impl std::error::Error for TranscriptError {}

/// ハンドシェイクメッセージ列の累積 SHA-256（RFC 8446 §4.4.1）。
///
/// メッセージ本体は保持せず、投入のたびに [`Sha256::update`] へ流し込む。
/// 名前付きチェックポイント（`hash_through_*`）は該当ステップを通過した
/// 直後にのみ取得できる（時点違いの取得を型・状態で防ぐ設計。誤った時点の
/// `current_hash()` のような汎用 getter は公開しない）。
pub struct Transcript {
    hasher: Sha256,
    step: Step,
    /// HelloRetryRequest を経由済みか（1 接続 1 回のみ許可する）。
    hrr_done: bool,
    /// 一度でも `Err` を返したら true になり、以後すべての呼び出しが
    /// `Err(TranscriptError::OutOfOrder)` を返し続ける（fail-closed）。
    poisoned: bool,
}

impl fmt::Debug for Transcript {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Transcript")
            .field("step", &self.step)
            .field("hrr_done", &self.hrr_done)
            .field("poisoned", &self.poisoned)
            .finish()
    }
}

impl Default for Transcript {
    fn default() -> Self {
        Self::new()
    }
}

impl Transcript {
    pub fn new() -> Self {
        Transcript {
            hasher: Sha256::new(),
            step: Step::Start,
            hrr_done: false,
            poisoned: false,
        }
    }

    /// 次に受理できるメッセージ種別（#965 の状態機械が参照する契約）。
    ///
    /// poison 済み（append・チェックポイントのいずれかが一度でも `Err` を
    /// 返した後）は `Err(TranscriptError::OutOfOrder)` を返す。状態機械は
    /// この問い合わせだけで遷移を判断するため、失敗済みの transcript に対して
    /// 正常な次メッセージを返すと継続可能と誤認させてしまう（fail-closed
    /// 契約違反）。読み取り専用の問い合わせのため、ここでは poison 状態を
    /// 変更しない。
    pub fn expected_next(&self) -> Result<ExpectedNext, TranscriptError> {
        self.check_not_poisoned()?;
        Ok(match self.step {
            Step::Start => ExpectedNext::ClientHello,
            Step::ClientHello if !self.hrr_done => ExpectedNext::HelloRetryRequestOrServerHello,
            Step::ClientHello => ExpectedNext::ServerHello,
            Step::HelloRetryRequest => ExpectedNext::ClientHello,
            Step::ServerHello => ExpectedNext::EncryptedExtensions,
            Step::EncryptedExtensions => ExpectedNext::Certificate,
            Step::Certificate => ExpectedNext::CertificateVerify,
            Step::CertificateVerify => ExpectedNext::ServerFinished,
            Step::ServerFinished => ExpectedNext::ClientFinished,
            Step::ClientFinished => ExpectedNext::Complete,
        })
    }

    fn fail(&mut self, err: TranscriptError) -> TranscriptError {
        self.poisoned = true;
        err
    }

    fn check_not_poisoned(&self) -> Result<(), TranscriptError> {
        if self.poisoned {
            Err(TranscriptError::OutOfOrder)
        } else {
            Ok(())
        }
    }

    /// 生バイト列（ヘッダ＋本文）をハッシュへ投入する。
    /// [`RawHandshake::header`] で 4 バイトヘッダだけを組み立て、ヘッダ・本文
    /// の順に [`Sha256::update`] へ直接渡す（本文をヘッダ込みの別バッファへ
    /// コピーしない。モジュール doc の O(1) メモリ契約。PR #1033 レビュー
    /// 指摘）。ヘッダは `(msg_type, body.len())` から一意に決まるため、
    /// 受信した生バイト列を欠落なく反映できる（`handshake.rs` の
    /// [`RawHandshake`] ドキュメンテーションコメント参照）。本文長が 24 ビット
    /// に収まらない場合は `Malformed` で poison する。
    fn absorb(&mut self, raw: &RawHandshake) -> Result<(), TranscriptError> {
        let header = raw
            .header()
            .map_err(|_| self.fail(TranscriptError::Malformed))?;
        self.hasher.update(&header);
        self.hasher.update(&raw.body);
        Ok(())
    }

    /// [`Step::Start`]（初回）または [`Step::HelloRetryRequest`]（HRR 後の
    /// 2 回目）でのみ受理する ClientHello。
    pub fn append_client_hello(&mut self, raw: &RawHandshake) -> Result<(), TranscriptError> {
        self.check_not_poisoned()?;
        if !matches!(self.step, Step::Start | Step::HelloRetryRequest) {
            return Err(self.fail(TranscriptError::OutOfOrder));
        }
        if raw.msg_type != HandshakeType::ClientHello {
            return Err(self.fail(TranscriptError::UnexpectedMessage));
        }
        self.absorb(raw)?;
        self.step = Step::ClientHello;
        Ok(())
    }

    /// HelloRetryRequest（`msg_type` は `ServerHello` と同じ。RFC 8446 §4.1.4
    /// の注記どおり `random` フィールドで見分ける）。1 接続 1 回のみ受理する。
    /// 受理時に transcript を `message_hash` 合成メッセージへ置換する
    /// （RFC 8446 §4.4.1。モジュール doc 参照）。
    pub fn append_hello_retry_request(
        &mut self,
        raw: &RawHandshake,
    ) -> Result<(), TranscriptError> {
        self.check_not_poisoned()?;
        if self.step != Step::ClientHello || self.hrr_done {
            return Err(self.fail(TranscriptError::OutOfOrder));
        }
        if raw.msg_type != HandshakeType::ServerHello {
            return Err(self.fail(TranscriptError::UnexpectedMessage));
        }
        // ServerHello 本文の先頭 2 バイトは legacy_version、続く 32 バイトが
        // random（RFC 8446 §4.1.3）。真の ServerHello を HRR として誤って
        // message_hash 置換してしまわないよう、取り違え防止のため random を
        // 照合する（untrusted 入力のため範囲外アクセスは `get` で拒否する）。
        let random = raw
            .body
            .get(2..34)
            .ok_or_else(|| self.fail(TranscriptError::Malformed))?;
        if random != HELLO_RETRY_REQUEST_RANDOM {
            return Err(self.fail(TranscriptError::UnexpectedMessage));
        }

        // message_hash 置換: Hash(CH1) を計算してから新しいハッシュ状態を
        // message_hash ヘッダ＋Hash(CH1) から始め、続けて HRR 自体を投入する。
        let ch1_hash = self.hasher.clone().finalize();
        let mut replaced = Sha256::new();
        replaced.update(&[MESSAGE_HASH_TYPE, 0x00, 0x00, 0x20]);
        replaced.update(&ch1_hash);
        self.hasher = replaced;
        self.absorb(raw)?;

        self.hrr_done = true;
        self.step = Step::HelloRetryRequest;
        Ok(())
    }

    pub fn append_server_hello(&mut self, raw: &RawHandshake) -> Result<(), TranscriptError> {
        self.check_not_poisoned()?;
        if self.step != Step::ClientHello {
            return Err(self.fail(TranscriptError::OutOfOrder));
        }
        if raw.msg_type != HandshakeType::ServerHello {
            return Err(self.fail(TranscriptError::UnexpectedMessage));
        }
        // HelloRetryRequest は ServerHello と同じ `msg_type` を使う
        // （RFC 8446 §4.1.4）ため、`random` が HRR の定数と一致する
        // メッセージをここで通常の ServerHello として吸収してしまうと
        // message_hash 置換（`append_hello_retry_request`）が漏れ、
        // 誤った transcript hash から traffic secret が導出される。
        // 呼び出し元は HRR を `append_hello_retry_request` へ回すべきで
        // あり、ここでは random が HRR 定数と一致する場合のみ拒否する
        // （本文が短く random を読み取れない場合は通常の ServerHello として
        // 扱い、後続の処理・上位層の妥当性検証に委ねる）。
        if let Some(random) = raw.body.get(2..34) {
            if random == HELLO_RETRY_REQUEST_RANDOM {
                return Err(self.fail(TranscriptError::UnexpectedMessage));
            }
        }
        self.absorb(raw)?;
        self.step = Step::ServerHello;
        Ok(())
    }

    /// ClientHello..ServerHello の transcript hash（[`HandshakeSecret::
    /// traffic_secrets`](super::key_schedule::HandshakeSecret::traffic_secrets)
    /// の入力）。ServerHello を通過した直後にのみ取得できる。
    pub fn hash_through_server_hello(&mut self) -> Result<[u8; 32], TranscriptError> {
        self.checkpoint(Step::ServerHello)
    }

    pub fn append_encrypted_extensions(
        &mut self,
        raw: &RawHandshake,
    ) -> Result<(), TranscriptError> {
        self.check_not_poisoned()?;
        if self.step != Step::ServerHello {
            return Err(self.fail(TranscriptError::OutOfOrder));
        }
        if raw.msg_type != HandshakeType::EncryptedExtensions {
            return Err(self.fail(TranscriptError::UnexpectedMessage));
        }
        self.absorb(raw)?;
        self.step = Step::EncryptedExtensions;
        Ok(())
    }

    pub fn append_certificate(&mut self, raw: &RawHandshake) -> Result<(), TranscriptError> {
        self.check_not_poisoned()?;
        if self.step != Step::EncryptedExtensions {
            return Err(self.fail(TranscriptError::OutOfOrder));
        }
        if raw.msg_type != HandshakeType::Certificate {
            return Err(self.fail(TranscriptError::UnexpectedMessage));
        }
        self.absorb(raw)?;
        self.step = Step::Certificate;
        Ok(())
    }

    /// ClientHello..Certificate の transcript hash（CertificateVerify の
    /// 署名対象。#961 が利用）。Certificate を通過した直後にのみ取得できる。
    pub fn hash_through_certificate(&mut self) -> Result<[u8; 32], TranscriptError> {
        self.checkpoint(Step::Certificate)
    }

    pub fn append_certificate_verify(&mut self, raw: &RawHandshake) -> Result<(), TranscriptError> {
        self.check_not_poisoned()?;
        if self.step != Step::Certificate {
            return Err(self.fail(TranscriptError::OutOfOrder));
        }
        if raw.msg_type != HandshakeType::CertificateVerify {
            return Err(self.fail(TranscriptError::UnexpectedMessage));
        }
        self.absorb(raw)?;
        self.step = Step::CertificateVerify;
        Ok(())
    }

    /// ClientHello..CertificateVerify の transcript hash（server Finished の
    /// verify_data 算出対象。CertificateVerify を通過した直後にのみ取得できる）。
    pub fn hash_through_certificate_verify(&mut self) -> Result<[u8; 32], TranscriptError> {
        self.checkpoint(Step::CertificateVerify)
    }

    pub fn append_server_finished(&mut self, raw: &RawHandshake) -> Result<(), TranscriptError> {
        self.check_not_poisoned()?;
        if self.step != Step::CertificateVerify {
            return Err(self.fail(TranscriptError::OutOfOrder));
        }
        if raw.msg_type != HandshakeType::Finished {
            return Err(self.fail(TranscriptError::UnexpectedMessage));
        }
        self.absorb(raw)?;
        self.step = Step::ServerFinished;
        Ok(())
    }

    /// ClientHello..server Finished の transcript hash（
    /// [`MasterSecret::application_traffic_secrets`]
    /// (super::key_schedule::MasterSecret::application_traffic_secrets) と
    /// client Finished の verify_data 検証対象。server Finished を通過した
    /// 直後にのみ取得できる）。
    pub fn hash_through_server_finished(&mut self) -> Result<[u8; 32], TranscriptError> {
        self.checkpoint(Step::ServerFinished)
    }

    pub fn append_client_finished(&mut self, raw: &RawHandshake) -> Result<(), TranscriptError> {
        self.check_not_poisoned()?;
        if self.step != Step::ServerFinished {
            return Err(self.fail(TranscriptError::OutOfOrder));
        }
        if raw.msg_type != HandshakeType::Finished {
            return Err(self.fail(TranscriptError::UnexpectedMessage));
        }
        self.absorb(raw)?;
        self.step = Step::ClientFinished;
        Ok(())
    }

    /// 名前付きチェックポイントの共通実装。時点違い（`self.step != want`）も
    /// モジュール doc の poison 契約（一度でも `Err` を返したら以後すべて
    /// `OutOfOrder`）の対象であるため `&mut self` を取り、失敗時は必ず
    /// [`Transcript::fail`] を経由して poison する（`&self` のまま `Err` だけ
    /// 返すと、以後も正しい append・チェックポイント取得が継続できてしまい
    /// fail-closed 契約を満たさない）。
    fn checkpoint(&mut self, want: Step) -> Result<[u8; 32], TranscriptError> {
        self.check_not_poisoned()?;
        if self.step != want {
            return Err(self.fail(TranscriptError::OutOfOrder));
        }
        Ok(self.hasher.clone().finalize())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tls::handshake::{HandshakeType, MAX_HANDSHAKE_WIRE_BODY_LEN};

    fn hex_decode(s: &str) -> Vec<u8> {
        let s: String = s.chars().filter(|c| !c.is_whitespace()).collect();
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).expect("valid hex in RFC fixture"))
            .collect()
    }

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    /// `RawHandshake` はヘッダ込みのバイト列を要求しないため（`msg_type` は
    /// 別フィールド）、RFC トレースのヘッダ 4 バイトを取り除いて本文だけを渡す。
    fn raw(msg_type: HandshakeType, full_with_header_hex: &str) -> RawHandshake {
        let full = hex_decode(full_with_header_hex);
        RawHandshake {
            msg_type,
            body: full[4..].to_vec(),
        }
    }

    // RFC 8448 §3（Simple 1-RTT Handshake。IETF の公開文書由来。`docs/spec` の
    // 内容ではない）の ClientHello・ServerHello。`tls_key_schedule_rfc8448.rs`
    // と同じバイト列。
    const CLIENT_HELLO_1RTT: &str = "010000c00303cb34ecb1e78163ba1c38c6dacb196a6dffa21a8d9912ec18a2ef6283024dece7000006130113031302010000910000000b0009000006736572766572ff01000100000a0014001200\
        1d0017001800190100010101020103010400230000003300260024001d002099381de560e4bd43d23d8e435a7dbafeb3c06e51c13cae4d5413691e529aaf2c002b0003020304000d0020001e04\
        0305030603020308040805080604010501060102010402050206020202002d000201\
        01001c00024001";
    const SERVER_HELLO_1RTT: &str = "020000560303a6af06a4121860dc5e6e60249cd34c95930c8ac5cb1434dac155772ed3e2692800130100002e00330024001d0020c9828876112095fe66762bdbf7c672e156d6cc253b833df1dd69b1b04e751f0f002b00020304";

    // RFC 8448 §5（HelloRetryRequest。IETF の公開文書由来）の 1 回目
    // ClientHello・HelloRetryRequest（ServerHello 形。random =
    // HELLO_RETRY_REQUEST_RANDOM）・2 回目 ClientHello・実際の ServerHello。
    const CLIENT_HELLO_1_HRR: &str = "010000b00303b0b1c5a5aa37c5919f2ed1d5c6fff7fcb7849716945a2b8cee9258a346677b6f000006130113031302010000810000000b0009000006736572766572ff01000100000a00080006001d00170018003300260024001d0020e8e8e3f3b93a25ed97a14a7dcacb8a272c6288e585c6484d05262fcad062ad1f002b0003020304000d0020001e040305030603020308040805080604010501060102010402050206020202002d00020101001c00024001";
    const HELLO_RETRY_REQUEST_HRR: &str = "020000ac0303cf21ad74e59a6111be1d8c021e65b891c2a211167abb8c5e079e09e2c8a8339c001301000084003300020017002c0074007271dcd04bb88bc3189119398a00000000eefafc76c146b823b096f8aacad365dd0030953f4edf625636e5f21bb2e23fcc654b1b5b40318d10d137abcbb87574e36e8a1f025f7dfa5d6e50781b5eda4aa15b0c8be778257d16aa3030e9e7841dd9e4c0342267e8ca0caf571fb2b7cff0f934b0002b00020304";
    const CLIENT_HELLO_2_HRR: &str = "010001fc0303b0b1c5a5aa37c5919f2ed1d5c6fff7fcb7849716945a2b8cee9258a346677b6f000006130113031302010001cd0000000b0009000006736572766572ff01000100000a00080006001d001700180033004700450017004104a6da7392ec591e17abfd535964b99894d13befb221b3def2ebe3830eac8f0151812677c4d6d2237e85cf01d6910cfb83954e76ba7352830534159897e8065780002b0003020304000d0020001e040305030603020308040805080604010501060102010402050206020202002c0074007271dcd04bb88bc3189119398a00000000eefafc76c146b823b096f8aacad365dd0030953f4edf625636e5f21bb2e23fcc654b1b5b40318d10d137abcbb87574e36e8a1f025f7dfa5d6e50781b5eda4aa15b0c8be778257d16aa3030e9e7841dd9e4c0342267e8ca0caf571fb2b7cff0f934b0002d00020101001c00024001001500af00000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000";
    const SERVER_HELLO_HRR: &str = "020000770303bb341d847fd789c47c387172dc0c9bf147fccacb5043d86ca4c598d3ff571b9800130100004f003300450017004104583e054b7a66672ae020ad9d2686fcc85b5ad41a134a0f03ee72b893052bd85b4c8de6776f5b04ac07d83540eab3e3d9c547bc6528c4317d294686093a6cad7d002b00020304";

    #[test]
    fn simple_1rtt_transcript_matches_rfc8448_checkpoints() {
        let mut t = Transcript::new();
        assert_eq!(t.expected_next(), Ok(ExpectedNext::ClientHello));

        t.append_client_hello(&raw(HandshakeType::ClientHello, CLIENT_HELLO_1RTT))
            .expect("valid ClientHello");
        assert_eq!(
            t.expected_next(),
            Ok(ExpectedNext::HelloRetryRequestOrServerHello)
        );

        t.append_server_hello(&raw(HandshakeType::ServerHello, SERVER_HELLO_1RTT))
            .expect("valid ServerHello");
        assert_eq!(t.expected_next(), Ok(ExpectedNext::EncryptedExtensions));

        let th_ch_sh = t.hash_through_server_hello().expect("checkpoint reached");
        assert_eq!(
            hex(&th_ch_sh),
            "860c06edc07858ee8e78f0e7428c58edd6b43f2ca3e6e95f02ed063cf0e1cad8"
        );
    }

    // A2: RFC 8448 §5（HelloRetryRequest。IETF の公開文書由来）のトレースで、
    // `message_hash` 置換後の transcript hash（`th_ch_sh` 相当。トレース中の
    // 「derive secret "tls13 c hs traffic"」の `hash` フィールド値）が実際の
    // ClientHello1→HelloRetryRequest→ClientHello2→ServerHello の並びから
    // 再現できることを固定する。この値は message_hash 置換が正しく行われた
    // 場合にのみ一致するため、置換ロジックの非 vacuous な検証になる。
    #[test]
    fn hello_retry_request_transcript_matches_rfc8448_message_hash_replacement() {
        let mut t = Transcript::new();
        t.append_client_hello(&raw(HandshakeType::ClientHello, CLIENT_HELLO_1_HRR))
            .expect("valid ClientHello1");
        t.append_hello_retry_request(&raw(HandshakeType::ServerHello, HELLO_RETRY_REQUEST_HRR))
            .expect("valid HelloRetryRequest");
        assert_eq!(t.expected_next(), Ok(ExpectedNext::ClientHello));

        t.append_client_hello(&raw(HandshakeType::ClientHello, CLIENT_HELLO_2_HRR))
            .expect("valid ClientHello2");
        assert_eq!(t.expected_next(), Ok(ExpectedNext::ServerHello));

        t.append_server_hello(&raw(HandshakeType::ServerHello, SERVER_HELLO_HRR))
            .expect("valid ServerHello");

        let th = t.hash_through_server_hello().expect("checkpoint reached");
        assert_eq!(
            hex(&th),
            "8aa8e828ec2f8a884fec95a3139de01c15a3daa7ff5bfc3f4bfcc21b438d7bf8"
        );
    }

    #[test]
    fn out_of_order_append_is_rejected_and_poisons() {
        let mut t = Transcript::new();
        // ClientHello の前に ServerHello を投入 → OutOfOrder。
        let err = t
            .append_server_hello(&raw(HandshakeType::ServerHello, SERVER_HELLO_1RTT))
            .unwrap_err();
        assert_eq!(err, TranscriptError::OutOfOrder);
        assert_eq!(err.alert_description(), AlertDescription::UnexpectedMessage);

        // poison 後は正しい順序の呼び出しも拒否され続ける。
        let err2 = t
            .append_client_hello(&raw(HandshakeType::ClientHello, CLIENT_HELLO_1RTT))
            .unwrap_err();
        assert_eq!(err2, TranscriptError::OutOfOrder);
    }

    // PR #1033 review（P1）: poison 後の `expected_next()` が正常な遷移先を
    // 返し続けると、状態機械が失敗済みの transcript を継続可能と誤認する。
    // append 失敗による poison 後は `Err(OutOfOrder)` を返すことを固定する。
    #[test]
    fn expected_next_is_rejected_after_append_failure_poisons() {
        let mut t = Transcript::new();
        t.append_client_hello(&raw(HandshakeType::ClientHello, CLIENT_HELLO_1RTT))
            .expect("valid ClientHello");
        assert_eq!(
            t.expected_next(),
            Ok(ExpectedNext::HelloRetryRequestOrServerHello)
        );
        // ServerHello 位置に Finished（msg_type 不一致）を投入して poison させる。
        let bogus = RawHandshake {
            msg_type: HandshakeType::Finished,
            body: vec![0u8; 32],
        };
        assert_eq!(
            t.append_server_hello(&bogus).unwrap_err(),
            TranscriptError::UnexpectedMessage
        );
        assert_eq!(t.expected_next(), Err(TranscriptError::OutOfOrder));
        // 読み取り専用の問い合わせを繰り返しても同じエラーを返し続ける。
        assert_eq!(t.expected_next(), Err(TranscriptError::OutOfOrder));
    }

    // PR #1033 review（P1）: チェックポイントの時点違いによる poison 後も
    // `expected_next()` が `Err(OutOfOrder)` を返すことを固定する。
    #[test]
    fn expected_next_is_rejected_after_checkpoint_failure_poisons() {
        let mut t = Transcript::new();
        assert_eq!(t.expected_next(), Ok(ExpectedNext::ClientHello));
        assert_eq!(
            t.hash_through_server_hello().unwrap_err(),
            TranscriptError::OutOfOrder
        );
        assert_eq!(t.expected_next(), Err(TranscriptError::OutOfOrder));
    }

    // PR #1033 review（P2）: `absorb` はヘッダ込みの一時バッファを作らず
    // ヘッダ・本文を別々に SHA-256 へ投入する。メモリ使用量そのものは
    // アロケータ計測なしには実行時に検証できないため、分割投入が従来の
    // 全量バッファ経由（`RawHandshake::to_bytes` の連結）とバイト等価で
    // あることを固定する。
    #[test]
    fn split_header_body_absorb_matches_whole_message_hash() {
        let ch = raw(HandshakeType::ClientHello, CLIENT_HELLO_1RTT);
        let sh = raw(HandshakeType::ServerHello, SERVER_HELLO_1RTT);
        let mut t = Transcript::new();
        t.append_client_hello(&ch).expect("valid ClientHello");
        t.append_server_hello(&sh).expect("valid ServerHello");
        let got = t.hash_through_server_hello().expect("checkpoint reached");

        let mut whole = Sha256::new();
        whole.update(&ch.to_bytes().expect("encodable ClientHello"));
        whole.update(&sh.to_bytes().expect("encodable ServerHello"));
        assert_eq!(got, whole.finalize());
    }

    // PR #1033 review（P2）: ヘッダ構成を `RawHandshake::header` へ切り出した
    // 後も、24 ビット長に収まらない本文は `Malformed` で拒否し poison する
    // （全量コピーを経由しなくなっても長さ検証が失われないことを固定する）。
    #[test]
    fn absorb_rejects_body_exceeding_u24_and_poisons() {
        let mut t = Transcript::new();
        let oversized = RawHandshake {
            msg_type: HandshakeType::ClientHello,
            body: vec![0u8; MAX_HANDSHAKE_WIRE_BODY_LEN as usize + 1],
        };
        assert_eq!(
            t.append_client_hello(&oversized).unwrap_err(),
            TranscriptError::Malformed
        );
        assert_eq!(t.expected_next(), Err(TranscriptError::OutOfOrder));
    }

    #[test]
    fn unexpected_msg_type_is_rejected() {
        let mut t = Transcript::new();
        // ClientHello の位置に Finished（msg_type 不一致）を投入。
        let bogus = RawHandshake {
            msg_type: HandshakeType::Finished,
            body: vec![0u8; 32],
        };
        let err = t.append_client_hello(&bogus).unwrap_err();
        assert_eq!(err, TranscriptError::UnexpectedMessage);
    }

    #[test]
    fn checkpoint_at_wrong_step_is_rejected_and_poisons() {
        let mut t = Transcript::new();
        // まだ ServerHello に到達していない時点でのチェックポイント取得は
        // 拒否され、モジュール doc の poison 契約（一度でも `Err` を返したら
        // 以後すべて `OutOfOrder`）により以後の呼び出しもすべて拒否される
        // （時点違いのチェックポイント取得を経ても正しい append を続行
        // できてしまわないことを固定する）。
        assert_eq!(
            t.hash_through_server_hello().unwrap_err(),
            TranscriptError::OutOfOrder
        );

        assert_eq!(
            t.append_client_hello(&raw(HandshakeType::ClientHello, CLIENT_HELLO_1RTT))
                .unwrap_err(),
            TranscriptError::OutOfOrder
        );
    }

    #[test]
    fn checkpoint_at_wrong_step_after_valid_progress_is_rejected_and_poisons() {
        let mut t = Transcript::new();
        t.append_client_hello(&raw(HandshakeType::ClientHello, CLIENT_HELLO_1RTT))
            .expect("valid ClientHello");
        // ServerHello 未投入のまま Certificate 側のチェックポイントを取得。
        assert_eq!(
            t.hash_through_certificate().unwrap_err(),
            TranscriptError::OutOfOrder
        );
        // poison 後は正しい ServerHello の append も拒否される。
        assert_eq!(
            t.append_server_hello(&raw(HandshakeType::ServerHello, SERVER_HELLO_1RTT))
                .unwrap_err(),
            TranscriptError::OutOfOrder
        );
    }

    #[test]
    fn hello_retry_request_twice_is_rejected() {
        // 本テストは append_hello_retry_request の「1 接続 1 回のみ」受理条件
        // のみを検証する（HRR 本文の厳密なバイト列は不問。RFC 8448 §5 の
        // 実バイト列を使った transcript 再構成の正しさは後続の統合テストが
        // 別途固定する）。
        let mut t = Transcript::new();
        t.append_client_hello(&raw(HandshakeType::ClientHello, CLIENT_HELLO_1RTT))
            .expect("valid ClientHello");

        // random = HELLO_RETRY_REQUEST_RANDOM を持つ ServerHello 形メッセージ
        // （HRR 相当。ボディの厳密な中身はここでは不問）。
        let mut hrr_body = vec![0x03, 0x03];
        hrr_body.extend_from_slice(&HELLO_RETRY_REQUEST_RANDOM);
        hrr_body.extend_from_slice(&[0x00; 4]);
        let hrr = RawHandshake {
            msg_type: HandshakeType::ServerHello,
            body: hrr_body,
        };
        t.append_hello_retry_request(&hrr).expect("valid HRR");

        // 2 回目の HRR は拒否される（1 接続 1 回のみ）。
        let ch2 = raw(HandshakeType::ClientHello, CLIENT_HELLO_1RTT);
        t.append_client_hello(&ch2).expect("2nd ClientHello");
        let hrr2 = RawHandshake {
            msg_type: HandshakeType::ServerHello,
            body: hrr.body.clone(),
        };
        assert_eq!(
            t.append_hello_retry_request(&hrr2).unwrap_err(),
            TranscriptError::OutOfOrder
        );
    }

    #[test]
    fn server_hello_with_hrr_random_is_not_mistaken_but_hrr_without_matching_random_is_rejected() {
        let mut t = Transcript::new();
        t.append_client_hello(&raw(HandshakeType::ClientHello, CLIENT_HELLO_1RTT))
            .expect("valid ClientHello");

        // random が HELLO_RETRY_REQUEST_RANDOM と異なる ServerHello 形メッセージを
        // append_hello_retry_request へ渡すと拒否される（取り違え防止）。
        let err = t
            .append_hello_retry_request(&raw(HandshakeType::ServerHello, SERVER_HELLO_1RTT))
            .unwrap_err();
        assert_eq!(err, TranscriptError::UnexpectedMessage);
    }

    #[test]
    fn append_server_hello_rejects_hello_retry_request_shaped_message() {
        // HRR は ServerHello と同じ `msg_type` を使うため（RFC 8446 §4.1.4）、
        // `random` が HRR 定数と一致するメッセージを `append_server_hello` が
        // msg_type だけで通常の ServerHello として受理してしまうと、
        // message_hash 置換（`append_hello_retry_request`）が漏れ誤った
        // transcript hash になる。呼び出し元の分類誤りをここで検出できる
        // ことを固定する。
        let mut t = Transcript::new();
        t.append_client_hello(&raw(HandshakeType::ClientHello, CLIENT_HELLO_1RTT))
            .expect("valid ClientHello");

        let mut hrr_body = vec![0x03, 0x03];
        hrr_body.extend_from_slice(&HELLO_RETRY_REQUEST_RANDOM);
        hrr_body.extend_from_slice(&[0x00; 4]);
        let hrr_shaped = RawHandshake {
            msg_type: HandshakeType::ServerHello,
            body: hrr_body,
        };
        assert_eq!(
            t.append_server_hello(&hrr_shaped).unwrap_err(),
            TranscriptError::UnexpectedMessage
        );
        // poison 済みのため、正しい呼び分け（append_hello_retry_request）を
        // 後から試みても拒否される（OutOfOrder）。
        assert_eq!(
            t.append_hello_retry_request(&hrr_shaped).unwrap_err(),
            TranscriptError::OutOfOrder
        );
    }

    #[test]
    fn complete_handshake_rejects_further_append() {
        // ClientHello→ServerHello→EncryptedExtensions→Certificate→
        // CertificateVerify→server Finished→client Finished まで完走させ、
        // 完了後の append がすべて拒否されることを固定する。
        let mut t = Transcript::new();
        t.append_client_hello(&raw(HandshakeType::ClientHello, CLIENT_HELLO_1RTT))
            .unwrap();
        t.append_server_hello(&raw(HandshakeType::ServerHello, SERVER_HELLO_1RTT))
            .unwrap();
        let ee = RawHandshake {
            msg_type: HandshakeType::EncryptedExtensions,
            body: vec![0x00, 0x00],
        };
        t.append_encrypted_extensions(&ee).unwrap();
        let cert = RawHandshake {
            msg_type: HandshakeType::Certificate,
            body: vec![0x00, 0x00, 0x00, 0x00],
        };
        t.append_certificate(&cert).unwrap();
        assert!(t.hash_through_certificate().is_ok());
        let cv = RawHandshake {
            msg_type: HandshakeType::CertificateVerify,
            body: vec![0x08, 0x04, 0x00, 0x00],
        };
        t.append_certificate_verify(&cv).unwrap();
        assert!(t.hash_through_certificate_verify().is_ok());
        let sf = RawHandshake {
            msg_type: HandshakeType::Finished,
            body: vec![0u8; 32],
        };
        t.append_server_finished(&sf).unwrap();
        assert!(t.hash_through_server_finished().is_ok());
        let cf = RawHandshake {
            msg_type: HandshakeType::Finished,
            body: vec![1u8; 32],
        };
        t.append_client_finished(&cf).unwrap();
        assert_eq!(t.expected_next(), Ok(ExpectedNext::Complete));

        // 完了後の append はすべて拒否される。
        assert_eq!(
            t.append_client_hello(&raw(HandshakeType::ClientHello, CLIENT_HELLO_1RTT))
                .unwrap_err(),
            TranscriptError::OutOfOrder
        );
    }

    // `Debug` に内部ハッシュ状態（生の SHA-256 中間バイト列）が含まれない
    // ことを確認する（本体は秘密ではないが、出力する理由がないため最小化）。
    #[test]
    fn debug_does_not_expose_hasher_internals() {
        let t = Transcript::new();
        let debug = format!("{t:?}");
        assert!(debug.contains("Transcript"));
        assert!(!debug.contains("state"));
    }
}
