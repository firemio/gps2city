# gps2city — 緯度経度から日本の市区町村を返す Cloudflare Worker

緯度経度を送ると、その地点の **都道府県・市区町村 (政令市は区まで)・全国地方公共団体コード** を返します。
周辺の市区町村と、その境界までの距離も返します。

- 稼働中: https://gps2city.firemio.workers.dev/ (ブラウザで開くと現在地で試せます)
- 作者: **firemio** ([GitHub @firemio](https://github.com/firemio))
- 利用例: 同じ作者の [gomisute-gps](https://github.com/firemio/gomisute-gps) (位置情報からその日に出せるごみを答える。Built with Claude: Japan 2026 応募作品) の入口として使っています

## 特徴

- **Rust を WebAssembly にして Cloudflare Workers で動かす**。サーバー (VPS) は要りません
- **全国の行政区域を1本のパック (約23MB) にして Worker 本体に埋め込む**。外部ストレージを読まないので、初めて問い合わせる地点でも速い
  (東京 (NRT) のデータセンターで、まだ問い合わせの無い地点の TTFB 中央値: タイルを Static Assets から読んでいた旧方式 250〜310ms → 11ms)
- データは国土数値情報 行政区域 **N03-2026 (2026年1月1日時点)**。能登半島地震の隆起でできた陸地も入っています ([データの更新履歴](#データの更新履歴))
- 境界線は誤差 1m 以内で間引き。間引き前の元データで計算した距離との差は最大 0.97m (N03-2024 版の 304 地点で確認。許容誤差は同じ 1m)
- 返すコード `lg_code` はデジタル庁 アドレス・ベース・レジストリ (ABR) の市区町村コードと同じ6桁。
  データに含まれる 1,905 エリア (市区町村。政令市は区) のうち 1,898 は ABR の市区町村マスターとコード・名称が一致します。
  残り 7 は所属未定地で、`city` は「所属未定地」、`code` は都道府県コード (例: 沖縄県 `47000` / `lg_code` `470007`) を返します

## API

CORS は全オリジン許可です。下の例は読みやすさのためキーを並べ替え、`distance` を丸めています
(実際の応答はキーがアルファベット順で、`distance` は丸めずに返ります)。

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

- どの市区町村にも入らない地点 (海上など) は `404 {"error":"Location not found"}`
- ただし境界線から 2m 以内の点 (間引きでできた隣接エリア間の隙間や、海岸線のすぐ外) は最寄りのエリアを返します
- JSON が不正、緯度経度が範囲外のときは `400 {"error":"…"}`

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

- `radius_km`: 0 より大きく 50 以下 (既定 10)。範囲外は `400`
- `sort_by`: `"distance"` (既定。距離順) または `"prefecture"` (最も近い市区町村がある都道府県から順に並べ、都道府県内は距離順)。それ以外の値は `"distance"` として扱います
- `distance`: 境界線までの距離 (m)。地点を含む市区町村は 0
- 政令市は市ごとに1行にまとめ、区ごとの内訳を `areas` に入れます (このとき行の `code` / `lg_code` は null)
- 半径内に陸地が無いときは `200 []`

レスポンスヘッダー `Server-Timing: tiles;desc="hit=… miss=…"` で、タイルのキャッシュの当たり外れが分かります。

## 仕組み

1. `tools/build_tiles` (Rust) が国土数値情報 N03 の GeoJSON を 0.1° 四方のタイル (5,062 枚) に分割する
   - 境界線を Douglas–Peucker 法で 1m まで間引き、タイル境界で切り取る (Sutherland–Hodgman)
   - 座標を 1e-6° (約 0.1m) に量子化し、差分を zigzag + LEB128 の可変長整数で詰めて、1本のパック (形式 `GTP1`) に書き出す
2. `worker` (Rust → Wasm) はパックを `include_bytes!` で埋め込み、リクエストに必要なタイルだけを展開して isolate 内にキャッシュする (上限 32MB)
3. 点の所属は偶奇判定 (レイキャスティング)。境界までの距離は、境界線上の最寄り点 (タイルの切れ目でできた見かけの辺は除く) までの大圏距離 (haversine)

```
worker/                 Cloudflare Worker 本体 (src/lib.rs, src/lookup.rs, public/index.html)
tools/build_tiles/      GeoJSON → タイルのパック (worker/data/tiles.pack) を作るツール
```

## 動かし方

前提: Rust (stable) と Node.js。初回の `npm run dev` / `npm run deploy` では、wrangler.toml の build コマンドが
worker-build を `cargo install` します (数分かかり、ネットワークが必要です)。

```bash
rustup target add wasm32-unknown-unknown
(cd worker && npm install)
```

**データ**: [国土数値情報 行政区域データ](https://nlftp.mlit.go.jp/ksj/gml/datalist/KsjTmplt-N03-2026.html) から
全国版 `N03-20260101_GML.zip` (約800MB) を取得し、中の `N03-20260101.geojson` (約580MB。`_prefecture` の付かない方) を
`tools/build_tiles/japan.geojson` として置きます (git 管理外)。

```bash
cd worker
npm run tiles     # tools/build_tiles → worker/data/tiles.pack (数秒。ビルド前に必要)
npm run dev       # http://127.0.0.1:8787
npx wrangler login
npm run deploy
```

同じ入力からは毎回バイト単位で同じパックができます。N03-20260101.geojson から作ったパックの SHA-256 は
`708d497fe56281f2a0789f566db2f70c0c502d57d504d439031c1d21ca727d07` です (2026-10-17 に生成・デプロイ)。

## データの出典

「国土数値情報（行政区域データ）」（国土交通省）（https://nlftp.mlit.go.jp/ksj/gml/datalist/KsjTmplt-N03-2026.html）を加工して作成。
N03-2026 (2026年1月1日時点) の境界線を間引き・タイル分割して使用しています (CC BY 4.0)。

## データの更新履歴

### 2026-10-17: N03-2024 → N03-2026

行政区域データは毎年1月1日時点で更新されます。2024年版から2026年版に入れ替えたときの差分です。
比較には本番と同じ判定コード (`worker/src/lookup.rs`) を使い、全国を約250m 間隔の 619万点で引き比べました。

- **市区町村の新設・廃止・改称は無し**。1,905 エリアのコードと名称は同じで、ABR とも引き続き 1,898 件が一致 (残り7は所属未定地)
- **所属が変わった点は 458 点 (0.0074%)**。境界や海岸線の変化です
  - **能登半島地震 (2024年1月) の隆起で、海だった所が陸になった**: 輪島市 +2.78km²、珠洲市 +1.72km²、志賀町 +0.23km²
    (パックから計算した面積の増加。国土地理院の公表値 [輪島市 2.78・珠洲市 1.72・志賀町 0.24km²](https://www.gsi.go.jp/kihonjohochousa/kihonjohochousa61016.html) とほぼ一致)
  - 東京都の所属未定地のうち約 1.2km² が江東区に
  - 新潟県 十日町市 → 湯沢町 (約 2.1km²)、東京都 中央区 → 港区 (約 0.2km²) などの境界の変更

## ライセンス

- ソースコードと文書: [MIT License](LICENSE) (Copyright (c) 2024-2026 firemio)
- 生成したタイルのパック: 「国土数値情報」の利用条件 (CC BY 4.0) に従います (上の出典の表示が必要です)

## 履歴

最初の版は actix-web の常駐サーバー (Docker + Traefik) でした。Cloudflare Workers 版への移行に伴い削除しています。
旧版のコードは git の履歴 (コミット [`44bfaee`](https://github.com/firemio/gps2city/tree/44bfaee) まで) で参照できます。
