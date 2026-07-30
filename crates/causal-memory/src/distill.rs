//! LLM distillation ingest: conversation turns → long-term memory items.
//!
//! The raw-ingest path (every turn becomes a chunk) floods the store with
//! chit-chat, so BM25 retrieval surfaces noise and the answer model has to
//! find the fact inside raw dialogue. All three benches (LoCoMo /
//! LongMemEval / Memora) showed the same memory-presence weakness. This
//! module distils each session into a few self-contained, absolutely-dated
//! memory items (fact / preference / lesson / event) before they hit the
//! store, and lets the LLM mark which earlier item a new item supersedes —
//! the write-time half of the "forgetting" dimension.
//!
//! LLM access reuses `crate::llm` (OpenAI-compatible chat, `LlmConfig`,
//! DEEPSEEK_API_KEY fallback). `parse_items` is pure and heavily unit
//! tested; nothing in the test suite touches the network.

use anyhow::Result;
use serde::Deserialize;

use crate::llm::{self, LlmConfig};

/// What a distilled item IS. Used for confidence shaping in the store and
/// for provenance in the chunk id; all kinds share one retrieval path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ItemKind {
    Fact,
    Preference,
    Lesson,
    Event,
}

impl ItemKind {
    fn parse(s: &str) -> Self {
        match s.trim().to_lowercase().as_str() {
            "preference" => Self::Preference,
            "lesson" => Self::Lesson,
            "event" => Self::Event,
            // Unknown / missing kinds default to Fact (the least surprising).
            _ => Self::Fact,
        }
    }
}

/// One distilled memory item. `text` is a single self-contained sentence
/// carrying an absolute date; `supersedes` names the earlier content this
/// item updates or retracts (keyword-style, used for text matching at
/// write time).
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct MemoryItem {
    pub kind: ItemKind,
    pub text: String,
    pub date: Option<String>,
    pub supersedes: Option<String>,
}

/// Why the prompt looks like this:
/// - "worth long-term memory" filter: Memora weekly has ~500 chit-chat
///   sessions per persona; distilling them verbatim would reproduce the
///   raw-ingest noise floor, so greetings/small talk are dropped explicitly.
/// - Absolute-date rule: raw turns only carry the session date in the chunk
///   prefix, so "yesterday"/"next week" in turn text are unresolvable later;
///   requiring absolute dates makes every item self-contained (the same
///   fix the LoCoMo answer prompt needed on the read side).
/// - supersedes: Memora's forgetting_absence metric penalizes answers that
///   still mention deleted/outdated items. Asking the distiller to name the
///   prior content an update retracts gives `record_distilled` a mechanical
///   handle for soft-invalidation at write time.
/// - JSON array output, temp=0: deterministic, machine-parseable; the
///   parser below tolerates fences and partial garbage anyway.
const DISTILL_PROMPT: &str = r#"You are distilling a conversation session into long-term memory items for a personal assistant.

Extract ONLY content worth remembering long-term:
- fact: stable facts about the user (job, projects, possessions, relationships)
- preference: likes/dislikes/habits (tools, food, style, workflow)
- lesson: advice given, decisions made and why, feedback to remember
- event: dated happenings, plans, deadlines, purchases, todo changes

DISCARD greetings, small talk, filler, and content with no future value.

Rules for EVERY item:
- "text": ONE self-contained sentence. It MUST include the absolute date (YYYY-MM-DD, from the session date or dates mentioned in the conversation). NEVER use relative time words ("yesterday", "next week", "recently") — resolve them to absolute dates.
- "kind": one of "fact" | "preference" | "lesson" | "event".
- "date": the item's date as YYYY-MM-DD (usually the session date).
- "supersedes": if this item UPDATES or RETRACTS information from an earlier conversation (e.g. a todo changed from "buy groceries" to "bought groceries", a preference was revised, an item was deleted), fill in 2-5 SPECIFIC keywords of the OLD content it replaces so it can be matched by text search. Otherwise null. Only fill supersedes for true RETRACTIONS (cancelled/completed/changed-away-from), NOT for restating the same fact in new words.

Preserve specifics — they are what makes a memory useful later:
- Keep concrete numbers, names, dates, prices, and list items verbatim (e.g. "walked 6,214 steps", "Lars Bergström to optimize the rendering pipeline").
- For content the user created or discussed in detail (meeting notes, emails, proposals), emit ONE item PER distinct fact (agenda item, decision, action item) instead of one vague summary.
- For recurring activities (steps, meals, expenses, workouts), emit one event item per day with that day's numbers.
- Up to 10 items per session; prioritize by long-term value.

Respond with ONLY a JSON array, nothing else:
[{"kind": "...", "text": "...", "date": "YYYY-MM-DD", "supersedes": null}]

Return an empty array [] if nothing is worth remembering."#;

/// LLM-backed session distiller. Construct via `from_env`; absent config
/// yields None and the caller falls back to raw ingest.
pub struct Distiller {
    config: LlmConfig,
}

/// Raw wire shape of one item; strings are validated/normalized into
/// `MemoryItem` by `parse_items`.
#[derive(Debug, Deserialize)]
struct RawItem {
    #[serde(default)]
    kind: String,
    text: String,
    #[serde(default)]
    date: Option<serde_json::Value>,
    #[serde(default)]
    supersedes: Option<serde_json::Value>,
}

impl Distiller {
    /// Load LLM config from env. Returns None when no API key is configured
    /// (caller falls back to raw ingest). Mirrors `LlmConfig::from_env`'s
    /// pattern, except the API base defaults to DeepSeek when the key came
    /// from DEEPSEEK_API_KEY — the same default the bench harnesses use.
    pub fn from_env() -> Option<Self> {
        if let Some(config) = LlmConfig::from_env() {
            return Some(Self { config });
        }
        let api_key = std::env::var("DEEPSEEK_API_KEY").ok()?;
        Some(Self {
            config: LlmConfig {
                api_base: "https://api.deepseek.com/v1".into(),
                api_key,
                model: "deepseek-chat".into(),
            },
        })
    }

    /// Distill one session's turns into memory items.
    ///
    /// `date` is the session date ("YYYY-MM-DD"); `turns` are
    /// (speaker, message) pairs in order. One retry on failure; a persistent
    /// error is returned so the caller can fall back to raw ingest for this
    /// session. An empty Ok(vec![]) means "nothing worth remembering" AND
    /// parse failure alike — the caller treats both as fallback-to-raw
    /// (data must not be lost).
    pub async fn distill_session(
        &self,
        date: &str,
        turns: &[(String, String)],
    ) -> Result<Vec<MemoryItem>> {
        let mut user_msg = format!("Session date: {date}\n\nConversation:\n");
        for (speaker, message) in turns {
            user_msg.push_str(&format!("{}: {}\n", speaker, message.trim()));
        }

        // Up to 3 attempts with 2s/4s backoff: distill runs over thousands
        // of sessions at high concurrency, and rate-limit (429) bursts need
        // real backoff, not a single immediate retry (a burst otherwise
        // fails EVERY session of a question — and with log-and-continue
        // recording, the question then looks "successfully empty").
        let mut last_err = anyhow::anyhow!("no attempt made");
        for attempt in 0..3 {
            match llm::chat(
                &self.config,
                DISTILL_PROMPT,
                &user_msg,
                Self::DISTILL_MAX_TOKENS,
                0.0,
            )
            .await
            {
                Ok(raw) => return Ok(Self::parse_items(&raw, date)),
                Err(e) => {
                    last_err = e;
                    if attempt < 2 {
                        tokio::time::sleep(std::time::Duration::from_secs(2 << attempt)).await;
                    }
                }
            }
        }
        Err(last_err)
    }

    /// Max tokens for the distill reply: up to 10 detailed items per
    /// session (numbers, names, per-day events) need headroom.
    const DISTILL_MAX_TOKENS: u32 = 1500;

    /// Parse the LLM reply into memory items. Pure function — the test
    /// surface for this module.
    ///
    /// Tolerance ladder:
    /// 1. strip markdown code fences (LLMs wrap JSON despite instructions);
    /// 2. try the whole payload as a JSON array (or an object wrapping one
    ///    under "items"/"memories"/"memory", or a single object);
    /// 3. on total failure, salvage balanced `{...}` objects one by one —
    ///    a truncated or malformed tail must not kill the items before it.
    ///
    /// Items with empty text are dropped. A missing/invalid `date` falls
    /// back to `fallback_date` (the session date); an invalid fallback
    /// leaves `date = None` and the store stamps the current time.
    pub fn parse_items(raw: &str, fallback_date: &str) -> Vec<MemoryItem> {
        let text = strip_fence(raw);
        let raw_items: Vec<RawItem> = serde_json::from_str::<Vec<RawItem>>(text)
            .or_else(|_| {
                serde_json::from_str::<serde_json::Value>(text).and_then(|v| match v {
                    serde_json::Value::Object(_) => {
                        for key in ["items", "memories", "memory"] {
                            if let Some(arr) = v.get(key) {
                                if let Ok(items) =
                                    serde_json::from_value::<Vec<RawItem>>(arr.clone())
                                {
                                    return Ok(items);
                                }
                            }
                        }
                        // Single bare object.
                        serde_json::from_value::<RawItem>(v).map(|item| vec![item])
                    }
                    other => serde_json::from_value::<Vec<RawItem>>(other),
                })
            })
            .unwrap_or_else(|_| salvage_objects(text));

        raw_items
            .into_iter()
            .filter_map(|item| normalize_item(item, fallback_date))
            .collect()
    }
}

/// Strip one layer of markdown code fence, if present.
fn strip_fence(raw: &str) -> &str {
    let t = raw.trim();
    if let Some(body) = t.strip_prefix("```") {
        let body = body.strip_prefix("json").unwrap_or(body).trim_start();
        body.strip_suffix("```").unwrap_or(body).trim()
    } else {
        t
    }
}

/// Best-effort salvage: scan for top-level balanced `{...}` spans and parse
/// each as a RawItem, skipping ones that fail. Brace counting is string-
/// aware so braces inside JSON strings don't unbalance the scan.
fn salvage_objects(text: &str) -> Vec<RawItem> {
    let mut out = Vec::new();
    let bytes = text.as_bytes();
    let mut start = None;
    let mut depth = 0i32;
    let mut in_string = false;
    let mut escaped = false;
    for (i, &b) in bytes.iter().enumerate() {
        if in_string {
            if escaped {
                escaped = false;
            } else if b == b'\\' {
                escaped = true;
            } else if b == b'"' {
                in_string = false;
            }
            continue;
        }
        match b {
            b'"' => in_string = true,
            b'{' => {
                if depth == 0 {
                    start = Some(i);
                }
                depth += 1;
            }
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    if let Some(s) = start.take() {
                        if let Ok(item) = serde_json::from_str::<RawItem>(&text[s..=i]) {
                            out.push(item);
                        }
                    }
                }
                if depth < 0 {
                    depth = 0;
                }
            }
            _ => {}
        }
    }
    out
}

/// Validate one raw item into a `MemoryItem`; None = drop (empty text).
fn normalize_item(item: RawItem, fallback_date: &str) -> Option<MemoryItem> {
    let text = item.text.trim().to_string();
    if text.is_empty() {
        return None;
    }
    let date_str = |v: Option<serde_json::Value>| -> Option<String> {
        v.and_then(|v| v.as_str().map(str::to_string))
            .map(|s| s.trim().to_string())
            .filter(|s| valid_ymd(s))
    };
    let date = date_str(item.date).or_else(|| {
        let fb = fallback_date.trim();
        valid_ymd(fb).then(|| fb.to_string())
    });
    let supersedes = item
        .supersedes
        .and_then(|v| v.as_str().map(str::to_string))
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty() && s != "null");
    Some(MemoryItem {
        kind: ItemKind::parse(&item.kind),
        text,
        date,
        supersedes,
    })
}

/// Strict YYYY-MM-DD check (chrono rejects 2025-13-40 etc.).
fn valid_ymd(s: &str) -> bool {
    chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d").is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_clean_json_array() {
        let raw = r#"[
            {"kind": "preference", "text": "2025-06-03: The user prefers Vim keybindings.", "date": "2025-06-03", "supersedes": null},
            {"kind": "event", "text": "2025-06-03: The user added Buy groceries to their todo list.", "date": "2025-06-03"}
        ]"#;
        let items = Distiller::parse_items(raw, "2025-06-03");
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].kind, ItemKind::Preference);
        assert_eq!(items[0].date.as_deref(), Some("2025-06-03"));
        assert_eq!(items[0].supersedes, None);
        assert_eq!(items[1].kind, ItemKind::Event);
    }

    #[test]
    fn parse_strips_markdown_fence() {
        let raw = "```json\n[{\"kind\": \"fact\", \"text\": \"2025-06-03: The user works as a software engineer.\", \"date\": \"2025-06-03\", \"supersedes\": null}]\n```";
        let items = Distiller::parse_items(raw, "2025-06-03");
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].kind, ItemKind::Fact);
    }

    #[test]
    fn parse_bad_tail_salvages_good_items() {
        // Truncated array: the complete first object must survive.
        let raw = r#"[{"kind": "fact", "text": "2025-06-03: The user runs Arch Linux.", "date": "2025-06-03"}, {"kind": "pref"#;
        let items = Distiller::parse_items(raw, "2025-06-03");
        assert_eq!(items.len(), 1);
        assert!(items[0].text.contains("Arch Linux"));
    }

    #[test]
    fn parse_skips_objects_with_empty_text() {
        let raw = r#"[{"kind": "fact", "text": "", "date": "2025-06-03"}, {"kind": "fact", "text": "2025-06-03: kept.", "date": "2025-06-03"}]"#;
        let items = Distiller::parse_items(raw, "2025-06-03");
        assert_eq!(items.len(), 1);
        assert!(items[0].text.contains("kept"));
    }

    #[test]
    fn parse_date_fallback_and_invalid_dates() {
        // Missing date -> session date.
        let raw = r#"[{"kind": "fact", "text": "no date here"}]"#;
        let items = Distiller::parse_items(raw, "2025-06-03");
        assert_eq!(items[0].date.as_deref(), Some("2025-06-03"));

        // Invalid date -> session date.
        let raw = r#"[{"kind": "fact", "text": "bad date", "date": "June 3rd"}]"#;
        let items = Distiller::parse_items(raw, "2025-06-03");
        assert_eq!(items[0].date.as_deref(), Some("2025-06-03"));

        // Invalid date AND invalid fallback -> None (store stamps now()).
        let items = Distiller::parse_items(raw, "garbage");
        assert_eq!(items[0].date, None);
    }

    #[test]
    fn parse_supersedes_extraction() {
        let raw = r#"[{"kind": "event", "text": "2025-06-05: The user bought groceries.", "date": "2025-06-05", "supersedes": "Buy groceries todo"}]"#;
        let items = Distiller::parse_items(raw, "2025-06-05");
        assert_eq!(items[0].supersedes.as_deref(), Some("Buy groceries todo"));

        // "null" string and empty string count as absent.
        let raw = r#"[{"kind": "fact", "text": "x", "supersedes": "null"}, {"kind": "fact", "text": "y", "supersedes": "  "}]"#;
        let items = Distiller::parse_items(raw, "2025-06-05");
        assert_eq!(items[0].supersedes, None);
        assert_eq!(items[1].supersedes, None);
    }

    #[test]
    fn parse_wrapped_and_single_object() {
        // Object wrapping the array.
        let raw = r#"{"memories": [{"kind": "lesson", "text": "2025-06-03: Always pin dependency versions.", "date": "2025-06-03"}]}"#;
        let items = Distiller::parse_items(raw, "2025-06-03");
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].kind, ItemKind::Lesson);

        // Single bare object.
        let raw = r#"{"kind": "event", "text": "2025-06-03: one-off", "date": "2025-06-03"}"#;
        let items = Distiller::parse_items(raw, "2025-06-03");
        assert_eq!(items.len(), 1);
    }

    #[test]
    fn parse_unknown_kind_defaults_to_fact() {
        let raw = r#"[{"kind": "hobby", "text": "x"}, {"text": "y"}]"#;
        let items = Distiller::parse_items(raw, "2025-06-03");
        assert_eq!(items[0].kind, ItemKind::Fact);
        assert_eq!(items[1].kind, ItemKind::Fact);
    }

    #[test]
    fn parse_garbage_returns_empty() {
        assert!(Distiller::parse_items("not json at all", "2025-06-03").is_empty());
        assert!(Distiller::parse_items("", "2025-06-03").is_empty());
        assert!(Distiller::parse_items("[]", "2025-06-03").is_empty());
    }
}
