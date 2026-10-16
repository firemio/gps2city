//! japan.geojson を 0.1° 四方のタイルに分割し、Workers 版 (worker/) に埋め込む 1 本のファイルに書き出す。
//!
//!   cargo run --release -- japan.geojson ../../worker/data/tiles.pack [許容誤差m (既定 1)]
//!
//! 境界線は Douglas–Peucker で許容誤差まで間引き、タイル境界で切り取って (Sutherland–Hodgman) 保存する。
//! 切り取りで生じた辺は必ずタイル境界線上に乗るので、Worker 側はそれを見分けて距離計算から除外する。
//!
//! パック形式 (リトルエンディアン):
//!   b"GTP1", u32 タイル数, タイルごとに (i32 ix, i32 iy, u32 オフセット, u32 長さ) を (ix, iy) 順に, 続けてタイル本体
//! タイル本体 (数値はすべて LEB128 可変長整数。座標差分は zigzag):
//!   エリア数, エリアごとに [長さ + UTF-8] × 4 (code, prefecture, city, ward)
//!   ポリゴン数, ポリゴンごとに
//!     エリア番号 (このタイル内), リング数, リングごとに
//!       点数, 点ごとに dx, dy (タイル原点から STEP 度単位の座標の、前の点との差。閉じ点は含まない)

use serde::Deserialize;
use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::io::{self, BufReader};
use std::path::Path;
use std::time::Instant;

// worker/src/lookup.rs と同じ値にすること
const TILES_PER_DEG: f64 = 10.0;
const TILE_SIZE: f64 = 1.0 / TILES_PER_DEG;
const STEP: f64 = 1e-6; // 座標の量子化単位 (約 0.1m)
const EPS: f64 = 2e-6; // タイル境界ぴったりの点でも判定できるよう、切り取り範囲を少し広げる
const MAGIC: &[u8; 4] = b"GTP1";

const M_PER_DEG: f64 = 6_371_008.8 * std::f64::consts::PI / 180.0;

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
    areas: Vec<usize>,               // タイル内エリア番号 -> 全体のエリア番号
    area_index: HashMap<usize, u16>, // 全体のエリア番号 -> タイル内エリア番号
    polygons: Vec<(u16, Vec<Vec<(i32, i32)>>)>,
}

fn main() -> io::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let input = args.get(1).map(String::as_str).unwrap_or("japan.geojson");
    let output = Path::new(args.get(2).map(String::as_str).unwrap_or("../../worker/data/tiles.pack"));
    let tolerance_m: f64 = args.get(3).map_or(1.0, |s| s.parse().expect("tolerance must be a number (meters)"));

    let started = Instant::now();
    println!("Reading {input} ...");
    let reader = BufReader::with_capacity(1 << 20, fs::File::open(input)?);
    let fc: FeatureCollection = serde_json::from_reader(reader)?;
    println!("  {} features ({:.1}s)", fc.features.len(), started.elapsed().as_secs_f64());

    let mut areas: Vec<Area> = Vec::new();
    let mut area_by_code: HashMap<String, usize> = HashMap::new();
    let mut tiles: HashMap<(i32, i32), Tile> = HashMap::new();
    let (mut before, mut after) = (0usize, 0usize);

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
            before += rings.iter().map(|r| r.len().saturating_sub(1)).sum::<usize>(); // 閉じ点を除く
            let rings: Vec<Vec<[f64; 2]>> = rings.iter().map(|r| simplify_ring(r, tolerance_m)).collect();
            after += rings.iter().map(Vec::len).sum::<usize>();
            add_polygon(&mut tiles, area, &rings);
        }
    }
    println!(
        "  simplified to {tolerance_m}m: {before} -> {after} vertices ({:.0}%), {} areas, {} tiles ({:.1}s)",
        after as f64 / before as f64 * 100.0,
        areas.len(),
        tiles.len(),
        started.elapsed().as_secs_f64()
    );

    let encoded: BTreeMap<(i32, i32), Vec<u8>> = tiles.iter().map(|(&k, t)| (k, encode_tile(t, &areas))).collect();
    let mut pack = Vec::new();
    pack.extend_from_slice(MAGIC);
    pack.extend_from_slice(&(encoded.len() as u32).to_le_bytes());
    let mut offset = 0u32;
    for (&(ix, iy), blob) in &encoded {
        for v in [ix as u32, iy as u32, offset, blob.len() as u32] {
            pack.extend_from_slice(&v.to_le_bytes());
        }
        offset += blob.len() as u32;
    }
    for blob in encoded.values() {
        pack.extend_from_slice(blob);
    }
    if let Some(dir) = output.parent() {
        fs::create_dir_all(dir)?;
    }
    fs::write(output, &pack)?;
    let largest = encoded.values().map(Vec::len).max().unwrap_or(0);
    println!(
        "Wrote {} : {} tiles, {:.1} MB, largest tile {:.0} KB ({:.1}s)",
        output.display(),
        encoded.len(),
        pack.len() as f64 / 1e6,
        largest as f64 / 1e3,
        started.elapsed().as_secs_f64()
    );
    Ok(())
}

/// 閉じたリングを Douglas–Peucker で間引く (許容誤差はメートル)。閉じ点は取り除いて返す。
/// 間引くと 4 点未満になる小さな島などは元のまま残す。
fn simplify_ring(ring: &[[f64; 2]], tolerance_m: f64) -> Vec<[f64; 2]> {
    let mut pts = ring.to_vec();
    if pts.len() > 1 && pts.first() == pts.last() {
        pts.pop();
    }
    let n = pts.len();
    if tolerance_m <= 0.0 || n < 8 {
        return pts;
    }
    let kx = M_PER_DEG * pts[0][1].to_radians().cos();
    let xy: Vec<(f64, f64)> = pts.iter().map(|p| (p[0] * kx, p[1] * M_PER_DEG)).collect();
    let at = |i: usize| xy[i % n];
    let dist2 = |a: (f64, f64), b: (f64, f64)| (a.0 - b.0).powi(2) + (a.1 - b.1).powi(2);

    // リングを点 0 と、そこから最も遠い点で 2 本の折れ線に分けて、それぞれを間引く
    let far = (1..n).max_by(|&a, &b| dist2(xy[0], xy[a]).total_cmp(&dist2(xy[0], xy[b]))).unwrap();
    let mut keep = vec![false; n];
    keep[0] = true;
    keep[far] = true;
    let mut stack = vec![(0, far), (far, n)]; // 添字 n は点 0 を指す
    while let Some((s, e)) = stack.pop() {
        let (a, b) = (at(s), at(e));
        let (mut worst, mut wi) = (0.0, 0);
        for i in s + 1..e {
            let d = segment_dist2(at(i), a, b);
            if d > worst {
                worst = d;
                wi = i;
            }
        }
        if worst > tolerance_m * tolerance_m {
            keep[wi] = true;
            stack.push((s, wi));
            stack.push((wi, e));
        }
    }
    let out: Vec<[f64; 2]> = (0..n).filter(|&i| keep[i]).map(|i| pts[i]).collect();
    if out.len() < 4 {
        pts
    } else {
        out
    }
}

fn segment_dist2(p: (f64, f64), a: (f64, f64), b: (f64, f64)) -> f64 {
    let (ex, ey) = (b.0 - a.0, b.1 - a.1);
    let len2 = ex * ex + ey * ey;
    let t = if len2 > 0.0 { (((p.0 - a.0) * ex + (p.1 - a.1) * ey) / len2).clamp(0.0, 1.0) } else { 0.0 };
    (a.0 + t * ex - p.0).powi(2) + (a.1 + t * ey - p.1).powi(2)
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
                let ring = quantize(&clip_ring(&local, -EPS, TILE_SIZE + EPS));
                if ring.is_empty() {
                    if i == 0 {
                        break; // 外周がタイルに掛からなければ、穴も掛からない
                    }
                    continue;
                }
                clipped.push(ring);
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
    pts
}

/// STEP 単位の整数座標にする。重なった点を除き、面積の無いリング (境界線に接しているだけ) は空にする。
fn quantize(ring: &[(f64, f64)]) -> Vec<(i32, i32)> {
    let mut q: Vec<(i32, i32)> = ring.iter().map(|&(x, y)| ((x / STEP).round() as i32, (y / STEP).round() as i32)).collect();
    q.dedup();
    while q.len() > 1 && q.first() == q.last() {
        q.pop();
    }
    let mut area2 = 0i64;
    if let Some(&last) = q.last() {
        let mut prev = last;
        for &cur in &q {
            area2 += prev.0 as i64 * cur.1 as i64 - cur.0 as i64 * prev.1 as i64;
            prev = cur;
        }
    }
    if q.len() < 3 || area2 == 0 {
        return Vec::new();
    }
    q
}

fn encode_tile(tile: &Tile, areas: &[Area]) -> Vec<u8> {
    let mut buf = Vec::new();
    varint(&mut buf, tile.areas.len() as u64);
    for &a in &tile.areas {
        let a = &areas[a];
        for s in [&a.code, &a.prefecture, &a.city, &a.ward] {
            varint(&mut buf, s.len() as u64);
            buf.extend_from_slice(s.as_bytes());
        }
    }
    varint(&mut buf, tile.polygons.len() as u64);
    for (area, rings) in &tile.polygons {
        varint(&mut buf, *area as u64);
        varint(&mut buf, rings.len() as u64);
        for ring in rings {
            varint(&mut buf, ring.len() as u64);
            let mut prev = (0, 0);
            for &(x, y) in ring {
                for d in [x - prev.0, y - prev.1] {
                    varint(&mut buf, ((d << 1) ^ (d >> 31)) as u32 as u64);
                }
                prev = (x, y);
            }
        }
    }
    buf
}

fn varint(buf: &mut Vec<u8>, mut v: u64) {
    loop {
        let b = (v & 0x7f) as u8;
        v >>= 7;
        if v == 0 {
            buf.push(b);
            return;
        }
        buf.push(b | 0x80);
    }
}
