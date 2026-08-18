//! Query routing (architecture hardening D4): a rule-based, LLM-free
//! classifier that maps a natural-language query to the memory layer most
//! likely to hold the answer. `search_memory` uses it to prefer a single
//! layer (facts vs causal lessons) when the intent is clear, and falls
//! back to RRF fusion when it is not.
//!
//! Rules are deliberately cheap and deterministic: explicit intent words
//! (why/because/root-cause -> causal; what-is/preference/config -> fact),
//! time expressions (fact), directory phrases, chain phrases. No LLM, no
//! embeddings — just token/pattern matching on the query text.

/// Which memory layer a query most likely targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum QueryIntent {
    /// "what is" — agent_facts (preferences, config, tech stack)
    Fact,
    /// "why / what caused" — causal_edges (decision -> outcome lessons)
    Causal,
    /// multi-hop root-cause / effect chains
    Chain,
    /// L0 directory browsing
    Directory,
    /// no clear signal — RRF fusion across all layers (default)
    #[default]
    Unified,
}

impl QueryIntent {
    /// A clear, high-confidence intent (>= 0.8) — routing may prefer the
    /// single layer; anything else keeps the fused fallback.
    pub fn is_confident(self) -> bool {
        matches!(self, QueryIntent::Fact | QueryIntent::Causal | QueryIntent::Chain | QueryIntent::Directory)
    }
}

fn has_any(q: &str, needles: &[&str]) -> bool {
    needles.iter().any(|n| q.contains(n))
}

/// Does the query reference a time (date, relative day, era)?
fn has_time_expression(q: &str) -> bool {
    let lower = q.to_lowercase();
    has_any(
        &lower,
        &[
            "yesterday", "today", "last week", "last month", "last year",
            "前天", "昨天", "上周", "上个月", "去年", "今年", "之前", "以前",
            "when", "什么时候", "何时",
        ],
    ) || lower.chars().any(|c| c.is_ascii_digit())
        && (lower.contains('-') || lower.contains("/") || lower.contains("年"))
}

/// Classify a query into the most likely memory layer.
pub fn classify_query(query: &str) -> QueryIntent {
    let q = query.to_lowercase();

    // Causal signals: why / causation / root-cause / lessons.
    if has_any(
        &q,
        &[
            "为什么", "为何", "因为", "导致", "造成", "引发", "根因", "根因是",
            "why", "because", "caused", "cause", "root cause", "lesson",
            "教训", "失败原因", "什么原因", "how did", "怎么回事",
        ],
    ) {
        return QueryIntent::Causal;
    }

    // Fact signals: what-is / preferences / config / stack.
    if has_any(
        &q,
        &[
            "是什么", "什么是", "what is", "what's", "what are",
            "preference", "偏好", "喜欢什么", "配置", "config", "技术栈",
            "tech stack", "用的什么", "使用什么", "哪个版本", "who is",
            "谁", "哪台", "多少", "how many", "how much",
        ],
    ) {
        return QueryIntent::Fact;
    }

    // Directory browsing.
    if has_any(
        &q,
        &["目录", "recent decisions", "recent lessons", "最近的决定", "decision list", "list of"],
    ) {
        return QueryIntent::Directory;
    }

    // Multi-hop chain signals.
    if has_any(
        &q,
        &["链路", "链条", "链式", "连锁反应", "multi-hop", "chain", "trace", "连锁"],
    ) {
        return QueryIntent::Chain;
    }

    // Time expression -> fact ("when" questions are factual).
    if has_time_expression(&q) {
        return QueryIntent::Fact;
    }

    QueryIntent::Unified
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 50 hand-labeled queries — the acceptance bar is >= 85% accuracy.
    fn cases() -> Vec<(&'static str, QueryIntent)> {
        vec![
            // Causal (12)
            ("为什么上次部署失败了", QueryIntent::Causal),
            ("这个 bug 是怎么导致的", QueryIntent::Causal),
            ("root cause of the cache stampede", QueryIntent::Causal),
            ("what caused the OOM", QueryIntent::Causal),
            ("那次死锁的根因是什么", QueryIntent::Causal),
            ("why did the mutex deadlock", QueryIntent::Causal),
            ("之前 Redis 超时的原因", QueryIntent::Causal),
            ("教训：不要关掉 WAL", QueryIntent::Causal),
            ("what lesson did we learn", QueryIntent::Causal),
            ("构建失败是什么原因", QueryIntent::Causal),
            ("because of the TTL issue", QueryIntent::Causal),
            ("怎么搞坏了生产环境", QueryIntent::Causal),
            // Fact (12)
            ("用户喜欢什么编程语言", QueryIntent::Fact),
            ("项目用的什么技术栈", QueryIntent::Fact),
            ("what is the user's preference", QueryIntent::Fact),
            ("数据库配置是什么", QueryIntent::Fact),
            ("当前用的 Redis 版本", QueryIntent::Fact),
            ("who is responsible for the dashboard", QueryIntent::Fact),
            ("用户偏好什么工具", QueryIntent::Fact),
            ("昨天部署了什么", QueryIntent::Fact),
            ("when was the last release", QueryIntent::Fact),
            ("how many hosts are there", QueryIntent::Fact),
            ("我上周买了什么", QueryIntent::Fact),
            ("配置文件在哪", QueryIntent::Fact),
            // Directory (4)
            ("最近有什么决策", QueryIntent::Directory),
            ("recent decisions list", QueryIntent::Directory),
            ("看一下因果记忆目录", QueryIntent::Directory),
            ("list of recent lessons", QueryIntent::Directory),
            // Chain (4)
            ("完整的事故因果链是什么", QueryIntent::Chain),
            ("trace the failure chain", QueryIntent::Chain),
            ("这次故障的连锁反应链路", QueryIntent::Chain),
            ("multi-hop root cause chain", QueryIntent::Chain),
            // Unified (18) — no strong signal, fusion is right
            ("redis 缓存", QueryIntent::Unified),
            ("deadlock", QueryIntent::Unified),
            ("网络超时", QueryIntent::Unified),
            ("kafka 分区", QueryIntent::Unified),
            ("grafana dashboard", QueryIntent::Unified),
            ("dockerfile 构建", QueryIntent::Unified),
            ("sqlite wal", QueryIntent::Unified),
            ("embedding 模型", QueryIntent::Unified),
            ("mcp 工具", QueryIntent::Unified),
            ("consolidation", QueryIntent::Unified),
            ("q-value", QueryIntent::Unified),
            ("hebbian", QueryIntent::Unified),
            ("bm25", QueryIntent::Unified),
            ("prometheus", QueryIntent::Unified),
            ("opentsdb 查询", QueryIntent::Unified),
            ("skopeo 镜像同步", QueryIntent::Unified),
            ("alpine 镜像", QueryIntent::Unified),
            ("test flake", QueryIntent::Unified),
        ]
    }

    #[test]
    fn test_classifier_accuracy_above_85() {
        let cases = cases();
        let correct = cases
            .iter()
            .filter(|(q, want)| classify_query(q) == *want)
            .count();
        let total = cases.len();
        let acc = correct as f64 / total as f64;
        eprintln!("classifier accuracy: {correct}/{total} = {:.0}%", acc * 100.0);
        assert!(
            acc >= 0.85,
            "accuracy {:.0}% below the 85% acceptance bar",
            acc * 100.0
        );
    }

    #[test]
    fn test_confident_intents() {
        assert!(classify_query("为什么部署失败").is_confident());
        assert!(classify_query("用户偏好什么").is_confident());
        assert!(!classify_query("redis 缓存").is_confident());
    }
}