//! japan.geojson を 0.1° 四方のタイルに分割し、Workers 版 (worker/) が読むバイナリを書き出す。
//!
//!   cargo run --release --bin build_tiles -- japan.geojson ../worker/public/data
//!
//! タイル内のポリゴンはタイル境界で切り取って (Sutherland–Hodgman) 保存する。
//! 切り取りで生じた辺は必ずタイル境界線上に乗るので、Worker 側はそれを見分けて距離計算から除外する。
//!
//! タイル形式 (リトルエンディアン):
//!   b"GTL1"
//!   u16 エリア数, エリアごとに [u8 長さ + UTF-8] × 4 (code, prefecture, city, ward)
//!   u32 ポリゴン数, ポリゴンごとに
//!     u16 エリア番号 (このタイル内), u16 リング数, リングごとに
//!       u32 点数, 点ごとに f32 dx, f32 dy (タイル原点からの度数。閉じ点は含まない)

use serde::Deserialize;
use std::collections::HashMap;
use std::fs;
use std::io::{self, BufReader, Write};
use std::path::Path;
use std::time::Instant;

// worker/src/tile.rs と同じ値にすること
const TILES_PER_DEG: f64 = 10.0;
const TILE_SIZE: f64 = 1.0 / TILES_PER_DEG;
const EPS: f64 = 1e-7; // タイル境界ぴったりの点でも判定できるよう、切り取り範囲を少し広げる
const MAGIC: &[u8; 4] = b"GTL1";

#[derive(Deserialize)]
struct FeatureCollection {
    features: Vec<Feature>,
}

#[derive(Deserialize)]
struct Feature {
    properties: Props,
    geometry: Option<Geometry>,
}

#[derive(Deserialize)]
struct Props {
    #[serde(rename = "N03_001")]
    prefecture: Option<String>,
    #[serde(rename = "N03_004")]
    city: Option<String>,
    #[serde(rename = "N03_005")]
    ward: Option<String>,
    #[serde(rename = "N03_007")]
    code: Option<String>,
}

#[derive(Deserialize)]
#[serde(tag = "type", content = "coordinates")]
enum Geometry {
    Polygon(Vec<Vec<[f64; 2]>>),
    MultiPolygon(Vec<Vec<Vec<[f64; 2]>>>),
}

struct Area {
    code: String,
    prefecture: String,
    city: String,
    ward: String,
}

#[derive(Default)]
struct Tile {
    areas: Vec<usize>,              // タイル内エリア番号 -> 全体のエリア番号
    area_index: HashMap<usize, u16>, // 全体のエリア番号 -> タイル内エリア番号
    polygons: Vec<(u16, Vec<Vec<(f32, f32)>>)>,
}

fn main() -> io::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let input = args.get(1).map(String::as_str).unwrap_or("japan.geojson");
    let out_dir = Path::new(args.get(2).map(String::as_str).unwrap_or("../worker/public/data"));

    let started = Instant::now();
    println!("Reading {input} ...");
    let reader = BufReader::with_capacity(1 << 20, fs::File::open(input)?);
    let fc: FeatureCollection = serde_json::from_reader(reader)?;
    println!("  {} features ({:.1}s)", fc.features.len(), started.elapsed().as_secs_f64());

    let mut areas: Vec<Area> = Vec::new();
    let mut area_by_code: HashMap<String, usize> = HashMap::new();
    let mut tiles: HashMap<(i32, i32), Tile> = HashMap::new();

    for feature in &fc.features {
        let Some(geometry) = &feature.geometry else { continue };
        let p = &feature.properties;
        let code = p.code.clone().unwrap_or_default();
        let area = *area_by_code.entry(code.clone()).or_insert_with(|| {
            areas.push(Area {
                code,
                prefecture: p.prefecture.clone().unwrap_or_default(),
                city: p.city.clone().unwrap_or_default(),
                ward: p.ward.clone().unwrap_or_default(),
            });
            areas.len() - 1
        });

        let polygons: Vec<&Vec<Vec<[f64; 2]>>> = match geometry {
            Geometry::Polygon(rings) => vec![rings],
            Geometry::MultiPolygon(polys) => polys.iter().collect(),
        };
        for rings in polygons {
            add_polygon(&mut tiles, area, rings);
        }
    }
    println!("  {} areas, {} tiles ({:.1}s)", areas.len(), tiles.len(), started.elapsed().as_secs_f64());

    let tile_dir = out_dir.join("tiles");
    if tile_dir.exists() {
        fs::remove_dir_all(&tile_dir)?;
    }
    fs::create_dir_all(&tile_dir)?;

    let (mut total, mut largest) = (0usize, (0usize, (0, 0)));
    for (&(ix, iy), tile) in &tiles {
        let bytes = encode_tile(tile, &areas);
        total += bytes.len();
        if bytes.len() > largest.0 {
            largest = (bytes.len(), (ix, iy));
        }
        fs::File::create(tile_dir.join(format!("{ix}_{iy}.bin")))?.write_all(&bytes)?;
    }
    println!(
        "Wrote {} tiles to {} : total {:.1} MB, largest {:.0} KB ({}_{}) ({:.1}s)",
        tiles.len(),
        tile_dir.display(),
        total as f64 / 1e6,
        largest.0 as f64 / 1e3,
        largest.1 .0,
        largest.1 .1,
        started.elapsed().as_secs_f64()
    );
    Ok(())
}

fn add_polygon(tiles: &mut HashMap<(i32, i32), Tile>, area: usize, rings: &[Vec<[f64; 2]>]) {
    let Some(outer) = rings.first() else { return };
    let (mut min_x, mut min_y, mut max_x, mut max_y) = (f64::MAX, f64::MAX, f64::MIN, f64::MIN);
    for &[x, y] in outer {
        min_x = min_x.min(x);
        min_y = min_y.min(y);
        max_x = max_x.max(x);
        max_y = max_y.max(y);
    }
    let tile_range = |lo: f64, hi: f64| {
        ((lo - EPS) * TILES_PER_DEG).floor() as i32..=((hi + EPS) * TILES_PER_DEG).floor() as i32
    };

    for ix in tile_range(min_x, max_x) {
        for iy in tile_range(min_y, max_y) {
            let (ox, oy) = (ix as f64 / TILES_PER_DEG, iy as f64 / TILES_PER_DEG);
            let mut clipped = Vec::new();
            for (i, ring) in rings.iter().enumerate() {
                let local: Vec<(f64, f64)> = ring.iter().map(|&[x, y]| (x - ox, y - oy)).collect();
                let ring = clip_ring(&local, -EPS, TILE_SIZE + EPS);
                if ring.is_empty() {
                    if i == 0 {
                        break; // 外周がタイルに掛からなければ、穴も掛からない
                    }
                    continue;
                }
                clipped.push(ring.iter().map(|&(x, y)| (x as f32, y as f32)).collect());
            }
            if clipped.is_empty() {
                continue;
            }
            let tile = tiles.entry((ix, iy)).or_default();
            let local_area = *tile.area_index.entry(area).or_insert_with(|| {
                tile.areas.push(area);
                (tile.areas.len() - 1) as u16
            });
            tile.polygons.push((local_area, clipped));
        }
    }
}

/// リングを正方形 [lo, hi] × [lo, hi] で切り取る (Sutherland–Hodgman)。
/// 交点の座標は境界値をそのまま代入するので、切り取りで生じた辺の両端は境界値と完全に一致する。
fn clip_ring(ring: &[(f64, f64)], lo: f64, hi: f64) -> Vec<(f64, f64)> {
    let mut pts: Vec<(f64, f64)> = ring.to_vec();
    if pts.len() > 1 && pts.first() == pts.last() {
        pts.pop();
    }
    for side in 0..4 {
        if pts.is_empty() {
            break;
        }
        let inside = |p: (f64, f64)| match side {
            0 => p.0 >= lo,
            1 => p.0 <= hi,
            2 => p.1 >= lo,
            _ => p.1 <= hi,
        };
        let cut = |a: (f64, f64), b: (f64, f64)| match side {
            0 | 1 => {
                let x = if side == 0 { lo } else { hi };
                (x, a.1 + (x - a.0) / (b.0 - a.0) * (b.1 - a.1))
            }
            _ => {
                let y = if side == 2 { lo } else { hi };
                (a.0 + (y - a.1) / (b.1 - a.1) * (b.0 - a.0), y)
            }
        };
        let mut out = Vec::with_capacity(pts.len() + 4);
        let mut prev = *pts.last().unwrap();
        for &cur in &pts {
            match (inside(prev), inside(cur)) {
                (true, true) => out.push(cur),
                (false, true) => {
                    out.push(cut(prev, cur));
                    out.push(cur);
                }
                (true, false) => out.push(cut(prev, cur)),
                (false, false) => {}
            }
            prev = cur;
        }
        pts = out;
    }
    pts.dedup();
    if pts.len() < 3 || ring_area(&pts).abs() < 1e-20 {
        return Vec::new(); // 境界線に接しているだけの退化したリング
    }
    pts
}

fn ring_area(pts: &[(f64, f64)]) -> f64 {
    let mut sum = 0.0;
    let mut prev = *pts.last().unwrap();
    for &cur in pts {
        sum += prev.0 * cur.1 - cur.0 * prev.1;
        prev = cur;
    }
    sum / 2.0
}

fn encode_tile(tile: &Tile, areas: &[Area]) -> Vec<u8> {
    let mut buf = Vec::new();
    buf.extend_from_slice(MAGIC);
    buf.extend_from_slice(&(tile.areas.len() as u16).to_le_bytes());
    for &a in &tile.areas {
        let a = &areas[a];
        for s in [&a.code, &a.prefecture, &a.city, &a.ward] {
            assert!(s.len() < 256, "name too long: {s}");
            buf.push(s.len() as u8);
            buf.extend_from_slice(s.as_bytes());
        }
    }
    buf.extend_from_slice(&(tile.polygons.len() as u32).to_le_bytes());
    for (area, rings) in &tile.polygons {
        buf.extend_from_slice(&area.to_le_bytes());
        buf.extend_from_slice(&(rings.len() as u16).to_le_bytes());
        for ring in rings {
            buf.extend_from_slice(&(ring.len() as u32).to_le_bytes());
            for &(x, y) in ring {
                buf.extend_from_slice(&x.to_le_bytes());
                buf.extend_from_slice(&y.to_le_bytes());
            }
        }
    }
    buf
}
