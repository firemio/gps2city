//! gps2city の Cloudflare Workers 版。
//! タイル (public/data/tiles/*.bin) を ASSETS バインディングから必要な分だけ読み、isolate 内にキャッシュする。

mod lookup;

use futures::future::join_all;
use lookup::{Nearby, Tile};
use serde::Deserialize;
use serde_json::json;
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use worker::*;

const DEFAULT_RADIUS_KM: f64 = 10.0;
const MAX_RADIUS_KM: f64 = 50.0;
const CACHE_LIMIT_BYTES: usize = 48 << 20; // isolate のメモリ上限は 128MB

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
        (Method::Post, "/location") => handle_location(req, &env).await,
        (Method::Post, "/location2") => handle_nearby(req, &env).await,
        // タイルは Worker 経由でのみ読む (wrangler.toml の run_worker_first で直接配信を止めている)
        (_, path) if path.starts_with("/data/") => Response::error("Not Found", 404),
        _ => return env.assets("ASSETS")?.fetch_request(req).await,
    };
    let resp = result.or_else(|e| Response::from_json(&json!({ "error": e.to_string() })).map(|r| r.with_status(500)))?;
    with_cors(resp)
}

async fn handle_location(mut req: Request, env: &Env) -> Result<Response> {
    let loc = match parse(&mut req).await {
        Ok(loc) => loc,
        Err(resp) => return resp,
    };
    let key = lookup::tile_index(loc.longitude, loc.latitude);
    let tiles = load_tiles(env, &[key]).await?;
    match tiles.first().and_then(|t| t.locate(loc.longitude, loc.latitude)) {
        Some(area) => Response::from_json(&json!({
            "prefecture": area.prefecture,
            "city": area.city,
            "ward": area.ward,
            "code": area.code,
            "lg_code": lookup::lg_code(&area.code),
        })),
        None => Response::from_json(&json!({ "error": "Location not found" })).map(|r| r.with_status(404)),
    }
}

async fn handle_nearby(mut req: Request, env: &Env) -> Result<Response> {
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
    let tiles = load_tiles(env, &keys).await?;
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
    Response::from_json(&body)
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

/// タイルを読み込む。キャッシュに無いものだけ ASSETS から並行して取得する。
async fn load_tiles(env: &Env, keys: &[(i32, i32)]) -> Result<Vec<Rc<Tile>>> {
    let (mut tiles, missing) = CACHE.with(|c| {
        let c = c.borrow();
        let mut hits = Vec::new();
        let mut missing = Vec::new();
        for k in keys {
            match c.tiles.get(k) {
                Some(t) => hits.extend(t.clone()),
                None => missing.push(*k),
            }
        }
        (hits, missing)
    });
    if !missing.is_empty() {
        let assets = env.assets("ASSETS")?;
        let fetched = join_all(missing.iter().map(|&key| {
            let assets = &assets;
            async move {
                let mut resp = assets.fetch(format!("https://assets.local{}", lookup::tile_path(key)), None).await?;
                if resp.status_code() == 404 {
                    return Ok::<_, Error>((key, None));
                }
                if resp.status_code() != 200 {
                    return Err(Error::RustError(format!("tile {key:?}: HTTP {}", resp.status_code())));
                }
                let tile = Tile::decode(key, &resp.bytes().await?).map_err(Error::RustError)?;
                Ok((key, Some(Rc::new(tile))))
            }
        }))
        .await;
        let fetched = fetched.into_iter().collect::<Result<Vec<_>>>()?;
        tiles.extend(fetched.iter().filter_map(|(_, t)| t.clone()));
        CACHE.with(|c| {
            let mut c = c.borrow_mut();
            let incoming: usize = fetched.iter().map(|(_, t)| t.as_ref().map_or(16, |t| t.approx_bytes())).sum();
            if c.bytes + incoming > CACHE_LIMIT_BYTES {
                c.tiles.clear();
                c.bytes = 0;
            }
            c.bytes += incoming;
            c.tiles.extend(fetched);
        });
    }
    Ok(tiles)
}

fn with_cors(mut resp: Response) -> Result<Response> {
    let h = resp.headers_mut();
    h.set("Access-Control-Allow-Origin", "*")?;
    h.set("Access-Control-Allow-Methods", "GET, POST, OPTIONS")?;
    h.set("Access-Control-Allow-Headers", "*")?;
    h.set("Access-Control-Max-Age", "3600")?;
    Ok(resp)
}
