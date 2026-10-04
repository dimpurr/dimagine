use dimagine_eagle::name_from_url;
use serde_json::Value;
use std::path::PathBuf;

#[test]
fn url_names_table_matches_shared_fixture() {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/url-names.json");
    let bytes =
        std::fs::read(&path).unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
    let cases: Value = serde_json::from_slice(&bytes).unwrap();
    let cases = cases.as_array().expect("fixture is a JSON array");
    assert!(cases.len() >= 30, "expected at least 30 cases");
    for case in cases {
        let url = case["url"].as_str().expect("url string");
        let expected = case["name"].as_str().map(str::to_owned);
        assert_eq!(name_from_url(url), expected, "url: {url}");
    }
}
