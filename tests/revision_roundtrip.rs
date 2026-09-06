//! Edit revisions hash serialized JSON. Loading a saved sidecar must preserve those bytes.
//! This guards the serde_json float_roundtrip feature without linking the gallery application.

use serde_json::{json, Value};

#[test]
fn saved_edit_history_preserves_timestamp_and_precise_adjustment_values() {
    // These Unix timestamps lose one ULP with serde_json's default float parser.
    // The first was captured from an immediate PUT -> /render revision conflict.
    for timestamp in [
        1788694795.4703007_f64,
        1788691234.0039895,
        1788691234.0249345,
    ] {
        let history = json!([
            {"op": "auto", "params": {"version": 3}, "ts": timestamp},
            {"op": "adjust", "params": {
                "exposure": 0.10000000000000002_f64,
                "contrast": 0.30000000000000004_f64,
                "saturation": -0.49999999999999994_f64,
                "temperature": 0.9999999999999999_f64
            }, "ts": timestamp}
        ]);
        let expected = serde_json::to_vec(&history).unwrap();
        let mut serialized = expected.clone();
        for cycle in 0..4 {
            let reloaded: Value = serde_json::from_slice(&serialized).unwrap();
            assert_eq!(
                reloaded[0]["ts"].as_f64().unwrap().to_bits(),
                timestamp.to_bits(),
                "timestamp changed on sidecar reload {cycle}"
            );
            serialized = serde_json::to_vec(&reloaded).unwrap();
            assert_eq!(
                serialized, expected,
                "revision input JSON changed on sidecar reload {cycle}"
            );
        }
    }
}
