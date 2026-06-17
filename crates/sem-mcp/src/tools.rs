use serde::Deserialize;

// ── Tool parameter structs ──

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EntitiesParams {
    #[schemars(description = "Optional path to a file or directory. If omitted, defaults to '.'.")]
    pub path: Option<String>,
}

impl EntitiesParams {
    pub fn path(&self) -> Option<&str> {
        self.path.as_deref().filter(|p| !p.is_empty())
    }
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DiffParams {
    #[schemars(
        description = "Base ref to compare from (branch, tag, or commit hash, e.g. 'main'). If omitted, shows working-tree changes (like `sem diff`)."
    )]
    pub base_ref: Option<String>,
    #[schemars(description = "Target ref to compare to. Defaults to HEAD.")]
    pub target_ref: Option<String>,
    #[schemars(description = "Optional: diff only this file")]
    pub file_path: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BlameParams {
    #[schemars(description = "Path to the file (relative to repo root or absolute)")]
    pub file_path: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ImpactAnalysisParams {
    #[schemars(description = "Path to the file containing the entity")]
    pub file_path: String,
    #[schemars(description = "Name of the entity to analyze impact for")]
    pub entity_name: String,
    #[schemars(
        description = "Analysis mode: 'all' (default, shows deps + dependents + transitive impact + tests), 'deps' (direct dependencies only), 'dependents' (direct dependents only), 'tests' (affected test entities only)"
    )]
    pub mode: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct XrefParams {
    #[schemars(description = "Name of the entity to analyze, optionally as \"type name\"")]
    pub entity_name: String,
    #[schemars(
        description = "Repos to span, each as \"tag=absolute_path\" (e.g. \"api=/abs/parkableapi\", \"web=/abs/parkable-web-next\"). Provide the API repo plus any client repos."
    )]
    pub repos: Vec<String>,
    #[schemars(description = "File the entity lives in (disambiguates if multiple matches)")]
    pub file: Option<String>,
    #[schemars(description = "How far to walk in-client dependents for affected UI. Default 2.")]
    pub depth: Option<usize>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LogParams {
    #[schemars(description = "Name of the entity to trace history for")]
    pub entity_name: String,
    #[schemars(description = "Path to the file containing the entity. If omitted, auto-detects.")]
    pub file_path: Option<String>,
    #[schemars(description = "Maximum number of commits to analyze. Defaults to 50.")]
    pub limit: Option<usize>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ContextParams {
    #[schemars(description = "Path to the file containing the entity")]
    pub file_path: String,
    #[schemars(description = "Name of the target entity")]
    pub entity_name: String,
    #[schemars(description = "Maximum token budget. Defaults to 8000.")]
    pub token_budget: Option<usize>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use rmcp::handler::server::tool::parse_json_object;
    use rmcp::model::ErrorCode;
    use serde::de::DeserializeOwned;
    use std::fmt::Debug;

    fn assert_unknown_fields_return_invalid_params<T>()
    where
        T: Debug + DeserializeOwned,
    {
        let arguments = serde_json::json!({
            "deliberate_bogus_field": 1
        })
        .as_object()
        .unwrap()
        .clone();
        let err = parse_json_object::<T>(arguments).unwrap_err();

        assert_eq!(err.code, ErrorCode::INVALID_PARAMS);
        assert!(
            err.message.contains("unknown field"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn entities_params_accepts_path() {
        let params: EntitiesParams =
            serde_json::from_value(serde_json::json!({ "path": "src/lib.rs" })).unwrap();

        assert_eq!(params.path(), Some("src/lib.rs"));
    }

    #[test]
    fn entities_params_allows_missing_path() {
        let params: EntitiesParams = serde_json::from_value(serde_json::json!({})).unwrap();

        assert_eq!(params.path(), None);
    }

    #[test]
    fn all_tool_params_return_invalid_params_for_unknown_fields() {
        assert_unknown_fields_return_invalid_params::<EntitiesParams>();
        assert_unknown_fields_return_invalid_params::<DiffParams>();
        assert_unknown_fields_return_invalid_params::<BlameParams>();
        assert_unknown_fields_return_invalid_params::<ImpactAnalysisParams>();
        assert_unknown_fields_return_invalid_params::<LogParams>();
        assert_unknown_fields_return_invalid_params::<ContextParams>();
    }
}
