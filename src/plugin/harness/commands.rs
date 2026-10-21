//! Bounded native command descriptors; discovery never grants execution authority.
use serde_json::{json, Value};

pub fn arguments(args: &Value) -> Result<&str, crate::core::error::Error> {
    match args.get("arguments") {
        None => Ok(""),
        Some(Value::String(input)) if input.len() <= 128 * 1024 => Ok(input),
        _ => Err(crate::core::error::Error::invalid()),
    }
}

pub fn normalize(value: &Value) -> Value {
    let mut names = std::collections::BTreeSet::new();
    let commands = value.as_array().into_iter().flatten().take(256).filter_map(|entry| {
        let name = entry.as_str().or(entry["name"].as_str())?.trim_start_matches('/');
        if !valid_name(name) || !names.insert(name.to_owned()) { return None; }
        Some(json!({"name":name,"description":entry["description"].as_str().unwrap_or("").chars().take(2048).collect::<String>(),
            "argumentHint":entry["argumentHint"].as_str().unwrap_or("").chars().take(256).collect::<String>(),
            "kind":if entry["kind"] == "action" {"action"} else {"prompt"}}))
    }).collect::<Vec<_>>();
    json!(commands)
}
/// SDK aliases are executable names, not extra model prompts or UI actions.
pub fn with_aliases(value: &Value) -> Value {
    let mut entries = Vec::new();
    for entry in value.as_array().into_iter().flatten().take(256) {
        entries.push(entry.clone());
        for alias in entry["aliases"].as_array().into_iter().flatten().take(32) {
            if let Some(name) = alias.as_str().filter(|name| valid_name(name)) {
                let mut row = entry.clone();
                row["name"] = json!(name);
                entries.push(row);
            }
        }
    }
    normalize(&json!(entries))
}
pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && name
            .chars()
            .all(|c| c.is_alphanumeric() || "_:-.".contains(c))
}
