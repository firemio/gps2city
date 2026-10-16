# gps2city — 緯度経度から日本の市区町村を返す Cloudflare Worker

緯度経度を送ると、その地点の **都道府県・市区町村 (政令市は区まで)・全国地方公共団体コード** を返します。
周辺の市区町村と、その境界までの距離も返します。

- 稼働中: https://gps2city.firemio.workers.dev/ (ブラウザで開くと現在地で試せます)
- 利用例: [gomisute-gps](https://github.com/firemio/gomisute-gps) (位置情報からごみ収集日を答える) の入口として使っています

## 特徴

- **Rust を WebAssembly にして Cloudflare Workers で動かす**。サーバー (VPS) は要りません
- **全国の行政区域を1本のパック (約23MB) にして Worker 本体に埋め込む**。外部ストレージを読まないので、
  初めて問い合わせる地点でも速い (本番 NRT での未問い合わせ地点の TTFB 中央値: 外部読み込み時 250〜310ms → 11ms)
- 境界線は 1m の誤差で間引き。距離の誤差は厳密計算に対して最大 0.97m (304 地点で確認)
- 返すコード `lg_code` はデジタル庁 アドレス・ベース・レジストリ (ABR) の市区町村コードと同じ6桁。
  タイル内の 1,905 エリア中 1,898 が ABR の市区町村マスターと名称まで一致 (残り7は所属未定地)

## API

CORS は全オリジン許可です。

### `POST /location` — 地点の市区町村

```bash
curl -X POST https://gps2city.firemio.workers.dev/location \
  -H 'Content-Type: application/json' -d '{"latitude":35.4437,"longitude":139.6380}'
```
```json
{"prefecture":"神奈川県","city":"横浜市","ward":"中区","code":"14104","lg_code":"141046"}
```

| フィールド | 内容 |
|---|---|
| `prefecture`, `city` | 都道府県名、市区町村名 (政令市は市名) |
| `ward` | 政令市の区名。それ以外は空文字 |
| `code` | 5桁の行政区域コード (国土数値情報 N03_007。政令市は区のコード) |
| `lg_code` | `code` に検査数字を付けた6桁の全国地方公共団体コード (ABR の `lg_code`) |

海上など、どの市区町村にも入らない地点は `404 {"error":"Location not found"}`。
間引きでできた隣接エリア間のわずかな隙間 (2m 以内) に落ちた点は、最寄りのエリアを返します。

### `POST /location2` — 周辺の市区町村と境界までの距離

```bash
curl -X POST https://gps2city.firemio.workers.dev/location2 \
  -H 'Content-Type: application/json' -d '{"latitude":36.5613,"longitude":136.6485,"radius_km":5}'
```
```json
[{"prefecture":"石川県","city":"金沢市","distance":0.0,"code":"17201","lg_code":"172014",
  "areas":[{"ward":"","code":"17201","lg_code":"172014","distance":0.0}]},
 {"prefecture":"石川県","city":"野々市市","distance":2912.07,"code":"17212","lg_code":"172120",
  "areas":[{"ward":"","code":"17212","lg_code":"172120","distance":2912.07}]}]
```

- `radius_km`: 0 より大きく 50 以下 (既定 10)。`sort_by`: `"distance"` (既定) または `"prefecture"`
- `distance`: 境界線までの距離 (m)。地点を含む市区町村は 0
- 政令市は市ごとに1行にまとめ、区ごとの内訳を `areas` に入れます (このとき行の `code` / `lg_code` は null)

レスポンスヘッダー `Server-Timing: tiles;desc="hit=… miss=…"` で、タイルのキャッシュの当たり外れが分かります。

## 仕組み

1. `tools/build_tiles` (Rust) が国土数値情報 N03 の GeoJSON を 0.1° 四方のタイル (5,062 枚) に分割する
   - 境界線を Douglas–Peucker 法で 1m まで間引き、タイル境界で切り取る (Sutherland–Hodgman)
   - 座標を 1e-6° (約 0.1m) に量子化し、差分を zigzag + LEB128 の可変長整数で詰めて、1本のパック (形式 `GTP1`) に書き出す
2. `worker` (Rust → Wasm) はパックを `include_bytes!` で埋め込み、リクエストに必要なタイルだけを展開して isolate 内にキャッシュする (上限 32MB)
3. 点の所属は偶奇判定、境界までの距離はタイル切り取りで生じた辺を除いた最寄り点までの haversine 距離

```
worker/                 Cloudflare Worker 本体 (src/lib.rs, src/lookup.rs, public/index.html)
tools/build_tiles/      GeoJSON → タイルのパック (worker/data/tiles.pack) を作るツール
```

## 動かし方

前提: Rust (stable) と Node.js。

```bash
rustup target add wasm32-unknown-unknown
cd worker && npm install
```

**データ**: [国土数値情報 行政区域データ](https://nlftp.mlit.go.jp/ksj/gml/datalist/KsjTmplt-N03-2024.html) から
全国版 `N03-20240101_GML.zip` を取得し、中の `N03-20240101.geojson` を `tools/build_tiles/japan.geojson` として置きます (約450MB、git 管理外)。

```bash
cd worker
npm run tiles     # tools/build_tiles → worker/data/tiles.pack (数秒。ビルド前に必要)
npm run dev       # http://127.0.0.1:8787
npx wrangler login
npm run deploy
```

同じ入力からは同じパックができます (2026-10-17 に SHA-256 の一致を確認)。

## データの出典

「国土数値情報 (行政区域データ)」(国土交通省) N03-2024 (2024-01-01 時点) を加工して作成。

## 履歴

最初の版は actix-web の常駐サーバー (Docker + Traefik) でした。Cloudflare Workers 版への移行に伴い削除しています。
旧版のコードは git の履歴 (コミット `44bfaee` まで) で参照できます。
