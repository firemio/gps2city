use actix_cors::Cors;
use actix_files as fs;
use actix_web::{web, App, HttpResponse, HttpServer, Responder};
use geo::{Contains, HaversineDistance, LineString, Point, Polygon};
use geojson::{GeoJson, Value};
use rayon::prelude::*;
use serde::Deserialize;
use std::{fs::File, io::{BufReader, Read}, sync::Arc, collections::HashSet}; use tokio::sync::RwLock;
use std::time::Instant;
use rstar::{RTree, RTreeObject, AABB};

use futures::future::join_all;


#[derive(Deserialize, Debug)]
struct Location {
    latitude: f64,
    longitude: f64,
    radius_km: Option<f64>, // Optional radius in kilometers
    sort_by: Option<String>, // Optional sorting method: "distance" or "prefecture"
}

struct AppState {
    polygons: Arc<RwLock<Vec<(Polygon<f64>, String, String)>>>,
}

async fn get_location(data: web::Data<AppState>, location: web::Json<Location>) -> impl Responder {
    println!("Received data: {:?}", location);

    let point = Point::new(location.longitude, location.latitude);
    let polygons = data.polygons.read().await;
    for (poly, prefecture, city) in polygons.iter() {
        if poly.contains(&point) {
            return HttpResponse::Ok().json(serde_json::json!({ "prefecture": prefecture, "city": city }));
        }
    }
    HttpResponse::NotFound().json(serde_json::json!({ "error": "Location not found" }))
}

async fn get_nearby_locations(data: web::Data<AppState>, location: web::Json<Location>) -> impl Responder {
    println!("Received data: {:?}", location);

    let point = Point::new(location.longitude, location.latitude);
    let radius_km = location.radius_km.unwrap_or(10.0); // Default radius 10 km if not specified
    let radius_m = radius_km * 1000.0; // Default radius 10 km if not specified
    let sort_by = location.sort_by.as_deref().unwrap_or("distance"); // Default sort by distance
    let polygons = data.polygons.read().await;
    let polygons_ref = &*polygons; // Get a reference to the vector

    // Use rayon's par_chunks method on the vector reference
    let polygons_chunks: Vec<_> = polygons_ref.par_chunks(polygons_ref.len() / 500).collect();
    let start_time = Instant::now();

    let futures = polygons_chunks.into_iter().map(|chunk| {
        let point = point.clone();
        let radius_m = radius_m.clone();
        async move {
            let mut local_nearby_locations = vec![];
            for (poly, prefecture, city) in chunk.iter() {
                for coord in poly.exterior().0.iter() {
                    let vertex = Point::new(coord.x, coord.y);
                    let distance = point.haversine_distance(&vertex);
                    if distance <= radius_m {
                        // Check if the location is already added
                        if !local_nearby_locations.iter().any(|(_, p, c)| p == prefecture && c == city) {
                            local_nearby_locations.push((distance, prefecture.clone(), city.clone()));
                            break; // Stop checking other vertices once added
                        }
                    }
                }
            }
            local_nearby_locations
        }
    });

    let results = join_all(futures).await;
    let mut nearby_locations = vec![];
    let mut unique_check = HashSet::new();

    for result in results {
        for (distance, prefecture, city) in result {
            if unique_check.insert((prefecture.clone(), city.clone())) {
                nearby_locations.push((distance, prefecture, city));
            }
        }
    }

    let duration = start_time.elapsed();
    println!("処理に掛かった時間: {:.3}秒", duration.as_secs_f64());
    println!("見つかった件数: {}", nearby_locations.len());
    // Sort and group by the specified method
    if sort_by == "prefecture" {
        // Group by prefecture and find minimum distance for each prefecture
        let mut grouped_locations: std::collections::HashMap<String, Vec<(f64, String)>> = std::collections::HashMap::new();
        for (distance, prefecture, city) in nearby_locations {
            grouped_locations.entry(prefecture).or_insert_with(Vec::new).push((distance, city));
        }

        // Sort prefectures by the minimum distance found in each
        let mut sorted_prefectures: Vec<_> = grouped_locations.iter().map(|(prefecture, cities)| {
            let min_distance = cities.iter().fold(f64::INFINITY, |acc, (dist, _)| f64::min(acc, *dist));
            (min_distance, prefecture, cities)
        }).collect();
        sorted_prefectures.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());

        // Sort cities within each prefecture by distance
        let mut result = Vec::new();
        for (_, prefecture, cities) in sorted_prefectures {
            let mut sorted_cities: Vec<_> = cities.iter().map(|(dist, city)| (dist, city)).collect();
            sorted_cities.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());
            for (dist, city) in sorted_cities {
                result.push(serde_json::json!({ "prefecture": prefecture, "city": city, "distance": dist }));
            }
        }

        HttpResponse::Ok().json(result)
    } else {
        // Default sort by distance
        nearby_locations.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());
        let result: Vec<_> = nearby_locations.into_iter().map(|(distance, prefecture, city)| {
            serde_json::json!({ "prefecture": prefecture, "city": city, "distance": distance })
        }).collect();
        HttpResponse::Ok().json(result)
    }

}


#[actix_web::main]
async fn main() -> std::io::Result<()> {
    let file = File::open("japan.geojson").expect("Failed to open GeoJSON file");
    let mut reader = BufReader::new(file);
    let mut content = String::new();
    println!("Reading GeoJSON file...");
    reader.read_to_string(&mut content).expect("Failed to read GeoJSON file");
    println!("Parsing JSON data...");
    let start_parse_time = std::time::SystemTime::now();
    let geojson: GeoJson = content.parse().expect("Failed to parse GeoJSON");
    let end_parse_time = std::time::SystemTime::now();
    let parse_duration = end_parse_time.duration_since(start_parse_time).expect("Time went backwards");
    println!("GeoJSONの解析に{:.3}秒かかりました", parse_duration.as_secs_f64());
    // let geojson = serde_json::from_slice::<GeoJson>(&geojson_str).expect("Failed to parse GeoJSON");

    let start_time = std::time::SystemTime::now();
    let start_millis = start_time.duration_since(std::time::UNIX_EPOCH).expect("Time went backwards").as_millis();

    let mut polygons = Vec::new();
    println!("Processing Polygon data...");
    if let GeoJson::FeatureCollection(collection) = geojson {
        for feature in &collection.features {
            if let Some(geometry) = &feature.geometry {
                if let Value::Polygon(polygon) = &geometry.value {
                    let exterior_coords: Vec<_> = polygon[0].iter().map(|coord| (coord[0], coord[1])).collect();
                    let exterior = LineString::from(exterior_coords);
                    let poly = Polygon::new(exterior, vec![]);
                    let prefecture = feature.property("N03_001").map_or_else(|| "Unknown".to_string(), |v| v.to_string());
                    let city = feature.property("N03_004").map_or_else(|| "Unknown".to_string(), |v| v.to_string());
                    polygons.push((poly, prefecture, city));
                }
            }
        }
    }

    

    println!("=== server start ====");

    let app_state = web::Data::new(AppState {
        polygons: Arc::new(RwLock::new(polygons)),
    });

    HttpServer::new(move || {
        let cors = Cors::default()
            .allow_any_origin()
            .allow_any_method()
            .allow_any_header()
            .max_age(3600);

        App::new()
            .app_data(app_state.clone())
            .wrap(cors)
            .route("/location", web::post().to(get_location))
            .route("/location2", web::post().to(get_nearby_locations))
            .service(fs::Files::new("/", "./static").index_file("geo.html"))
    })
    .bind("127.0.0.1:3000")?
    .run()
    .await
}