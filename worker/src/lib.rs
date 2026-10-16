//! gps2city の Cloudflare Workers 版。
//! 全国のタイルを 1 本のパック (data/tiles.pack) にして Worker に埋め込み、必要なタイルだけを展開して
//! isolate 内にキャッシュする。外部からの読み込みが無いので、初回アクセスでも待ち時間が出ない。

mod lookup;

use lookup::{Nearby, Tile};
use serde::Deserialize;
use serde_json::json;
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use worker::*;

const DEFAULT_RADIUS_KM: f64 = 10.0;
const MAX_RADIUS_KM: f64 = 50.0;
const CACHE_LIMIT_BYTES: usize = 32 << 20; // isolate のメモリ上限は 128MB (うちパックが約 23MB)
// 境界線は build_tiles で 1m まで間引いてある。隣のエリアとの間にできた隙間に落ちた点は、この距離までなら最寄りのエリアにする
const SNAP_M: f64 = 2.0;

static PACK: &[u8] = include_bytes!("../data/tiles.pack");

#[derive(Deserialize, Debug)]
struct Location {
    latitude: f64,
    longitude: f64,
    radius_km: Option<f64>,
    sort_by: Option<String>, // "distance" (既定) または "prefecture"
}

#[derive(Default)]
struct TileCache {
    tiles: HashMap<(i32, i32), Option<Rc<Tile>>>, // None = 海などでタイルが無い
    bytes: usize,
}

thread_local! {
    static CACHE: RefCell<TileCache> = RefCell::new(TileCache::default());
}

#[event(fetch)]
async fn fetch(req: Request, env: Env, _ctx: Context) -> Result<Response> {
    let result = match (req.method(), req.path().as_str()) {
        (Method::Options, _) => Response::empty().map(|r| r.with_status(204)),
        (Method::Post, "/location") => handle_location(req).await,
        (Method::Post, "/location2") => handle_nearby(req).await,
        _ => return env.assets("ASSETS")?.fetch_request(req).await,
    };
    let resp = result.or_else(|e| Response::from_json(&json!({ "error": e.to_string() })).map(|r| r.with_status(500)))?;
    with_cors(resp)
}

async fn handle_location(mut req: Request) -> Result<Response> {
    let loc = match parse(&mut req).await {
        Ok(loc) => loc,
        Err(resp) => return resp,
    };
    let key = lookup::tile_index(loc.longitude, loc.latitude);
    let (tiles, stats) = load_tiles(&[key])?;
    let found = tiles.first().and_then(|t| {
        t.locate(loc.longitude, loc.latitude).or_else(|| t.nearest(loc.longitude, loc.latitude, SNAP_M))
    });
    let resp = match found {
        Some(area) => Response::from_json(&json!({
            "prefecture": area.prefecture,
            "city": area.city,
            "ward": area.ward,
            "code": area.code,
            "lg_code": lookup::lg_code(&area.code),
        })),
        None => Response::from_json(&json!({ "error": "Location not found" })).map(|r| r.with_status(404)),
    };
    stats.annotate(resp?)
}

async fn handle_nearby(mut req: Request) -> Result<Response> {
    let loc = match parse(&mut req).await {
        Ok(loc) => loc,
        Err(resp) => return resp,
    };
    let radius_km = loc.radius_km.unwrap_or(DEFAULT_RADIUS_KM);
    if !(radius_km > 0.0 && radius_km <= MAX_RADIUS_KM) {
        return bad_request(&format!("radius_km must be greater than 0 and at most {MAX_RADIUS_KM}"));
    }
    let radius_m = radius_km * 1000.0;
    let keys = lookup::tiles_in_radius(loc.longitude, loc.latitude, radius_m);
    let (tiles, stats) = load_tiles(&keys)?;
    let refs: Vec<&Tile> = tiles.iter().map(Rc::as_ref).collect();
    let mut found = lookup::nearby(&refs, loc.longitude, loc.latitude, radius_m);

    let sorted = if loc.sort_by.as_deref() == Some("prefecture") {
        // 県ごとの最短距離で県を並べ、県内は距離順
        let mut nearest: HashMap<String, f64> = HashMap::new();
        for n in &found {
            let e = nearest.entry(n.prefecture.clone()).or_insert(f64::INFINITY);
            *e = e.min(n.distance_m);
        }
        found.sort_by(|a, b| {
            nearest[&a.prefecture]
                .total_cmp(&nearest[&b.prefecture])
                .then_with(|| a.prefecture.cmp(&b.prefecture))
                .then_with(|| a.distance_m.total_cmp(&b.distance_m))
        });
        found
    } else {
        found.sort_by(|a, b| a.distance_m.total_cmp(&b.distance_m));
        found
    };
    let body: Vec<_> = sorted
        .iter()
        .map(|Nearby { prefecture, city, distance_m, areas }| {
            // 区に分かれない市区町村は code / lg_code を直接持つ。政令市は areas (区ごと) を見る
            let single = (areas.len() == 1 && areas[0].ward.is_empty()).then(|| &areas[0].code);
            json!({
                "prefecture": prefecture,
                "city": city,
                "distance": distance_m,
                "code": single,
                "lg_code": single.and_then(|c| lookup::lg_code(c)),
                "areas": areas.iter().map(|a| json!({
                    "ward": a.ward,
                    "code": a.code,
                    "lg_code": lookup::lg_code(&a.code),
                    "distance": a.distance_m,
                })).collect::<Vec<_>>(),
            })
        })
        .collect();
    stats.annotate(Response::from_json(&body)?)
}

async fn parse(req: &mut Request) -> std::result::Result<Location, Result<Response>> {
    let loc: Location = req.json().await.map_err(|e| bad_request(&format!("invalid JSON body: {e}")))?;
    let valid = loc.latitude.is_finite()
        && loc.longitude.is_finite()
        && (-90.0..=90.0).contains(&loc.latitude)
        && (-180.0..=180.0).contains(&loc.longitude);
    if !valid {
        return Err(bad_request("latitude/longitude out of range"));
    }
    Ok(loc)
}

fn bad_request(message: &str) -> Result<Response> {
    Response::from_json(&json!({ "error": message })).map(|r| r.with_status(400))
}

/// タイル読み込みの内訳。Server-Timing ヘッダーで返して本番の計測に使う。
struct LoadStats {
    hit: usize,
    miss: usize,
}

impl LoadStats {
    fn annotate(&self, mut resp: Response) -> Result<Response> {
        let h = resp.headers_mut();
        h.set("Server-Timing", &format!("tiles;desc=\"hit={} miss={}\"", self.hit, self.miss))?;
        h.set("Timing-Allow-Origin", "*")?;
        Ok(resp)
    }
}

/// タイルを取り出す。展開済みのものは isolate 内のキャッシュから、それ以外はパックから展開する。
fn load_tiles(keys: &[(i32, i32)]) -> Result<(Vec<Rc<Tile>>, LoadStats)> {
    CACHE.with(|c| {
        let mut c = c.borrow_mut();
        let mut tiles = Vec::with_capacity(keys.len());
        let mut miss = 0;
        for &key in keys {
            if let Some(t) = c.tiles.get(&key) {
                tiles.extend(t.clone());
                continue;
            }
            miss += 1;
            let tile = match lookup::find_tile(PACK, key).map_err(Error::RustError)? {
                Some(blob) => Some(Rc::new(Tile::decode(key, blob).map_err(Error::RustError)?)),
                None => None,
            };
            let size = tile.as_ref().map_or(16, |t| t.approx_bytes());
            if c.bytes + size > CACHE_LIMIT_BYTES {
                c.tiles.clear(); // 返す分は tiles が保持しているので消えない
                c.bytes = 0;
            }
            c.bytes += size;
            tiles.extend(tile.clone());
            c.tiles.insert(key, tile);
        }
        Ok((tiles, LoadStats { hit: keys.len() - miss, miss }))
    })
}

fn with_cors(mut resp: Response) -> Result<Response> {
    let h = resp.headers_mut();
    h.set("Access-Control-Allow-Origin", "*")?;
    h.set("Access-Control-Allow-Methods", "GET, POST, OPTIONS")?;
    h.set("Access-Control-Allow-Headers", "*")?;
    h.set("Access-Control-Max-Age", "3600")?;
    Ok(resp)
}
