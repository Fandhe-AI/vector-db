# vector-db の開発コンテナ。
#
# 環境非依存の開発・検証用（make docker-shell / docker-ci から利用。compose.yaml 参照）。
# `make ci` の lint-docs 系（markdownlint / editorconfig-checker / commitlint は npx、
# yamllint は Debian パッケージ）と cargo 系（Cargo.toml 追加後に有効化）の両方が
# クリーンなコンテナで即実行できる構成にする。
# Fandhe-AI/rust-ai-library の Dockerfile と同一方針（実機 GPU 前提が無い分を簡素化）。
#
# ベースはメジャー版固定の rust:1-slim-bookworm とする。ツールチェーンの正は
# リポジトリルートの rust-toolchain.toml（channel = "stable"）であり、/work で
# マウントされたワークスペースでの cargo / rustup 実行はイメージ既定版ではなく
# stable を解決する。未導入のまま非 root の dev ユーザーで cargo を実行すると
# stable の自動インストールが root 所有の RUSTUP_HOME への書き込みで権限エラーに
# なるため、イメージビルド時に components ごと導入しておく（rust-ai-library の教訓）。
FROM rust:1-slim-bookworm

# ビルド・検証に必要な最小ツールのみ導入する（レイヤ削減のため 1 RUN に集約）。
# nodejs / npm: make ci の lint-md / lint-editorconfig / lint-commits（npx 実行）に必要。
# yamllint: make ci の lint-yaml に必要（Python 製のため npx で賄えない。Debian
# パッケージで導入し、追加 PyPI 依存を持ち込まない）。
RUN apt-get update && apt-get install -y --no-install-recommends \
        build-essential \
        pkg-config \
        git \
        make \
        curl \
        ca-certificates \
        nodejs \
        npm \
        yamllint \
    && rm -rf /var/lib/apt/lists/* \
    && rustup component add rustfmt clippy \
    && rustup toolchain install stable \
        --component rustfmt \
        --component clippy

# ホストと UID を合わせやすい非 root ユーザーで作業する（成果物の所有権事故防止）
ARG UID=1000
RUN useradd -m -u "${UID}" dev

# named volume（compose.yaml の cargo-registry / target-cache）は初回マウント時に
# イメージ内の該当パスの内容・所有者を引き継ぐため、dev 所有で事前作成しておく。
# これを行わないとマウントポイントが root 所有になり、非 root の dev ユーザーが
# crate キャッシュ・target へ書き込めず cargo が失敗する。
# /usr/local/cargo は rust イメージの CARGO_HOME（registry 以外の git/ 等にも書き込みが
# 発生するため CARGO_HOME 全体を chown する）。
RUN mkdir -p /usr/local/cargo/registry /work/target \
    && chown -R dev:dev /usr/local/cargo /work

USER dev
WORKDIR /work

# make deny（deny.toml。Cargo.toml 追加後に有効化）がクリーンなコンテナでも即実行
# できるよう、cargo-deny をイメージビルド時に導入しておく（Makefile の deny ターゲット
# 自体も未導入なら自動導入する自己修復を持つが、初回 docker-ci でのネットワーク依存を
# 避けるためここで先行導入する。dev ユーザーの CARGO_HOME 配下にインストールされる。
# バージョンは Makefile の CARGO_DENY_VERSION と同期させる）。
RUN cargo install cargo-deny@0.20.2 --locked

CMD ["bash"]
