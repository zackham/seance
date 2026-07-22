//! Persisted split-ratio / pane-weight layout file (`~/.local/share/seance/layout.json`).

use std::collections::HashMap;
use std::path::PathBuf;

pub(super) fn layout_file_path() -> PathBuf {
    PathBuf::from(shellexpand::tilde("~/.local/share/seance/layout.json").into_owned())
}

/// Defaults when the file is missing or unparseable.
fn empty_layout() -> (f32, HashMap<String, f32>, HashMap<String, f32>) {
    (0.5, HashMap::new(), HashMap::new())
}

/// Pure decode: parse layout JSON text into `(split_ratio, weights, row_weights)`.
/// Malformed / non-object JSON (and any missing field) falls back to defaults.
/// The split ratio is clamped to `[0.2, 0.8]` — identical to the on-disk read.
fn parse_layout_json(bytes: &str) -> (f32, HashMap<String, f32>, HashMap<String, f32>) {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(bytes) else {
        return empty_layout();
    };
    let split = v.get("split_ratio").and_then(|x| x.as_f64()).unwrap_or(0.5) as f32;
    let mut weights = HashMap::new();
    if let Some(obj) = v.get("weights").and_then(|w| w.as_object()) {
        for (k, val) in obj {
            if let Some(f) = val.as_f64() {
                weights.insert(k.clone(), f as f32);
            }
        }
    }
    let mut row_weights = HashMap::new();
    if let Some(obj) = v.get("row_weights").and_then(|w| w.as_object()) {
        for (k, val) in obj {
            if let Some(f) = val.as_f64() {
                row_weights.insert(k.clone(), f as f32);
            }
        }
    }
    (split.clamp(0.2, 0.8), weights, row_weights)
}

/// Pure encode: render `(split_ratio, weights, row_weights)` as pretty JSON text.
fn serialize_layout_json(
    split_ratio: f32,
    weights: &HashMap<String, f32>,
    row_weights: &HashMap<String, f32>,
) -> String {
    let mut wmap = serde_json::Map::new();
    for (k, v) in weights {
        wmap.insert(k.clone(), serde_json::json!(*v));
    }
    let mut rmap = serde_json::Map::new();
    for (k, v) in row_weights {
        rmap.insert(k.clone(), serde_json::json!(*v));
    }
    let v = serde_json::json!({
        "split_ratio": split_ratio,
        "weights": wmap,
        "row_weights": rmap,
    });
    serde_json::to_string_pretty(&v).unwrap_or_default()
}

pub(super) fn load_layout_file() -> (f32, HashMap<String, f32>, HashMap<String, f32>) {
    let Ok(bytes) = std::fs::read_to_string(layout_file_path()) else {
        return empty_layout();
    };
    parse_layout_json(&bytes)
}

pub(super) fn save_layout_file(
    split_ratio: f32,
    weights: &HashMap<String, f32>,
    row_weights: &HashMap<String, f32>,
) {
    let s = serialize_layout_json(split_ratio, weights, row_weights);
    if s.is_empty() {
        return;
    }
    let path = layout_file_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(path, s);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_full_round_trips_through_serialize() {
        let mut weights = HashMap::new();
        weights.insert("cadence".to_string(), 0.7_f32);
        weights.insert("lab".to_string(), 0.3_f32);
        let mut row_weights = HashMap::new();
        row_weights.insert("main".to_string(), 1.5_f32);

        let text = serialize_layout_json(0.42, &weights, &row_weights);
        let (split, w, rw) = parse_layout_json(&text);

        assert!((split - 0.42).abs() < 1e-6);
        assert_eq!(w.len(), 2);
        assert!((w["cadence"] - 0.7).abs() < 1e-6);
        assert!((w["lab"] - 0.3).abs() < 1e-6);
        assert_eq!(rw.len(), 1);
        assert!((rw["main"] - 1.5).abs() < 1e-6);
    }

    #[test]
    fn parse_malformed_json_falls_back_to_defaults() {
        let (split, w, rw) = parse_layout_json("{ this is not json ");
        assert!((split - 0.5).abs() < 1e-6);
        assert!(w.is_empty());
        assert!(rw.is_empty());
    }

    #[test]
    fn parse_non_object_json_falls_back_to_defaults() {
        // Valid JSON but not an object (array / scalar) → defaults.
        let (split, w, rw) = parse_layout_json("[1, 2, 3]");
        assert!((split - 0.5).abs() < 1e-6);
        assert!(w.is_empty());
        assert!(rw.is_empty());
    }

    #[test]
    fn parse_missing_fields_uses_defaults_per_field() {
        // Object present but empty → default split, empty maps.
        let (split, w, rw) = parse_layout_json("{}");
        assert!((split - 0.5).abs() < 1e-6);
        assert!(w.is_empty());
        assert!(rw.is_empty());

        // Only split present.
        let (split2, w2, rw2) = parse_layout_json(r#"{"split_ratio": 0.6}"#);
        assert!((split2 - 0.6).abs() < 1e-6);
        assert!(w2.is_empty());
        assert!(rw2.is_empty());
    }

    #[test]
    fn parse_clamps_split_ratio() {
        // Above range → clamped to 0.8.
        let (hi, _, _) = parse_layout_json(r#"{"split_ratio": 0.99}"#);
        assert!((hi - 0.8).abs() < 1e-6);
        // Below range → clamped to 0.2.
        let (lo, _, _) = parse_layout_json(r#"{"split_ratio": 0.05}"#);
        assert!((lo - 0.2).abs() < 1e-6);
    }

    #[test]
    fn parse_ignores_non_numeric_weight_values() {
        // Non-f64 weight entries are silently skipped, numeric ones kept.
        let text = r#"{"weights": {"a": 0.4, "b": "oops", "c": null}}"#;
        let (_, w, _) = parse_layout_json(text);
        assert_eq!(w.len(), 1);
        assert!((w["a"] - 0.4).abs() < 1e-6);
    }
}
