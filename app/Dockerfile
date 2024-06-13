# app/Dockerfile
# Rustの公式イメージを使用
FROM rust:alpine

# 作業ディレクトリを設定
WORKDIR /usr/src/app

# ソースコードとCargo.tomlをコピー
COPY . .

# cargo-watchをインストール
RUN cargo install cargo-watch

# コンテナ起動時にアプリケーションを実行
CMD ["cargo", "watch", "-x", "run"]