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
    /// A causal lesson: "doing X caused/enabled/prevented Y".
    /// The `causal_relation` field on MemoryItem specifies which edge type.
    Causal,
}

impl ItemKind {
    fn parse(s: &str) -> Self {
        match s.trim().to_lowercase().as_str() {
            "preference" => Self::Preference,
            "lesson" => Self::Lesson,
            "event" => Self::Event,
            "causal" => Self::Causal,
            // Unknown / missing kinds default to Fact (the least surprising).
            _ => Self::Fact,
        }
    }
}

/// The causal edge type for Causal items.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CausalRelation {
    /// "Doing X caused Y" — positive activation (+1.0)
    Caused,
    /// "Doing X enabled Y" — mild positive (+0.5)
    Enabled,
    /// "Doing X prevented Y" — NEGATIVE activation (-0.3, GABA)
    Prevented,
}

impl CausalRelation {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Caused => "caused",
            Self::Enabled => "enabled",
            Self::Prevented => "prevented",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        match s.trim().to_lowercase().as_str() {
            "caused" => Some(Self::Caused),
            "enabled" => Some(Self::Enabled),
            "prevented" => Some(Self::Prevented),
            _ => None,
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
    /// For Causal items: the relation type (caused/enabled/prevented).
    /// Ignored for other kinds.
    #[serde(default)]
    pub causal_relation: Option<CausalRelation>,
    /// For Causal items: the decision text (what the user did).
    /// Combined with `text` (the outcome) to form a proper causal edge.
    #[serde(default)]
    pub decision: Option<String>,
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
const DISTILL_PROMPT: &str = r#"You are a Memory Extractor for a personal AI assistant. Your job is to extract EVERY piece of memorable information from a conversation session into structured memory items.

# What to Extract

Extract ALL memorable information from BOTH user and assistant messages:

**From user messages:**
- Personal details: name, job, location, relationships, projects
- Preferences: likes/dislikes (food, tools, movies, music, books, travel)
- Plans and intentions: upcoming events, goals, deadlines, travel
- Activities: workouts, meals, expenses, daily routines (with exact numbers)
- Professional context: role, team, tools, technologies, career goals
- Health and wellness: dietary restrictions, fitness routines
- Opinions, emotional states, motivations
- Shared content: meeting notes, emails, proposals, documents (extract EACH point)

**From assistant messages (only genuinely new info):**
- Specific recommendations given (books, restaurants, products)
- Plans or schedules created
- Solutions provided, instructions given

**Do NOT extract:**
- Greetings, small talk, filler ("Hi!", "Sounds good!", "Thanks!")
- Vague acknowledgments or restatements of what the user said
- Meta-commentary about capabilities

# Critical Extraction Rules

## Rule 1: Extract ALL Dimensions — Not Just the First Topic
A conversation about a meeting may also mention a preference, a deadline, and a personal detail. Extract EACH topic separately. Do NOT let the dominant topic cause you to miss secondary information.

## Rule 2: Preserve Exact Specifics
- Keep concrete numbers verbatim: "walked 6,214 steps", "budget is $15,000", "3 out of 5 items"
- Keep proper nouns verbatim: "Osteria Francescana", not "a restaurant"
- Keep titles verbatim: "The Hitchhiker's Guide to the Galaxy", not "a book"
- Keep dates verbatim: "June 15, 2025", not "a summer date"
- NEVER generalize: "grilled salmon and roasted vegetables", not "a healthy meal"

## Rule 3: Capture Transitions and Changes
When a preference or plan CHANGES, capture BOTH the new state AND what it replaces:
- "switched from almond milk to oat milk" -> NOT just "prefers oat milk"
- "cancelled the gym membership" -> NOT just "uses the gym"
- "rescheduled from Tuesday to Thursday" -> capture the day change

## Rule 4: Extract Incidental Facts from Questions
When a user asks a question, the question itself often contains personal facts:
- "As an aspiring actor, can you recommend..." -> extract "is an aspiring actor"
- "I just started learning Python..." -> extract "started learning Python on [date]"

## Rule 5: Temporal Grounding
- Use absolute dates (YYYY-MM-DD), NEVER relative time ("yesterday", "recently")
- Resolve relative references using the session date
- Keep exact durations: "18 days" stays "18 days", not "some time"

## Rule 6: Contextually Rich Memories
Each item should be a complete, self-contained sentence (15-80 words):
- BAD: "User has a dog"
- GOOD: "User has a golden retriever named Max who they walk every morning"

# Item Format

Each item has:
- "kind": "fact" | "preference" | "lesson" | "event" | "causal"
  - fact: stable personal details (job, relationships, possessions)
  - preference: likes/dislikes/habits (food, tools, entertainment)
  - lesson: decisions made, advice given, feedback received
  - event: dated happenings (plans, purchases, activities, todo changes)
  - causal: a decision→outcome relationship — "doing X caused/enabled/prevented Y"
- "causal_relation": ONLY for kind="causal". One of "caused" | "enabled" | "prevented".
  - caused: the decision directly led to the outcome (e.g. "deploying without tests caused a production crash")
  - enabled: the decision made the outcome possible (e.g. "adding caching enabled faster response times")
  - prevented: the decision blocked something from happening (e.g. "adding input validation prevented SQL injection")
- "decision": ONLY for kind="causal". The action/decision text (the "cause"). The "text" field holds the outcome (the "effect").
- "text": one self-contained, absolutely-dated sentence
- "date": YYYY-MM-DD (usually the session date)
- "supersedes": CRITICAL for forgetting accuracy. Fill this whenever the user CHANGES, CANCELS, or COMPLETES something previously stated. Put 2-5 keywords of the OLD content. Examples: user says "I now prefer tea" (previously liked coffee) → supersedes: "likes coffee". User says "I cancelled my gym membership" → supersedes: "gym membership". User says "the budget is now $1M" (was $1.2M) → supersedes: "budget 1200000". A missed supersedes means the old fact stays live and pollutes future answers.

# Examples

Input: "Hi, I'm Sarah. I work at Google as a senior engineer. Oh, and I just adopted a cat named Luna!"
Output: [
  {"kind": "fact", "text": "User's name is Sarah, works as a senior engineer at Google", "date": "DATE", "supersedes": null},
  {"kind": "event", "text": "User adopted a cat named Luna on DATE", "date": "DATE", "supersedes": null}
]

Input: "I usually take the bus, but I just bought a car yesterday so I'll be driving from now on."
Output: [
  {"kind": "event", "text": "User bought a car on DATE and switched from taking the bus to driving", "date": "DATE", "supersedes": "takes the bus"}
]

Input: "We had the project review meeting. Three decisions: 1) Launch v2.0 in March, 2) Allocate $50k for marketing, 3) Hire a junior dev by April."
Output: [
  {"kind": "lesson", "text": "On DATE, project review decided to launch v2.0 in March", "date": "DATE", "supersedes": null},
  {"kind": "lesson", "text": "On DATE, project review allocated $50,000 for marketing budget", "date": "DATE", "supersedes": null},
  {"kind": "lesson", "text": "On DATE, project review decided to hire a junior developer by April", "date": "DATE", "supersedes": null}
]

Input: "Hey, how's it going? Nice weather today."
Output: []

Input: "I tried deploying without running tests and it caused a production crash. The rollback took 2 hours. Lesson learned — always run tests before deploying."
Output: [
  {"kind": "causal", "causal_relation": "caused", "decision": "deployed without running tests", "text": "Production crash on DATE, requiring a 2-hour rollback", "date": "DATE", "supersedes": null},
  {"kind": "lesson", "text": "Always run tests before deploying (learned from crash on DATE)", "date": "DATE", "supersedes": null}
]

Input: "I walked 8,231 steps today, had a salad for lunch ($12.50), and finished reading 'Project Hail Mary'. Oh, I also decided to switch from Yoga to Pilates starting next week."
Output: [
  {"kind": "event", "text": "User walked 8,231 steps on DATE", "date": "DATE", "supersedes": null},
  {"kind": "event", "text": "User had a salad for lunch costing $12.50 on DATE", "date": "DATE", "supersedes": null},
  {"kind": "event", "text": "User finished reading 'Project Hail Mary' on DATE", "date": "DATE", "supersedes": null},
  {"kind": "preference", "text": "User decided to switch from Yoga to Pilates starting the week of DATE", "date": "DATE", "supersedes": "does yoga"}
]

# Extraction Checklist (verify before outputting)
1. Have you extracted at least one item from EVERY distinct topic in the conversation?
2. Have you checked messages in the MIDDLE and END, not just the beginning?
3. For a conversation with 5+ messages, you should typically extract 5-15 items.
4. Have you preserved all numbers, names, dates, and prices exactly as stated?
5. Have you captured any transitions or changes with both old and new state?

# Output

Return ONLY a valid JSON array. No text, no explanation:
[{"kind": "fact", "text": "...", "date": "YYYY-MM-DD", "supersedes": null}]

For causal items, include decision and causal_relation:
[{"kind": "causal", "causal_relation": "caused", "decision": "what was done", "text": "what happened as a result", "date": "YYYY-MM-DD", "supersedes": null}]

Return [] if nothing is worth remembering."#;

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
    #[serde(default)]
    causal_relation: Option<String>,
    #[serde(default)]
    decision: Option<String>,
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
    /// (speaker, message) pairs in order. `existing_memories` is an optional
    /// list of already-stored memory texts — when provided, they are passed
    /// into the prompt so the LLM can avoid re-extracting duplicates and
    /// detect transitions ("switched from X to Y") by seeing what's already
    /// stored.
    pub async fn distill_session(
        &self,
        date: &str,
        turns: &[(String, String)],
    ) -> Result<Vec<MemoryItem>> {
        self.distill_session_with_context(date, turns, &[]).await
    }

    /// Distill with dedup context: `existing_memories` are recently-stored
    /// items that the LLM sees to avoid duplicates and detect transitions.
    pub async fn distill_session_with_context(
        &self,
        date: &str,
        turns: &[(String, String)],
        existing_memories: &[String],
    ) -> Result<Vec<MemoryItem>> {
        let mut user_msg = format!("Session date: {date}\n");
        if !existing_memories.is_empty() {
            user_msg.push_str("\nRecently stored memories (do NOT re-extract these — detect transitions instead):\n");
            for mem in existing_memories.iter().take(20) {
                user_msg.push_str(&format!("- {mem}\n"));
            }
        }
        user_msg.push_str("\nConversation:\n");
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

    /// Max tokens for the distill reply: up to 30 detailed items per
    /// session (numbers, names, per-day events, meeting notes, emails,
    /// proposals) need substantial headroom.
    const DISTILL_MAX_TOKENS: u32 = 4000;

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
        causal_relation: item.causal_relation.as_deref().and_then(CausalRelation::parse),
        decision: item.decision.filter(|s| !s.is_empty()),
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
