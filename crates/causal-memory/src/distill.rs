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
  - fact: stable personal details (job, relationships, possessions, tech stack)
  - preference: likes/dislikes/habits (tools, workflows, conventions)
  - lesson: decisions made, advice given, feedback received
  - event: dated happenings (plans, purchases, activities, todo changes)
  - causal: a decision→outcome relationship — "doing X caused/enabled/prevented Y"
- "causal_relation": ONLY for kind="causal". One of "caused" | "enabled" | "prevented".
  - caused: the decision directly led to the outcome (e.g. "deploying without tests caused a production crash")
  - enabled: the decision made the outcome possible (e.g. "adding caching enabled faster response times")
  - prevented: the decision blocked something bad from happening (e.g. "adding input validation prevented SQL injection", "setting up health checks prevented cascading failures")
- "decision": ONLY for kind="causal". The action/decision text (the "cause"). The "text" field holds the outcome (the "effect").
- "text": one self-contained, absolutely-dated sentence
- "date": YYYY-MM-DD (usually the session date)
- "supersedes": CRITICAL for forgetting accuracy. Fill this whenever the user CHANGES, CANCELS, or COMPLETES something previously stated. Put 2-5 keywords of the OLD content. Examples: user says "I now prefer tea" (previously liked coffee) → supersedes: "likes coffee". User says "I cancelled my gym membership" → supersedes: "gym membership". User says "the budget is now $1M" (was $1.2M) → supersedes: "budget 1200000". A missed supersedes means the old fact stays live and pollutes future answers.

# CRITICAL: Causal Item Extraction

Pay special attention to causal relationships. When someone describes:
- A decision AND its consequence → extract as "causal"
- A fix that stops something bad → extract as "prevented" (NOT "caused")
- An enabler that makes something possible → extract as "enabled" (NOT "caused")

**Prevented vs Caused — do not confuse them!**
- "Added retry logic, and the timeouts stopped" → PREVENTED (retry logic prevented timeouts)
- "Skipped tests, and a bug reached production" → CAUSED (skipping tests caused the bug)
- "Added CI gate, and untested code can no longer merge" → PREVENTED (CI gate prevented untested merges)
- "Switched to async, and throughput doubled" → ENABLED (async enabled higher throughput)

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

Input: "I tried deploying without running tests and it caused a production crash. The rollback took 2 hours. Then I added a CI gate so untested code can't merge anymore. After that, I set up canary deployments to catch regressions early — since then, no bad release has reached all users."
Output: [
  {"kind": "causal", "causal_relation": "caused", "decision": "deployed without running tests", "text": "Production crash on DATE, requiring a 2-hour rollback", "date": "DATE", "supersedes": null},
  {"kind": "causal", "causal_relation": "prevented", "decision": "added a CI gate that blocks merges without tests", "text": "Untested code can no longer reach production after DATE", "date": "DATE", "supersedes": null},
  {"kind": "causal", "causal_relation": "prevented", "decision": "set up canary deployments to catch regressions early", "text": "No bad release reached all users after DATE", "date": "DATE", "supersedes": null},
  {"kind": "lesson", "text": "Always run tests before deploying (learned from crash on DATE)", "date": "DATE", "supersedes": null}
]

Input: "I switched from polling to webhooks for the notification system. After that, the API response time dropped from 2s to 200ms because we weren't constantly checking for updates anymore. But I also had to add rate limiting — without it, a burst of webhook calls would have overwhelmed the server."
Output: [
  {"kind": "causal", "causal_relation": "enabled", "decision": "switched from polling to webhooks for notifications", "text": "API response time dropped from 2s to 200ms after DATE", "date": "DATE", "supersedes": null},
  {"kind": "causal", "causal_relation": "prevented", "decision": "added rate limiting for webhook calls", "text": "Burst of webhook calls did not overwhelm the server after DATE", "date": "DATE", "supersedes": null},
  {"kind": "lesson", "text": "Webhooks are faster than polling for notification systems (implemented on DATE)", "date": "DATE", "supersedes": null}
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

# Anti-Repetition (HARD RULE)
- Each item must describe a DIFFERENT fact/event/lesson. NEVER emit the same or near-duplicate item twice — v4-flash has an observed degeneration mode where a handful of items repeat until max_tokens; the parser dedups exact repeats, but do not rely on it.
- Output AT MOST 20 items. If you have more candidates, keep only the 20 most valuable.

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
    #[serde(default, alias = "type")]
    kind: String,
    #[serde(default, alias = "description", alias = "content")]
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

    /// The configured model name (for benchmark report headers).
    pub fn model(&self) -> &str {
        &self.config.model
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

        // Up to 3 attempts with exponential backoff (2s → 4s): distill runs
        // over thousands of sessions at high concurrency, and rate-limit
        // (429) bursts need real backoff, not a single immediate retry (a
        // burst otherwise fails EVERY session of a question — and with
        // log-and-continue recording, the question then looks "successfully
        // empty"). Uses the `backoff` crate (previously a hand-rolled loop);
        // randomization disabled so retry timing stays deterministic.
        let backoff_cfg = backoff::ExponentialBackoff {
            initial_interval: std::time::Duration::from_secs(2),
            multiplier: 2.0,
            randomization_factor: 0.0,
            max_elapsed_time: Some(std::time::Duration::from_secs(10)),
            ..Default::default()
        };
        match backoff::future::retry(backoff_cfg, || async {
            llm::chat_with_timeout(
                &self.config,
                DISTILL_PROMPT,
                &user_msg,
                Self::DISTILL_MAX_TOKENS,
                0.0,
                // Distilling a full session takes 30-120s on current DeepSeek
                // (chat → v4-flash); the 8s MCP-path default aborts mid-body
                // ("error decoding response body"). Floor at 120s.
                llm::http_timeout().max(std::time::Duration::from_secs(120)),
            )
            .await
            .map_err(backoff::Error::transient)
        })
        .await
        {
            Ok(raw) => Ok(Self::parse_items(&raw, date)),
            // `backoff::future::retry` unwraps the transient error — the final
            // failure IS the underlying anyhow error.
            Err(e) => Err(e),
        }
    }

    /// Max tokens for the distill reply: up to 30 detailed items per
    /// session (numbers, names, per-day events, meeting notes, emails,
    /// proposals) need substantial headroom. 8000: deepseek-v4-flash writes
    /// longer per item than the v3-era model the 4000 budget was tuned for —
    /// a truncated JSON tail costs the items after the cut.
    const DISTILL_MAX_TOKENS: u32 = 8000;

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

        // Exact-duplicate guard: deepseek-v4-flash has an observed
        // degeneration mode where the same items repeat until max_tokens.
        // Dedup on normalized text so a degenerate run cannot flood the
        // store (write-side idempotency is the second net, this is the
        // first).
        let mut seen = std::collections::HashSet::new();
        raw_items
            .into_iter()
            .filter_map(|item| normalize_item(item, fallback_date))
            .filter(|item| seen.insert(item.text.trim().to_lowercase()))
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
    let causal_relation = item
        .causal_relation
        .as_deref()
        .and_then(CausalRelation::parse);
    // If the LLM provided a causal_relation, force kind=Causal even if it
    // used a different field name (type/description) or didn't set kind.
    let kind = if causal_relation.is_some() {
        ItemKind::Causal
    } else {
        ItemKind::parse(&item.kind)
    };
    Some(MemoryItem {
        kind,
        text,
        date,
        supersedes,
        causal_relation,
        decision: item.decision.filter(|s| !s.is_empty()),
    })
}

/// Strict YYYY-MM-DD check (chrono rejects 2025-13-40 etc.).
fn valid_ymd(s: &str) -> bool {
    chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d").is_ok()
}

// ─── Recurrence-triggered distillation (RecMem, arXiv:2605.16045) ─────────
//
// Eager distill (every session → LLM) is the token whale of the write path.
// RecMem shows only recurrence matters: a fact is worth distilling when the
// same topic surfaces AGAIN — single-topic sessions that never repeat carry
// nothing the store does not already know. The gate:
//
//   session end → log turns to session_logs (WITH the session embedding)
//               → recurrence check: does this topic match a prior session?
//                   YES → distill (this + matched session merged)
//                   NO  → leave pending in session_logs
//               → batch mode drains pending sessions that later become
//                 recurrent (or fall back to eager for embedding-less ones)

/// Outcome of one recurrence-gated distill decision.
#[derive(Debug, Clone, PartialEq)]
pub struct RecurrenceOutcome {
    /// The session that was checked.
    pub session_id: i64,
    /// True when the session's topic repeated a prior session and was distilled.
    pub distilled: bool,
    /// The prior session whose topic matched (recurrence), if any.
    pub matched_session: Option<i64>,
    /// Cosine similarity of the best match, if any.
    pub similarity: Option<f32>,
    /// Items written by the distillation (empty when skipped).
    pub items: Vec<MemoryItem>,
}

/// Aggregate of one batch drain run.
#[derive(Debug, Default)]
pub struct BatchOutcome {
    /// Sessions distilled because their topic recurred.
    pub distilled: Vec<RecurrenceOutcome>,
    /// Sessions that had no embedding and fell back to eager distill.
    pub eager_fallback: Vec<i64>,
    /// Sessions still pending (no recurrence, no embedding).
    pub still_pending: Vec<i64>,
}

/// Pure recurrence decision — unit-testable without a store or network.
/// Returns the best (prior_session_id, cosine) at or above `min_similarity`,
/// if any.
pub fn should_distill(
    current_embedding: &[f32],
    prior_sessions: &[(i64, Vec<f32>)],
    min_similarity: f32,
) -> Option<(i64, f32)> {
    prior_sessions
        .iter()
        .filter_map(|(sid, emb)| {
            let sim = crate::embed::cosine_similarity(current_embedding, emb) as f32;
            (sim >= min_similarity).then_some((*sid, sim))
        })
        .max_by(|a, b| a.1.total_cmp(&b.1))
}

/// Record distilled items through the same write path the eager CLI uses:
/// facts/preferences → `record_fact`, lessons/events/causal → `record_distilled`.
/// Returns the number of items written.
pub fn record_items(
    store: &crate::store::CausalStore,
    items: &[MemoryItem],
    task_tag: Option<&str>,
) -> Result<usize> {
    let mut written = 0;
    for item in items {
        match item.kind {
            ItemKind::Fact | ItemKind::Preference => {
                store.record_fact(
                    match item.kind {
                        ItemKind::Fact => "fact",
                        ItemKind::Preference => "preference",
                        _ => "fact",
                    },
                    &item.text,
                    "user",
                    "distill",
                    0.8,
                )?;
            }
            ItemKind::Lesson | ItemKind::Event | ItemKind::Causal => {
                store.record_distilled(item, task_tag)?;
            }
        }
        written += 1;
    }
    Ok(written)
}

/// Parse the session date of a stored session (its earliest turn) into
/// "YYYY-MM-DD", falling back to `fallback`.
fn session_date_str(store: &crate::store::CausalStore, session_id: i64, fallback: &str) -> String {
    store
        .session_date(session_id)
        .ok()
        .flatten()
        .and_then(|ts| chrono::DateTime::from_timestamp(ts, 0))
        .map(|dt| dt.format("%Y-%m-%d").to_string())
        .filter(|s| valid_ymd(s))
        .unwrap_or_else(|| fallback.to_string())
}

/// Recurrence-gated distill of ONE new session (the CLI-facing entry):
///
/// 1. Log the raw turns into `session_logs` — the session embedding rides on
///    the first turn (turn_index 0), which `sessions_with_embeddings` reads.
/// 2. Recurrence check against prior DISTILLED sessions. A match at or above
///    `min_similarity` triggers distill of this session merged with the
///    matched one (the LLM sees the earlier context and can confirm or
///    transition); both sessions are marked distilled.
/// 3. No match → the session stays pending in `session_logs`; `items` is
///    empty and `distilled` is false. The daily batch drains it later.
///
/// `date` is "YYYY-MM-DD"; `session_embedding` is the embedding of the whole
/// session (concatenated turns), `None` when no embedder is configured —
/// the session then skips the recurrence gate and distills eagerly (the old
/// behavior, so nothing is lost when embeddings are unavailable).
pub async fn distill_recurrence(
    store: &crate::store::CausalStore,
    distiller: &Distiller,
    session_id: i64,
    turns: &[(String, String)],
    date: &str,
    session_embedding: Option<&[f32]>,
    min_similarity: f32,
) -> Result<RecurrenceOutcome> {
    let event_time = chrono::NaiveDate::parse_from_str(date, "%Y-%m-%d")
        .ok()
        .and_then(|d| d.and_hms_opt(0, 0, 0))
        .map(|dt| dt.and_utc().timestamp())
        .unwrap_or_else(|| chrono::Utc::now().timestamp());
    for (i, (speaker, text)) in turns.iter().enumerate() {
        store.log_session_turn(
            &format!("{session_id}:{i}"),
            session_id,
            i as i64,
            speaker,
            text,
            event_time,
            None,
            if i == 0 { session_embedding } else { None },
        )?;
    }
    distill_recurrence_inner(
        store,
        distiller,
        session_id,
        date,
        session_embedding,
        min_similarity,
    )
    .await
}

/// Core of `distill_recurrence`, assuming the turns are already in
/// `session_logs` (batch mode). Exposed for the batch drain; the public
/// `distill_recurrence` = logging + this.
async fn distill_recurrence_inner(
    store: &crate::store::CausalStore,
    distiller: &Distiller,
    session_id: i64,
    date: &str,
    session_embedding: Option<&[f32]>,
    min_similarity: f32,
) -> Result<RecurrenceOutcome> {
    // No embedding → no gate possible; eager distill (safe fallback).
    let Some(embedding) = session_embedding else {
        let turns = store.session_turns(session_id)?;
        let items = distiller.distill_session(date, &turns).await?;
        record_items(store, &items, None)?;
        store.mark_session_distilled(session_id, None)?;
        return Ok(RecurrenceOutcome {
            session_id,
            distilled: true,
            matched_session: None,
            similarity: None,
            items,
        });
    };

    let prior = store.sessions_with_embeddings(10_000)?;
    let matched = should_distill(embedding, &prior, min_similarity);

    let Some((matched_session, sim)) = matched else {
        // Recurrence gate closed: leave pending for the next batch run.
        return Ok(RecurrenceOutcome {
            session_id,
            distilled: false,
            matched_session: None,
            similarity: None,
            items: Vec::new(),
        });
    };

    // Recurrence fired: distill this session merged with the matched one, so
    // the LLM sees the earlier context and can detect transitions instead of
    // re-extracting duplicates.
    let mut merged = store.session_turns(matched_session)?;
    merged.extend(store.session_turns(session_id)?);
    let items = distiller.distill_session(date, &merged).await?;
    record_items(store, &items, None)?;
    store.mark_session_distilled(session_id, None)?;
    store.mark_session_distilled(matched_session, None)?;
    Ok(RecurrenceOutcome {
        session_id,
        distilled: true,
        matched_session: Some(matched_session),
        similarity: Some(sim),
        items,
    })
}

/// Daily batch drain: distill every pending session that NOW repeats a prior
/// distilled session (RecMem's ~0.13N sweet spot), and eagerly distill
/// embedding-less pending sessions so nothing is lost.
pub async fn distill_undistilled_batch(
    store: &crate::store::CausalStore,
    distiller: &Distiller,
    date: &str,
    limit: usize,
    min_similarity: f32,
) -> Result<BatchOutcome> {
    let mut out = BatchOutcome::default();
    for session_id in store.undistilled_session_ids(limit)? {
        let stored = store.session_embedding(session_id)?;
        match stored {
            Some(emb) => {
                let date = session_date_str(store, session_id, date);
                let outcome = distill_recurrence_inner(
                    store,
                    distiller,
                    session_id,
                    &date,
                    Some(&emb),
                    min_similarity,
                )
                .await?;
                if outcome.distilled {
                    out.distilled.push(outcome);
                } else {
                    out.still_pending.push(session_id);
                }
            }
            None => {
                // No embedding ever stored → eager distill (old behavior).
                let date = session_date_str(store, session_id, date);
                let turns = store.session_turns(session_id)?;
                let items = distiller.distill_session(&date, &turns).await?;
                record_items(store, &items, None)?;
                store.mark_session_distilled(session_id, None)?;
                out.eager_fallback.push(session_id);
            }
        }
    }
    Ok(out)
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
    fn parse_dedups_exact_repeats() {
        // deepseek-v4-flash degeneration mode: the same items repeat until
        // max_tokens. parse_items must collapse exact (normalized) repeats.
        let raw = r#"[
            {"kind": "event", "text": "2025-06-03: The user pushed commit abc123.", "date": "2025-06-03"},
            {"kind": "event", "text": "2025-06-03: The user pushed commit abc123.", "date": "2025-06-03"},
            {"kind": "event", "text": "  2025-06-03: The user pushed commit abc123. ", "date": "2025-06-03"},
            {"kind": "lesson", "text": "2025-06-03: Always run tests first.", "date": "2025-06-03"}
        ]"#;
        let items = Distiller::parse_items(raw, "2025-06-03");
        assert_eq!(
            items.len(),
            2,
            "exact repeats collapse, distinct items stay"
        );
        assert_eq!(items[0].kind, ItemKind::Event);
        assert_eq!(items[1].kind, ItemKind::Lesson);
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

// ─── v8: recurrence-triggered distill (P1) ────────────────────────────

#[test]
fn test_should_distill_recurrence() {
    let a = vec![1.0f32, 0.0, 0.0];
    let prior = vec![
        (1, vec![0.5f32, 0.5, 0.0]), // sim 0.707 — below a 0.9 gate
        (2, vec![1.0f32, 0.0, 0.0]), // sim 1.0 — exact topic repeat
    ];
    // Above threshold → picks the BEST match.
    assert_eq!(should_distill(&a, &prior, 0.9), Some((2, 1.0)));
    // Lower threshold still picks the best.
    assert_eq!(should_distill(&a, &prior, 0.6), Some((2, 1.0)));
    // Strict gate rejects the 0.707 match.
    assert_eq!(should_distill(&a, &prior, 0.99), Some((2, 1.0)));
    assert_eq!(should_distill(&a, &prior, 1.01), None);
    // Unrelated topic (orthogonal to every candidate) → no recurrence.
    let b = vec![0.0f32, 0.0, 1.0];
    assert_eq!(should_distill(&b, &prior, 0.5), None);
    // Empty candidate set → never fires.
    assert_eq!(should_distill(&a, &[], 0.1), None);
}

#[test]
fn test_record_items_splits_facts_and_edges() {
    use crate::store::CausalStore;
    let store = CausalStore::open_in_memory().unwrap();
    let items = vec![
        MemoryItem {
            kind: ItemKind::Fact,
            text: "user uses Arch Linux".to_string(),
            date: Some("2026-08-01".to_string()),
            supersedes: None,
            causal_relation: None,
            decision: None,
        },
        MemoryItem {
            kind: ItemKind::Lesson,
            text: "always pin dependency versions".to_string(),
            date: Some("2026-08-01".to_string()),
            supersedes: None,
            causal_relation: None,
            decision: None,
        },
        MemoryItem {
            kind: ItemKind::Causal,
            text: "production crash".to_string(),
            date: Some("2026-08-01".to_string()),
            supersedes: None,
            causal_relation: Some(CausalRelation::Caused),
            decision: Some("deployed without tests".to_string()),
        },
    ];
    let n = record_items(&store, &items, None).unwrap();
    assert_eq!(n, 3);
    // Fact → agent_facts; lesson (self-edge) + causal (proper edge) → causal_edges.
    let facts = store.search_facts_bm25("Arch", None, 10).unwrap().len();
    assert_eq!(facts, 1);
    assert_eq!(store.count_edges().unwrap(), 2);
    // The causal item formed a real decision → outcome edge.
    let causal = store
        .search_causal_bm25(None, "deployed without tests", 10)
        .unwrap();
    assert_eq!(causal.len(), 1);
    assert_eq!(causal[0].relation, "caused");
}
