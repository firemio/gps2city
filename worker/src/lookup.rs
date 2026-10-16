//! タイルの読み込みと、点の所属判定・近傍検索。Workers に依存しない純粋な計算部分。
//! タイル形式は app/src/bin/build_tiles.rs を参照。

use std::collections::HashMap;

// app/src/bin/build_tiles.rs と同じ値にすること
pub const TILES_PER_DEG: f64 = 10.0;
const TILE_SIZE: f64 = 1.0 / TILES_PER_DEG;
const EPS: f64 = 1e-7;
const MAGIC: &[u8; 4] = b"GTL1";

const EARTH_RADIUS_M: f64 = 6_371_008.8;
const M_PER_DEG: f64 = EARTH_RADIUS_M * std::f64::consts::PI / 180.0;

// 切り取りで生じた辺はこの座標の上に乗る (build_tiles と同じ式で求めるので値が完全に一致する)
fn border_lo() -> f32 {
    (-EPS) as f32
}
fn border_hi() -> f32 {
    (TILE_SIZE + EPS) as f32
}

pub struct Area {
    pub code: String,
    pub prefecture: String,
    pub city: String,
    #[allow(dead_code)] // 政令市の区 (N03_005)。API ではまだ返していない
    pub ward: String,
}

struct Polygon {
    area: u16,
    rings: Vec<Vec<[f32; 2]>>,
}

pub struct Tile {
    origin: (f64, f64),
    areas: Vec<Area>,
    polygons: Vec<Polygon>,
}

pub fn tile_index(lon: f64, lat: f64) -> (i32, i32) {
    ((lon * TILES_PER_DEG).floor() as i32, (lat * TILES_PER_DEG).floor() as i32)
}

pub fn tile_path((ix, iy): (i32, i32)) -> String {
    format!("/data/tiles/{ix}_{iy}.bin")
}

/// 中心から半径 radius_m の円に掛かるタイルの一覧
pub fn tiles_in_radius(lon: f64, lat: f64, radius_m: f64) -> Vec<(i32, i32)> {
    let dlat = radius_m / M_PER_DEG;
    let widest = (lat.abs() + dlat).min(89.0).to_radians().cos();
    let dlon = radius_m / (M_PER_DEG * widest);
    let (x0, y0) = tile_index(lon - dlon, lat - dlat);
    let (x1, y1) = tile_index(lon + dlon, lat + dlat);
    let kx = M_PER_DEG * widest;
    let mut tiles = Vec::new();
    for ix in x0..=x1 {
        for iy in y0..=y1 {
            // 外接矩形の角に当たるタイルのうち、円から外れるものは読まない
            let (ox, oy) = (ix as f64 / TILES_PER_DEG, iy as f64 / TILES_PER_DEG);
            let dx = (ox - lon).max(lon - (ox + TILE_SIZE)).max(0.0) * kx;
            let dy = (oy - lat).max(lat - (oy + TILE_SIZE)).max(0.0) * M_PER_DEG;
            if dx * dx + dy * dy <= radius_m * radius_m * 1.01 {
                tiles.push((ix, iy));
            }
        }
    }
    tiles
}

impl Tile {
    pub fn decode((ix, iy): (i32, i32), bytes: &[u8]) -> Result<Tile, String> {
        let mut r = Reader { buf: bytes, pos: 0 };
        if r.take(4)? != MAGIC {
            return Err("bad tile magic".into());
        }
        let n_areas = r.u16()? as usize;
        let mut areas = Vec::with_capacity(n_areas);
        for _ in 0..n_areas {
            areas.push(Area { code: r.string()?, prefecture: r.string()?, city: r.string()?, ward: r.string()? });
        }
        let n_polys = r.u32()? as usize;
        let mut polygons = Vec::with_capacity(n_polys);
        for _ in 0..n_polys {
            let area = r.u16()?;
            let n_rings = r.u16()? as usize;
            let mut rings = Vec::with_capacity(n_rings);
            for _ in 0..n_rings {
                let n = r.u32()? as usize;
                let mut ring = Vec::with_capacity(n);
                for _ in 0..n {
                    ring.push([r.f32()?, r.f32()?]);
                }
                rings.push(ring);
            }
            polygons.push(Polygon { area, rings });
        }
        Ok(Tile { origin: (ix as f64 / TILES_PER_DEG, iy as f64 / TILES_PER_DEG), areas, polygons })
    }

    /// 点を含むエリア。点はこのタイルの中にあること。
    pub fn locate(&self, lon: f64, lat: f64) -> Option<&Area> {
        let (px, py) = ((lon - self.origin.0) as f32, (lat - self.origin.1) as f32);
        self.polygons
            .iter()
            .find(|poly| contains(&poly.rings, px, py))
            .map(|poly| &self.areas[poly.area as usize])
    }

    pub fn approx_bytes(&self) -> usize {
        self.polygons.iter().flat_map(|p| &p.rings).map(|r| r.len() * 8).sum::<usize>() + 64
    }
}

/// 偶奇判定。穴 (2本目以降のリング) も同じ規則で除外される。
fn contains(rings: &[Vec<[f32; 2]>], px: f32, py: f32) -> bool {
    let mut inside = false;
    for ring in rings {
        let mut j = ring.len() - 1;
        for i in 0..ring.len() {
            let ([xi, yi], [xj, yj]) = (ring[i], ring[j]);
            if (yi > py) != (yj > py) && px < (xj - xi) * (py - yi) / (yj - yi) + xi {
                inside = !inside;
            }
            j = i;
        }
    }
    inside
}

pub struct Nearby {
    pub prefecture: String,
    pub city: String,
    pub distance_m: f64,
}

/// 半径 radius_m 以内にある (都道府県, 市区町村) と、その境界までの距離。点を含む市区町村は距離 0。
/// tiles には tiles_in_radius() のタイルを渡す (海などで存在しないタイルは省いてよい)。
pub fn nearby(tiles: &[&Tile], lon: f64, lat: f64, radius_m: f64) -> Vec<Nearby> {
    // 点の周りを平面とみなして (メートル単位) 境界線上の最寄り点を探し、最後にその点まで haversine で測る
    let kx = M_PER_DEG * lat.to_radians().cos();
    let ky = M_PER_DEG;
    let (lo, hi) = (border_lo(), border_hi());

    // エリアコード -> (平面距離の2乗, 最寄り点の経度, 緯度, エリア)
    let mut best: HashMap<&str, (f64, f64, f64, &Area)> = HashMap::new();
    let home = tile_index(lon, lat);

    for tile in tiles {
        let (tx, ty) = ((tile.origin.0 - lon) * kx, (tile.origin.1 - lat) * ky);
        let in_home_tile = tile_index(tile.origin.0 + TILE_SIZE / 2.0, tile.origin.1 + TILE_SIZE / 2.0) == home;
        for poly in &tile.polygons {
            let area = &tile.areas[poly.area as usize];
            if in_home_tile && contains(&poly.rings, (lon - tile.origin.0) as f32, (lat - tile.origin.1) as f32) {
                best.insert(&area.code, (0.0, lon, lat, area));
                continue;
            }
            for ring in &poly.rings {
                let mut a = ring[ring.len() - 1];
                for &b in ring {
                    let cut = (a[0] == b[0] && (a[0] == lo || a[0] == hi)) || (a[1] == b[1] && (a[1] == lo || a[1] == hi));
                    if !cut {
                        let (ax, ay) = (tx + a[0] as f64 * kx, ty + a[1] as f64 * ky);
                        let (bx, by) = (tx + b[0] as f64 * kx, ty + b[1] as f64 * ky);
                        let (ex, ey) = (bx - ax, by - ay);
                        let len2 = ex * ex + ey * ey;
                        let t = if len2 > 0.0 { (-(ax * ex + ay * ey) / len2).clamp(0.0, 1.0) } else { 0.0 };
                        let (cx, cy) = (ax + t * ex, ay + t * ey);
                        let d2 = cx * cx + cy * cy;
                        let entry = best.entry(&area.code).or_insert((f64::INFINITY, 0.0, 0.0, area));
                        if d2 < entry.0 {
                            *entry = (d2, lon + cx / kx, lat + cy / ky, area);
                        }
                    }
                    a = b;
                }
            }
        }
    }

    // 政令市の区などは同じ (都道府県, 市区町村) にまとめる
    let mut merged: HashMap<(&str, &str), f64> = HashMap::new();
    for (d2, nlon, nlat, area) in best.into_values() {
        let d = if d2 == 0.0 { 0.0 } else { haversine(lon, lat, nlon, nlat) };
        if d <= radius_m {
            let e = merged.entry((&area.prefecture, &area.city)).or_insert(f64::INFINITY);
            *e = e.min(d);
        }
    }
    merged
        .into_iter()
        .map(|((p, c), d)| Nearby { prefecture: p.to_string(), city: c.to_string(), distance_m: d })
        .collect()
}

pub fn haversine(lon1: f64, lat1: f64, lon2: f64, lat2: f64) -> f64 {
    let (p1, p2) = (lat1.to_radians(), lat2.to_radians());
    let dp = p2 - p1;
    let dl = (lon2 - lon1).to_radians();
    let a = (dp / 2.0).sin().powi(2) + p1.cos() * p2.cos() * (dl / 2.0).sin().powi(2);
    2.0 * EARTH_RADIUS_M * a.sqrt().asin()
}

struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], String> {
        let s = self.buf.get(self.pos..self.pos + n).ok_or("truncated tile")?;
        self.pos += n;
        Ok(s)
    }
    fn u16(&mut self) -> Result<u16, String> {
        Ok(u16::from_le_bytes(self.take(2)?.try_into().unwrap()))
    }
    fn u32(&mut self) -> Result<u32, String> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
    fn f32(&mut self) -> Result<f32, String> {
        Ok(f32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
    fn string(&mut self) -> Result<String, String> {
        let n = self.take(1)?[0] as usize;
        String::from_utf8(self.take(n)?.to_vec()).map_err(|e| e.to_string())
    }
}
