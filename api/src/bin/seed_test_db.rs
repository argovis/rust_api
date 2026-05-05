// Seeds a MongoDB instance with the test fixtures used by the integration tests.
//
// Run before starting the API container so the API picks up the right
// `timeseriesMeta` document at startup:
//
//     MONGODB_URI=mongodb://localhost:27017 cargo run --bin seed_test_db
//
// What it does:
//   * drops the `argo.bsose` and `argo.timeseriesMeta` collections
//   * loads the JSON fixtures embedded at compile time
//   * converts ISO-8601 strings in known date fields to BSON DateTimes
//   * inserts the resulting documents
//   * creates a 2dsphere index on `geolocation` for the bsose collection
//
// Date fields in the fixtures are written as ISO-8601 strings to keep the
// JSON readable; the seeder converts them to BSON DateTimes here, since
// MongoDB's geo and time queries depend on the typed representation.

use mongodb::{
    bson::{self, Bson, Document, DateTime as BsonDateTime},
    options::ClientOptions,
    Client, IndexModel,
};
use std::env;

const TIMESERIES_META_FIXTURE: &str =
    include_str!("../../fixtures/timeseriesMeta.json");
const BSOSE_FIXTURE: &str = include_str!("../../fixtures/bsose.json");

const DB_NAME: &str = "argo";

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let uri = env::var("MONGODB_URI")
        .expect("MONGODB_URI must be set (e.g. mongodb://localhost:27017)");
    let opts = ClientOptions::parse(&uri).await?;
    let client = Client::with_options(opts)?;
    let db = client.database(DB_NAME);

    // timeseriesMeta has BSON dates in two fields
    seed_collection(
        &db,
        "timeseriesMeta",
        TIMESERIES_META_FIXTURE,
        &["date_updated_argovis", "timeseries"],
    )
    .await?;

    // bsose has no top-level date fields
    seed_collection(&db, "bsose", BSOSE_FIXTURE, &[]).await?;

    // Geospatial queries (`$geoWithin`, `$near`) require a 2dsphere index on
    // the GeoJSON field. MongoDB picks a default index name from the keys.
    let geo_index = IndexModel::builder()
        .keys(bson::doc! { "geolocation": "2dsphere" })
        .build();
    db.collection::<Document>("bsose")
        .create_index(geo_index, None)
        .await?;

    println!("Seed complete: {} populated.", DB_NAME);
    Ok(())
}

async fn seed_collection(
    db: &mongodb::Database,
    name: &str,
    json_str: &str,
    date_fields: &[&str],
) -> Result<(), Box<dyn std::error::Error>> {
    let coll = db.collection::<Document>(name);
    coll.drop(None).await?;

    let value: serde_json::Value = serde_json::from_str(json_str)?;
    let array = value
        .as_array()
        .ok_or_else(|| format!("fixture for {} must be a JSON array", name))?;

    let mut docs: Vec<Document> = Vec::with_capacity(array.len());
    for item in array {
        let bson_val: Bson = bson::to_bson(item)?;
        let mut doc: Document = match bson_val {
            Bson::Document(d) => d,
            other => {
                return Err(format!(
                    "fixture entry for {} must be an object, got {:?}",
                    name, other
                )
                .into())
            }
        };
        convert_date_fields(&mut doc, date_fields);
        docs.push(doc);
    }

    if !docs.is_empty() {
        coll.insert_many(docs.clone(), None).await?;
    }
    println!("  seeded {}: {} documents", name, docs.len());
    Ok(())
}

/// For each named field, convert ISO-8601 strings (or arrays of them) to
/// BSON DateTimes. Anything that doesn't parse is left alone so the failure
/// surfaces during query rather than during seed.
fn convert_date_fields(doc: &mut Document, fields: &[&str]) {
    for field in fields {
        let Some(val) = doc.remove(*field) else { continue };
        let converted = convert_value(val);
        doc.insert(*field, converted);
    }
}

fn convert_value(val: Bson) -> Bson {
    match val {
        Bson::String(s) => match chrono::DateTime::parse_from_rfc3339(&s) {
            Ok(dt) => Bson::DateTime(BsonDateTime::from_millis(dt.timestamp_millis())),
            Err(_) => Bson::String(s),
        },
        Bson::Array(arr) => Bson::Array(arr.into_iter().map(convert_value).collect()),
        other => other,
    }
}
