//! Session controls advertised by ACP v1 agents.
use serde_json::Value;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SettingKind {
    Config,
    Mode,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Setting {
    pub id: String,
    pub name: String,
    pub current: String,
    pub choices: Vec<(String, String)>,
    pub kind: SettingKind,
}

pub fn session_settings(result: &Value) -> Vec<Setting> {
    fn choices(values: &[Value], out: &mut Vec<(String, String)>) {
        for value in values {
            if let Some(group) = value.get("options").and_then(Value::as_array) {
                choices(group, out);
            } else if let Some(id) = value.get("value").and_then(Value::as_str) {
                out.push((
                    id.to_owned(),
                    value
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or(id)
                        .to_owned(),
                ));
            }
        }
    }
    if let Some(options) = result.get("configOptions").and_then(Value::as_array) {
        return options
            .iter()
            .filter_map(|option| {
                if option.get("type").and_then(Value::as_str) != Some("select") {
                    return None;
                }
                let id = option
                    .get("id")
                    .or_else(|| option.get("configId"))?
                    .as_str()?
                    .to_owned();
                let mut values = Vec::new();
                choices(option.get("options")?.as_array()?, &mut values);
                Some(Setting {
                    name: option
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or(&id)
                        .to_owned(),
                    id,
                    current: option.get("currentValue")?.as_str()?.to_owned(),
                    choices: values,
                    kind: SettingKind::Config,
                })
            })
            .collect();
    }
    let Some(modes) = result.get("modes") else {
        return Vec::new();
    };
    let choices = modes
        .get("availableModes")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|mode| {
            let id = mode.get("id")?.as_str()?;
            Some((
                id.to_owned(),
                mode.get("name")
                    .and_then(Value::as_str)
                    .unwrap_or(id)
                    .to_owned(),
            ))
        })
        .collect();
    vec![Setting {
        id: "mode".into(),
        name: "Mode".into(),
        current: modes
            .get("currentModeId")
            .and_then(Value::as_str)
            .unwrap_or("")
            .into(),
        choices,
        kind: SettingKind::Mode,
    }]
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn config_options_override_legacy_modes_and_preserve_group_order() {
        let options = session_settings(&json!({"modes":{"currentModeId":"old"},"configOptions":[
            {"id":"model","name":"Model","type":"select","currentValue":"a","options":[{"group":"fast","options":[{"value":"a","name":"A"}]},{"value":"b","name":"B"}]},
            {"id":"future","type":"unknown","currentValue":false}
        ]}));
        assert_eq!(options.len(), 1);
        assert_eq!(
            options[0].choices,
            vec![("a".into(), "A".into()), ("b".into(), "B".into())]
        );
        assert_eq!(options[0].kind, SettingKind::Config);
        assert!(session_settings(&json!({})).is_empty());
        assert_eq!(session_settings(&json!({"modes":{"currentModeId":"ask","availableModes":[{"id":"ask","name":"Ask"}]}}))[0].kind,SettingKind::Mode);
    }
}
