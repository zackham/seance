//! Persisted split-ratio / pane-weight layout file (`~/.local/share/seance/layout.json`).

use std::path::PathBuf;

pub(super) fn layout_file_path() -> PathBuf {
    PathBuf::from(shellexpand::tilde("~/.local/share/seance/layout.json").into_owned())
}

pub(super) fn load_layout_file() -> (
    f32,
    std::collections::HashMap<String, f32>,
    std::collections::HashMap<String, f32>,
) {
    let empty = || {
        (
            0.5,
            std::collections::HashMap::new(),
            std::collections::HashMap::new(),
        )
    };
    let Ok(bytes) = std::fs::read_to_string(layout_file_path()) else {
        return empty();
    };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&bytes) else {
        return empty();
    };
    let split = v.get("split_ratio").and_then(|x| x.as_f64()).unwrap_or(0.5) as f32;
    let mut weights = std::collections::HashMap::new();
    if let Some(obj) = v.get("weights").and_then(|w| w.as_object()) {
        for (k, val) in obj {
            if let Some(f) = val.as_f64() {
                weights.insert(k.clone(), f as f32);
            }
        }
    }
    let mut row_weights = std::collections::HashMap::new();
    if let Some(obj) = v.get("row_weights").and_then(|w| w.as_object()) {
        for (k, val) in obj {
            if let Some(f) = val.as_f64() {
                row_weights.insert(k.clone(), f as f32);
            }
        }
    }
    (split.clamp(0.2, 0.8), weights, row_weights)
}

pub(super) fn save_layout_file(
    split_ratio: f32,
    weights: &std::collections::HashMap<String, f32>,
    row_weights: &std::collections::HashMap<String, f32>,
) {
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
    if let Ok(s) = serde_json::to_string_pretty(&v) {
        let path = layout_file_path();
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::write(path, s);
    }
}
