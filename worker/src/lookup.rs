//! タイルの読み込みと、点の所属判定・近傍検索。Workers に依存しない純粋な計算部分。
//! パック・タイルの形式は app/src/bin/build_tiles.rs を参照。

use std::collections::HashMap;

// app/src/bin/build_tiles.rs と同じ値にすること
pub const TILES_PER_DEG: f64 = 10.0;
const TILE_SIZE: f64 = 1.0 / TILES_PER_DEG;
const STEP: f64 = 1e-6;
const EPS: f64 = 2e-6;
const MAGIC: &[u8; 4] = b"GTP1";

const EARTH_RADIUS_M: f64 = 6_371_008.8;
const M_PER_DEG: f64 = EARTH_RADIUS_M * std::f64::consts::PI / 180.0;

fn q_to_f32(q: i32) -> f32 {
    (q as f64 * STEP) as f32
}

// 切り取りで生じた辺はこの座標の上に乗る (build_tiles と同じ式で量子化するので値が完全に一致する)
fn border_lo() -> f32 {
    q_to_f32((-EPS / STEP).round() as i32)
}
fn border_hi() -> f32 {
    q_to_f32(((TILE_SIZE + EPS) / STEP).round() as i32)
}

/// パックからタイル本体を探す。タイルが無ければ (海など) None。
pub fn find_tile(pack: &[u8], (ix, iy): (i32, i32)) -> Result<Option<&[u8]>, String> {
    let header = |i: usize| -> Option<i32> { pack.get(i..i + 4).map(|b| i32::from_le_bytes(b.try_into().unwrap())) };
    if pack.get(..4) != Some(MAGIC) {
        return Err("bad pack magic".into());
    }
    let n = header(4).ok_or("truncated pack")? as usize;
    let entry = |i: usize| -> Option<(i32, i32, usize, usize)> {
        let at = 8 + i * 16;
        Some((header(at)?, header(at + 4)?, header(at + 8)? as u32 as usize, header(at + 12)? as u32 as usize))
    };
    let (mut lo, mut hi) = (0, n);
    while lo < hi {
        let mid = (lo + hi) / 2;
        let (x, y, offset, len) = entry(mid).ok_or("truncated pack index")?;
        match (x, y).cmp(&(ix, iy)) {
            std::cmp::Ordering::Less => lo = mid + 1,
            std::cmp::Ordering::Greater => hi = mid,
            std::cmp::Ordering::Equal => {
                let start = 8 + n * 16 + offset;
                return pack.get(start..start + len).map(Some).ok_or_else(|| "truncated pack body".into());
            }
        }
    }
    Ok(None)
}

pub struct Area {
    pub code: String, // N03_007 (5桁の行政区域コード。政令市は区のコード)
    pub prefecture: String,
    pub city: String,
    pub ward: String, // 政令市の区 (N03_005)。それ以外は空
}

/// 5桁の行政区域コードに検査数字を付けた6桁の全国地方公共団体コード
/// (デジタル庁 アドレス・ベース・レジストリの lg_code と同じ)。5桁の数字でなければ None。
pub fn lg_code(code: &str) -> Option<String> {
    if code.len() != 5 || !code.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let sum: u32 = code.bytes().zip([6, 5, 4, 3, 2]).map(|(b, w)| (b - b'0') as u32 * w).sum();
    Some(format!("{code}{}", (11 - sum % 11) % 10))
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
        let n_areas = r.varint()? as usize;
        let mut areas = Vec::with_capacity(n_areas);
        for _ in 0..n_areas {
            areas.push(Area { code: r.string()?, prefecture: r.string()?, city: r.string()?, ward: r.string()? });
        }
        let n_polys = r.varint()? as usize;
        let mut polygons = Vec::with_capacity(n_polys);
        for _ in 0..n_polys {
            let area = r.varint()? as u16;
            let n_rings = r.varint()? as usize;
            let mut rings = Vec::with_capacity(n_rings);
            for _ in 0..n_rings {
                let n = r.varint()? as usize;
                let mut ring = Vec::with_capacity(n);
                let (mut x, mut y) = (0i32, 0i32);
                for _ in 0..n {
                    x += r.zigzag()?;
                    y += r.zigzag()?;
                    ring.push([q_to_f32(x), q_to_f32(y)]);
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

    /// 境界線まで max_m 以内で最も近いエリア。境界線を間引いたことで隣り合うエリアの間にできた
    /// わずかな隙間 (許容誤差以下) に点が落ちたときに使う。
    pub fn nearest(&self, lon: f64, lat: f64, max_m: f64) -> Option<&Area> {
        let refs = [self];
        let mut best: Option<(f64, &Area)> = None;
        for_each_segment(&refs, lon, lat, |area, d2, _, _| {
            if d2 <= max_m * max_m && best.map_or(true, |(b, _)| d2 < b) {
                best = Some((d2, area));
            }
        });
        best.map(|(_, area)| area)
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
    pub areas: Vec<NearbyArea>, // まとめる前の行政区域 (政令市なら区ごと)。距離順
}

pub struct NearbyArea {
    pub code: String,
    pub ward: String,
    pub distance_m: f64,
}

/// 半径 radius_m 以内にある (都道府県, 市区町村) と、その境界までの距離。点を含む市区町村は距離 0。
/// tiles には tiles_in_radius() のタイルを渡す (海などで存在しないタイルは省いてよい)。
pub fn nearby(tiles: &[&Tile], lon: f64, lat: f64, radius_m: f64) -> Vec<Nearby> {
    let kx = M_PER_DEG * lat.to_radians().cos();
    let ky = M_PER_DEG;

    // エリアコード -> (平面距離の2乗, 最寄り点の経度, 緯度, エリア)
    let mut best: HashMap<&str, (f64, f64, f64, &Area)> = HashMap::new();
    let home = tile_index(lon, lat);
    for tile in tiles {
        if tile_index(tile.origin.0 + TILE_SIZE / 2.0, tile.origin.1 + TILE_SIZE / 2.0) != home {
            continue;
        }
        let (px, py) = ((lon - tile.origin.0) as f32, (lat - tile.origin.1) as f32);
        for poly in tile.polygons.iter().filter(|p| contains(&p.rings, px, py)) {
            let area = &tile.areas[poly.area as usize];
            best.insert(&area.code, (0.0, lon, lat, area));
        }
    }
    // 点の周りを平面とみなして境界線上の最寄り点を探し、最後にその点まで haversine で測る
    for_each_segment(tiles, lon, lat, |area, d2, cx, cy| {
        let entry = best.entry(&area.code).or_insert((f64::INFINITY, 0.0, 0.0, area));
        if d2 < entry.0 {
            *entry = (d2, lon + cx / kx, lat + cy / ky, area);
        }
    });

    // 政令市の区などは同じ (都道府県, 市区町村) にまとめ、区ごとの距離は areas に残す
    let mut merged: HashMap<(&str, &str), Vec<NearbyArea>> = HashMap::new();
    for (d2, nlon, nlat, area) in best.into_values() {
        let d = if d2 == 0.0 { 0.0 } else { haversine(lon, lat, nlon, nlat) };
        if d <= radius_m {
            merged.entry((&area.prefecture, &area.city)).or_default().push(NearbyArea {
                code: area.code.clone(),
                ward: area.ward.clone(),
                distance_m: d,
            });
        }
    }
    merged
        .into_iter()
        .map(|((p, c), mut areas)| {
            areas.sort_by(|a, b| a.distance_m.total_cmp(&b.distance_m).then_with(|| a.code.cmp(&b.code)));
            Nearby { prefecture: p.to_string(), city: c.to_string(), distance_m: areas[0].distance_m, areas }
        })
        .collect()
}

/// 本来の境界線 (切り取りで生じた辺を除く) の各線分について、点からの距離を f(エリア, 距離², 最寄り点 x, y) に渡す。
/// 座標は点を原点とし、周りを平面とみなしたメートル単位。
fn for_each_segment<'a>(tiles: &[&'a Tile], lon: f64, lat: f64, mut f: impl FnMut(&'a Area, f64, f64, f64)) {
    let kx = M_PER_DEG * lat.to_radians().cos();
    let ky = M_PER_DEG;
    let (lo, hi) = (border_lo(), border_hi());
    for &tile in tiles {
        let (tx, ty) = ((tile.origin.0 - lon) * kx, (tile.origin.1 - lat) * ky);
        for poly in &tile.polygons {
            let area = &tile.areas[poly.area as usize];
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
                        f(area, cx * cx + cy * cy, cx, cy);
                    }
                    a = b;
                }
            }
        }
    }
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
    fn varint(&mut self) -> Result<u64, String> {
        let mut v = 0u64;
        for shift in (0..64).step_by(7) {
            let b = *self.buf.get(self.pos).ok_or("truncated tile")?;
            self.pos += 1;
            v |= ((b & 0x7f) as u64) << shift;
            if b & 0x80 == 0 {
                return Ok(v);
            }
        }
        Err("bad varint".into())
    }
    fn zigzag(&mut self) -> Result<i32, String> {
        let v = self.varint()? as u32;
        Ok((v >> 1) as i32 ^ -((v & 1) as i32))
    }
    fn string(&mut self) -> Result<String, String> {
        let n = self.varint()? as usize;
        String::from_utf8(self.take(n)?.to_vec()).map_err(|e| e.to_string())
    }
}
