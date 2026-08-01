//! Model catalog helpers + effort/sandbox mapping for Grok Build.
//!
//! Models are discovered live from an ACP `initialize` handshake
//! (`_meta.modelState`). This module only maps reasoning / sandbox wire values
//! and turns raw modelState into [`Model`] rows.

use comet_proto::{Model, ReasoningLevel, SandboxLevel};
use serde_json::Value;

/// Grok Build accepts exactly Low / Medium / High on the CLI
/// (`--reasoning-effort low|medium|high`).
pub(crate) const REASONING_LEVELS: &[ReasoningLevel] = &[
    ReasoningLevel::Low,
    ReasoningLevel::Medium,
    ReasoningLevel::High,
];

/// Map a Comet reasoning level to the CLI `--reasoning-effort` value.
/// Unlisted levels clamp to the nearest supported effort.
pub(crate) fn to_effort(reasoning: Option<ReasoningLevel>) -> Option<&'static str> {
    Some(match reasoning? {
        ReasoningLevel::Minimal | ReasoningLevel::Low => "low",
        ReasoningLevel::Medium => "medium",
        ReasoningLevel::High
        | ReasoningLevel::XHigh
        | ReasoningLevel::Max
        | ReasoningLevel::Ultra
        | ReasoningLevel::Ultracode
        | ReasoningLevel::Ultrathink => "high",
    })
}

/// Map a Comet sandbox level into the child's `GROK_SANDBOX` env value.
pub(crate) fn sandbox_env(sandbox: SandboxLevel) -> &'static str {
    match sandbox {
        SandboxLevel::ReadOnly => "read-only",
        SandboxLevel::WorkspaceWrite => "workspace",
        SandboxLevel::DangerFullAccess => "off",
    }
}

/// Parse `_meta.modelState` (or an equivalent object with `currentModelId` /
/// `availableModels`) into the catalog rows the picker expects.
///
/// Returns `(current_model_id, models)`. When `availableModels` is empty but
/// a current id is present, synthesizes a single row for that id so the UI
/// still has something to select.
pub(crate) fn models_from_model_state(model_state: &Value) -> (Option<String>, Vec<Model>) {
    let current = model_state
        .get("currentModelId")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_owned);

    let mut models: Vec<Model> = model_state
        .get("availableModels")
        .and_then(Value::as_array)
        .map(|a| a.as_slice())
        .unwrap_or_default()
        .iter()
        .filter_map(model_from_entry)
        .collect();

    if models.is_empty()
        && let Some(id) = &current
    {
        models.push(Model {
            id: id.clone(),
            label: id.clone(),
            description: None,
            reasoning_levels: REASONING_LEVELS.to_vec(),
            options: Vec::new(),
        });
    }

    // Prefer listing the current model first when present.
    if let Some(cur) = &current
        && let Some(pos) = models.iter().position(|m| &m.id == cur)
    {
        models.swap(0, pos);
    }

    (current, models)
}

fn model_from_entry(entry: &Value) -> Option<Model> {
    let id = entry
        .get("modelId")
        .or_else(|| entry.get("id"))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())?
        .to_owned();
    let label = entry
        .get("name")
        .or_else(|| entry.get("label"))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .unwrap_or(&id)
        .to_owned();
    let description = entry
        .get("description")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_owned);

    let reasoning_levels = reasoning_levels_from_entry(entry);

    Some(Model {
        id,
        label,
        description,
        reasoning_levels,
        options: Vec::new(),
    })
}

fn reasoning_levels_from_entry(entry: &Value) -> Vec<ReasoningLevel> {
    let meta = entry.get("_meta");
    let efforts = meta
        .and_then(|m| m.get("reasoningEfforts"))
        .and_then(Value::as_array)
        .map(|a| a.as_slice())
        .unwrap_or_default();

    let mut levels: Vec<ReasoningLevel> = efforts
        .iter()
        .filter_map(|e| {
            let id = e
                .get("value")
                .or_else(|| e.get("id"))
                .and_then(Value::as_str)?;
            parse_effort(id)
        })
        .collect();

    if levels.is_empty() {
        // supportsReasoningEffort true (or unknown) → full ladder; false → none.
        let supports = meta
            .and_then(|m| m.get("supportsReasoningEffort"))
            .and_then(Value::as_bool)
            .unwrap_or(true);
        if supports {
            return REASONING_LEVELS.to_vec();
        }
        return Vec::new();
    }

    // Stable Low → Medium → High order, de-duped.
    let order = [
        ReasoningLevel::Low,
        ReasoningLevel::Medium,
        ReasoningLevel::High,
    ];
    levels = order.into_iter().filter(|l| levels.contains(l)).collect();
    levels
}

fn parse_effort(s: &str) -> Option<ReasoningLevel> {
    match s.to_ascii_lowercase().as_str() {
        "low" => Some(ReasoningLevel::Low),
        "medium" => Some(ReasoningLevel::Medium),
        "high" => Some(ReasoningLevel::High),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn effort_maps_and_clamps() {
        assert_eq!(to_effort(None), None);
        assert_eq!(to_effort(Some(ReasoningLevel::Low)), Some("low"));
        assert_eq!(to_effort(Some(ReasoningLevel::Medium)), Some("medium"));
        assert_eq!(to_effort(Some(ReasoningLevel::High)), Some("high"));
        assert_eq!(to_effort(Some(ReasoningLevel::Minimal)), Some("low"));
        assert_eq!(to_effort(Some(ReasoningLevel::XHigh)), Some("high"));
        assert_eq!(to_effort(Some(ReasoningLevel::Ultra)), Some("high"));
    }

    #[test]
    fn sandbox_env_values() {
        assert_eq!(sandbox_env(SandboxLevel::ReadOnly), "read-only");
        assert_eq!(sandbox_env(SandboxLevel::WorkspaceWrite), "workspace");
        assert_eq!(sandbox_env(SandboxLevel::DangerFullAccess), "off");
    }

    #[test]
    fn model_state_parses_live_shape() {
        let state = json!({
            "currentModelId": "grok-4.5",
            "availableModels": [{
                "modelId": "grok-4.5",
                "name": "Grok 4.5",
                "description": "SpaceXAI's new frontier model",
                "_meta": {
                    "supportsReasoningEffort": true,
                    "reasoningEffort": "high",
                    "reasoningEfforts": [
                        {"id": "high", "value": "high", "label": "High Effort", "default": true},
                        {"id": "medium", "value": "medium", "label": "Medium Effort"},
                        {"id": "low", "value": "low", "label": "Low Effort"}
                    ]
                }
            }]
        });
        let (current, models) = models_from_model_state(&state);
        assert_eq!(current.as_deref(), Some("grok-4.5"));
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id, "grok-4.5");
        assert_eq!(models[0].label, "Grok 4.5");
        assert_eq!(
            models[0].reasoning_levels,
            vec![
                ReasoningLevel::Low,
                ReasoningLevel::Medium,
                ReasoningLevel::High
            ]
        );
    }

    #[test]
    fn model_state_synthesizes_current_when_catalog_empty() {
        let state = json!({ "currentModelId": "grok-only" });
        let (current, models) = models_from_model_state(&state);
        assert_eq!(current.as_deref(), Some("grok-only"));
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id, "grok-only");
    }
}
