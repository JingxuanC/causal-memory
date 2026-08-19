//! Server tests — MCP parameter-schema parsing only.
//!
//! The orchestration-logic tests moved to the library with the facade
//! (`causal_memory::memory::tests`); what remains here covers the rmcp
//! parameter structs that still live in this crate.

use super::tools::*;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_intervention_params_task_tag_parsing() {
        let p: InterventionQueryParams =
            serde_json::from_str(r#"{"action":"use redis mutex","task_tag":"caching"}"#).unwrap();
        assert_eq!(p.action, "use redis mutex");
        assert_eq!(p.task_tag.as_deref(), Some("caching"));
        assert_eq!(p.max_depth, None);
        // Optional and absent by default.
        let p: InterventionQueryParams =
            serde_json::from_str(r#"{"action":"use redis mutex"}"#).unwrap();
        assert_eq!(p.task_tag, None);
    }

    #[test]
    fn test_counterfactual_params_parsing() {
        let p: CounterfactualParams = serde_json::from_str(
            r#"{"decision":"use mutex","alternative":"use channel","task_tag":"concurrency","limit":3}"#,
        )
        .unwrap();
        assert_eq!(p.decision, "use mutex");
        assert_eq!(p.alternative, "use channel");
        assert_eq!(p.task_tag.as_deref(), Some("concurrency"));
        assert_eq!(p.limit, Some(3));
        let p: CounterfactualParams =
            serde_json::from_str(r#"{"decision":"a","alternative":"b"}"#).unwrap();
        assert_eq!(p.task_tag, None);
        assert_eq!(p.limit, None);
    }

    #[test]
    fn test_reconstruct_params_parsing() {
        let p: ReconstructLessonParams =
            serde_json::from_str(r#"{"query":"redis","max_edges":10,"calibrate":3}"#).unwrap();
        assert_eq!(p.query, "redis");
        assert_eq!(p.max_edges, Some(10));
        assert_eq!(p.calibrate, Some(3));
        let p: ReconstructLessonParams = serde_json::from_str(r#"{"query":"redis"}"#).unwrap();
        assert_eq!(p.max_edges, None);
        assert_eq!(p.calibrate, None);
    }
}
