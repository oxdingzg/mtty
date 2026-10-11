//! Compatibility readers for the application's saved state (including what
//! the former miaotty builds wrote).

use serde_json::{json, Value};
use std::path::Path;

/// Prefer canonical state, then the former native host, then the former
/// eframe host. Legacy files remain intact; subsequent saves use canonical paths.
pub fn load(config: &Path, data: Option<&Path>) -> Option<Value> {
    let canonical = config.join("session.json");
    if canonical.exists() {
        return read(&canonical);
    }
    let native = config.join("native-session.json");
    if native.exists() {
        return read(&native);
    }
    read(&data?.join("session.json"))
}

fn read(path: &Path) -> Option<Value> {
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

/// Convert indexed eframe pane/layout references to native pane IDs. Native
/// sessions and recipes pass through unchanged.
pub fn normalize(mut value: Value) -> Option<Value> {
    let tabs = value.get_mut("tabs")?.as_array_mut()?;
    for tab in tabs {
        recover_miao_host_title(tab);
        let indexed = tab.get("active").is_some_and(Value::is_u64);
        if !indexed {
            continue;
        }
        let panes = tab.get_mut("panes")?.as_array_mut()?;
        let ids: Vec<String> = (0..panes.len())
            .map(|i| format!("legacy-pane-{i}"))
            .collect();
        for (pane, id) in panes.iter_mut().zip(&ids) {
            pane["id"] = json!(id);
        }
        let active = tab.get("active")?.as_u64()? as usize;
        tab["active"] = json!(ids.get(active).or_else(|| ids.first())?);
        tab["layout"] = convert_layout(tab.get("layout")?, &ids)?;
    }
    if value.get("active_tab").is_none() {
        value["active_tab"] = value.get("active").cloned().unwrap_or(json!(0));
    }
    if value.get("recent").is_none() {
        value["recent"] = value.get("recent_files").cloned().unwrap_or(json!([]));
    }
    Some(value)
}

/// Older hosted snapshots saved only the screen. Recover the active pane's
/// known miao OSC title from the displayed tab label without treating a user
/// rename or a view-template label as program metadata.
fn recover_miao_host_title(tab: &mut Value) {
    if tab.get("title_set").and_then(Value::as_bool) != Some(false) {
        return;
    }
    let Some(title) = tab
        .get("shown")
        .and_then(Value::as_str)
        .filter(|s| {
            s.strip_prefix("miao | ")
                .is_some_and(|name| !name.trim().is_empty())
        })
        .map(str::to_owned)
    else {
        return;
    };
    let Some(active) = tab.get("active").and_then(Value::as_str).map(str::to_owned) else {
        return;
    };
    let Some(host) = tab
        .get_mut("panes")
        .and_then(Value::as_array_mut)
        .and_then(|panes| {
            panes
                .iter_mut()
                .find(|pane| pane.get("id").and_then(Value::as_str) == Some(active.as_str()))
        })
        .and_then(|pane| pane.get_mut("host"))
        .and_then(Value::as_object_mut)
    else {
        return;
    };
    // An explicit null belongs to the new schema and must not be resurrected.
    host.entry("title").or_insert(json!(title));
}

fn convert_layout(value: &Value, ids: &[String]) -> Option<Value> {
    if let Some(index) = value.get("Leaf").and_then(Value::as_u64) {
        return Some(json!({"leaf": ids.get(index as usize)?}));
    }
    let split = value.get("Split")?;
    Some(json!({
        "dir": if split.get("down")?.as_bool()? { "down" } else { "right" },
        "ratio": split.get("ratio")?,
        "a": convert_layout(split.get("a")?, ids)?,
        "b": convert_layout(split.get("b")?, ids)?,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recovers_legacy_miao_title_only_for_the_active_hosted_pane() {
        let value = json!({"tabs": [{"active": "a", "title_set": false,
        "shown": "miao | saved session", "panes": [
            {"id": "a", "host": {"id": "a"}},
            {"id": "b", "host": {"id": "b"}}
        ]}]});
        let restored = normalize(value.clone()).unwrap();
        assert_eq!(
            restored["tabs"][0]["panes"][0]["host"]["title"],
            "miao | saved session"
        );
        assert!(restored["tabs"][0]["panes"][1]["host"]
            .get("title")
            .is_none());
        for title in [Value::Null, json!("miao | fresh title"), json!("")] {
            let mut newer = value.clone();
            newer["tabs"][0]["panes"][0]["host"]["title"] = title.clone();
            assert_eq!(
                normalize(newer).unwrap()["tabs"][0]["panes"][0]["host"]["title"],
                title
            );
        }
        for shown in ["project alias", "miao | ", "shell 1"] {
            let mut unrelated = value.clone();
            unrelated["tabs"][0]["shown"] = json!(shown);
            assert!(normalize(unrelated).unwrap()["tabs"][0]["panes"][0]["host"]
                .get("title")
                .is_none());
        }
        let mut renamed = value;
        renamed["tabs"][0]["title_set"] = json!(true);
        assert!(normalize(renamed).unwrap()["tabs"][0]["panes"][0]["host"]
            .get("title")
            .is_none());
    }

    #[test]
    fn migrates_indexed_splits_focus_and_tab_decorations() {
        let old = json!({"active": 1, "recent_files": ["note.txt"], "tabs": [{
            "title": "work", "prefix": "dev", "mark": "★", "group": "project",
            "panes": [{"cwd": "/tmp"}, {"cwd": "/"}, {"cwd": null}], "active": 2,
            "layout": {"Split": {"down": false, "ratio": 0.3, "a": {"Leaf": 0},
                "b": {"Split": {"down": true, "ratio": 0.7, "a": {"Leaf": 1}, "b": {"Leaf": 2}}}}}
        }]});
        let converted = normalize(old).unwrap();
        let tab = &converted["tabs"][0];
        assert_eq!(converted["active_tab"], 1);
        assert_eq!(converted["recent"], json!(["note.txt"]));
        assert_eq!(tab["active"], "legacy-pane-2");
        assert_eq!(tab["layout"]["a"]["leaf"], "legacy-pane-0");
        assert_eq!(tab["layout"]["b"]["dir"], "down");
        assert_eq!(tab["layout"]["b"]["b"]["leaf"], "legacy-pane-2");
        assert_eq!(tab["panes"][1]["cwd"], "/");
        assert_eq!(tab["group"], "project");
        assert_eq!(tab["mark"], "★");
    }

    #[test]
    fn canonical_wins_and_legacy_state_is_never_deleted() {
        let root = std::env::temp_dir().join(format!("mtty-migration-{}", std::process::id()));
        let config = root.join("config");
        let data = root.join("data");
        std::fs::create_dir_all(&config).unwrap();
        std::fs::create_dir_all(&data).unwrap();
        for (path, source) in [
            (data.join("session.json"), "eframe"),
            (config.join("native-session.json"), "native"),
        ] {
            std::fs::write(path, json!({"source": source}).to_string()).unwrap();
        }
        assert_eq!(load(&config, Some(&data)).unwrap()["source"], "native");
        std::fs::write(
            config.join("session.json"),
            json!({"source": "canonical"}).to_string(),
        )
        .unwrap();
        assert_eq!(load(&config, Some(&data)).unwrap()["source"], "canonical");
        assert!(config.join("native-session.json").exists());
        assert!(data.join("session.json").exists());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn native_session_survives_normalization_and_invalid_indices_are_rejected() {
        let native = json!({"active_tab": 0, "recent": [], "tabs": [{"active": "a", "panes": [{"id":"a"}], "layout": {"leaf":"a"}}]});
        assert_eq!(normalize(native.clone()), Some(native));
        let bad = json!({"tabs": [{"active":0, "panes":[{}], "layout":{"Leaf":4}}]});
        assert!(normalize(bad).is_none());
    }
}
