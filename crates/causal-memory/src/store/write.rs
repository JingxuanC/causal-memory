//! Causal-edge write path: recording decisions, distilled items, and invalidation.

use std::collections::HashSet;
use std::sync::atomic::Ordering;

use anyhow::{anyhow, Result};
use rusqlite::{params, Connection, OptionalExtension};

use super::{
    CausalStore, ID_COUNTER, SUPERSEDES_MIN_SHARED_TOKENS, SUPERSEDES_SIM_THRESHOLD,
    containment_similarity, date_tokens, effective_polarity, is_retraction_record,
};

impl CausalStore {
    /// Record a decision and its outcome, creating the causal edge.
    pub fn record_decision(
        &self,
        decision: &str,
        outcome: &str,
        relation: &str,
        task_tag: Option<&str>,
        confidence: f64,
        discovered_by: &str,
    ) -> Result<String> {
        self.record_decision_at(
            decision,
            outcome,
            relation,
            task_tag,
            confidence,
            discovered_by,
            chrono::Utc::now().timestamp(),
        )
    }

    /// Record with an explicit event_time (for extractors that know the real event time).
    /// discovered_at defaults to now() (DB write time).
    #[allow(clippy::too_many_arguments)]
    pub fn record_decision_at(
        &self,
        decision: &str,
        outcome: &str,
        relation: &str,
        task_tag: Option<&str>,
        confidence: f64,
        discovered_by: &str,
        event_time: i64,
    ) -> Result<String> {
        self.record_decision_full(
            decision,
            outcome,
            relation,
            task_tag,
            confidence,
            discovered_by,
            event_time,
            None,
        )
    }

    /// Record with an explicit event_time and a pre-judged outcome polarity
    /// (v4: positive/negative/mixed/neutral, judged by the LLM or the
    /// heuristic at the caller). `None` stores NULL — read paths then fall
    /// back to the signal-word heuristic.
    #[allow(clippy::too_many_arguments)]
    pub fn record_decision_full(
        &self,
        decision: &str,
        outcome: &str,
        relation: &str,
        task_tag: Option<&str>,
        confidence: f64,
        discovered_by: &str,
        event_time: i64,
        outcome_polarity: Option<&str>,
    ) -> Result<String> {
        let conn = self.conn.lock().map_err(|e| anyhow!("DB lock: {e}"))?;
        let db_time = chrono::Utc::now().timestamp();
        let seq = ID_COUNTER.fetch_add(1, Ordering::Relaxed);
        let dec_id = format!("d{}{}", event_time, seq);
        let out_id = format!("o{}{}", event_time, seq);

        conn.execute(
            "INSERT INTO chunks (id, text, created_at) VALUES (?1, ?2, ?3)",
            params![&dec_id, decision, event_time],
        )?;
        conn.execute(
            "INSERT INTO chunks (id, text, created_at) VALUES (?1, ?2, ?3)",
            params![&out_id, outcome, event_time],
        )?;
        // Contradiction short-circuit (rule-based, no LLM): if the same decision
        // already has valid edges whose outcome is contradicted by the new one,
        // the old lesson is falsified by the new evidence — soft-invalidate it.
        // Must run BEFORE inserting the new edge so the new edge is never matched.
        Self::invalidate_contradicted_edges(&conn, decision, outcome, outcome_polarity, db_time)?;
        conn.execute(
            "INSERT INTO causal_edges (from_id, to_id, relation, confidence, discovered_by, event_time, discovered_at, task_tag, outcome_polarity)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![&dec_id, &out_id, relation, confidence, discovered_by, event_time, db_time, task_tag, outcome_polarity],
        )?;
        Ok(dec_id)
    }

    /// Soft-invalidate valid edges on the same decision text whose outcome
    /// contradicts the new outcome. Returns the number of invalidated edges.
    ///
    /// Conservative rule: only "old edge clearly negative AND new edge clearly
    /// positive" auto-invalidates. A stored polarity (v4) wins over the text
    /// heuristic — 'negative' counts as failure, 'positive' as success, and
    /// 'mixed'/'neutral' never trigger on either side; edges with NULL stored
    /// polarity fall back to the signal-word heuristic on the outcome text.
    fn invalidate_contradicted_edges(
        conn: &Connection,
        decision: &str,
        new_outcome: &str,
        new_polarity: Option<&str>,
        now: i64,
    ) -> Result<usize> {
        let mut stmt = conn.prepare(
            "SELECT ce.id, ct.text, ce.outcome_polarity
             FROM causal_edges ce
             JOIN chunks cf ON cf.id = ce.from_id
             JOIN chunks ct ON ct.id = ce.to_id
             WHERE cf.text = ?1 AND ce.valid_to IS NULL",
        )?;
        let old_edges: Vec<(i64, String, Option<String>)> = stmt
            .query_map(params![decision], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let new_eff = effective_polarity(new_polarity, new_outcome);
        let mut invalidated = 0;
        for (edge_id, old_outcome, old_polarity) in old_edges {
            let old_eff = effective_polarity(old_polarity.as_deref(), &old_outcome);
            if old_eff == Some(false) && new_eff == Some(true) {
                conn.execute(
                    "UPDATE causal_edges SET valid_to = ?1 WHERE id = ?2",
                    params![now, edge_id],
                )?;
                invalidated += 1;
            }
        }
        Ok(invalidated)
    }

    /// Record one distilled memory item (see `crate::distill`).
    ///
    /// Every item becomes ONE chunk whose text carries a `[YYYY-MM-DD]` date
    /// prefix (event_time parsed from `item.date`; current time when the
    /// item has no valid date) plus ONE self-referential `caused` edge —
    /// the edge exists so the item is visible to the edge-based read paths
    /// (`search_causal_bm25` etc.), and it is a self-edge so retrieval
    /// surfaces the item text exactly once (a separate "recorded" outcome
    /// chunk would show up as a second, content-free line).
    ///
    /// Idempotent: an identical distilled chunk text already present is
    /// returned as a duplicate without inserting anything.
    ///
    /// `supersedes`: tokenizes the hint and scores it against the decision
    /// text of every other valid edge in scope (same `task_tag` when given,
    /// event_time not later than the new item's) by containment similarity
    /// |intersection| / min(|a|, |b|) — robust for the keyword-style hints
    /// the distiller emits. Three guards (Memora weekly round-2):
    /// 1. KILL-ALL: EVERY candidate at or above `SUPERSEDES_SIM_THRESHOLD`
    ///    (and sharing ≥ `SUPERSEDES_MIN_SHARED_TOKENS` tokens with the
    ///    hint) is soft-invalidated — an outdated fact scattered over
    ///    several chunks must not survive via the non-best copies.
    /// 2. SAME-FACT EXEMPTION: a candidate mentioning the same absolute
    ///    date (YYYY-MM-DD) as the new item is kept — restating one dated
    ///    fact ("rescheduled to 06-10" → "confirmed 06-10") is not a
    ///    retraction, and killing it wipes whole calendar chains.
    /// 3. NEGATION MEMORY: each invalidated entry spawns a new valid
    ///    `Event` memory "[date] Cancelled/superseded: <old text>" so
    ///    answers can say "this was cancelled" instead of "no such thing".
    pub fn record_distilled(
        &self,
        item: &crate::distill::MemoryItem,
        task_tag: Option<&str>,
    ) -> Result<super::RecordDistilledOutcome> {
        let conn = self.conn.lock().map_err(|e| anyhow!("DB lock: {e}"))?;
        let now = chrono::Utc::now().timestamp();
        let date_str = item.date.clone().unwrap_or_else(|| {
            chrono::DateTime::from_timestamp(now, 0)
                .map(|dt| dt.format("%Y-%m-%d").to_string())
                .unwrap_or_default()
        });
        let event_time = chrono::NaiveDate::parse_from_str(&date_str, "%Y-%m-%d")
            .ok()
            .and_then(|d| d.and_hms_opt(0, 0, 0))
            .map(|dt| dt.and_utc().timestamp())
            .unwrap_or(now);
        let text = format!("[{date_str}] {}", item.text.trim());

        // Idempotency: same distilled text already stored -> return existing.
        let existing: Option<String> = conn
            .query_row(
                "SELECT id FROM chunks WHERE id LIKE 'distill:%' AND text = ?1 LIMIT 1",
                params![&text],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(chunk_id) = existing {
            return Ok(super::RecordDistilledOutcome {
                chunk_id,
                edge_id: None,
                duplicate: true,
                invalidated_edge_ids: Vec::new(),
            });
        }

        let confidence = match item.kind {
            crate::distill::ItemKind::Lesson => 0.7,
            _ => 0.6,
        };
        let (chunk_id, edge_id) =
            Self::insert_distilled_chunk(&conn, &text, event_time, now, confidence, task_tag)?;

        // Effective kill hint: the LLM's `supersedes` field when given,
        // otherwise — when the item text itself announces a retraction
        // ("no longer likes X", "removed X", ...) — the item's own text.
        // The distiller forgets `supersedes` surprisingly often, and every
        // miss leaves the outdated fact valid and retrievable.
        let hint = item
            .supersedes
            .clone()
            .or_else(|| is_retraction_record(&item.text).then(|| item.text.clone()));
        let invalidated_edge_ids = match &hint {
            Some(hint) => {
                let killed = Self::invalidate_superseded(
                    &conn, hint, task_tag, &chunk_id, &text, event_time, now,
                )?;
                // Guard 3 — negation memory: invalidated entries must not
                // silently vanish. Record one retrievable Event memory per
                // killed entry stating it is void, dated like the new item.
                // (Killed entries are never retraction records themselves —
                // those are excluded from candidacy — so this never writes
                // a self-cancelling double negation.)
                for (_, old_text) in &killed {
                    let summary: String = old_text.chars().take(200).collect();
                    let neg_text = format!("[{date_str}] Cancelled/superseded: {summary}");
                    Self::insert_distilled_chunk(&conn, &neg_text, event_time, now, 0.6, task_tag)?;
                }
                killed.into_iter().map(|(edge_id, _)| edge_id).collect()
            }
            None => Vec::new(),
        };

        Ok(super::RecordDistilledOutcome {
            chunk_id,
            edge_id: Some(edge_id),
            duplicate: false,
            invalidated_edge_ids,
        })
    }

    /// Insert one distilled chunk plus its self-referential `caused` edge.
    /// Shared by `record_distilled` (the item itself) and the negation
    /// memories spawned for invalidated entries. Returns (chunk_id, edge_id).
    fn insert_distilled_chunk(
        conn: &Connection,
        text: &str,
        event_time: i64,
        now: i64,
        confidence: f64,
        task_tag: Option<&str>,
    ) -> Result<(String, i64)> {
        let seq = ID_COUNTER.fetch_add(1, Ordering::Relaxed);
        let chunk_id = format!("distill:{event_time}:{seq}");
        conn.execute(
            "INSERT INTO chunks (id, text, created_at) VALUES (?1, ?2, ?3)",
            params![&chunk_id, text, event_time],
        )?;
        conn.execute(
            "INSERT INTO causal_edges
             (from_id, to_id, relation, confidence, discovered_by, event_time, discovered_at, task_tag)
             VALUES (?1, ?1, 'caused', ?2, 'distill', ?3, ?4, ?5)",
            params![&chunk_id, confidence, event_time, now, task_tag],
        )?;
        Ok((chunk_id, conn.last_insert_rowid()))
    }

    /// Find every valid in-scope edge whose decision text matches the
    /// supersedes hint (containment similarity over tokens) at or above
    /// `SUPERSEDES_SIM_THRESHOLD` and soft-invalidate ALL of them. Returns
    /// the (edge id, decision text) pairs actually invalidated — the caller
    /// turns each into a negation memory.
    fn invalidate_superseded(
        conn: &Connection,
        hint: &str,
        task_tag: Option<&str>,
        exclude_chunk_id: &str,
        new_item_text: &str,
        item_event_time: i64,
        now: i64,
    ) -> Result<Vec<(i64, String)>> {
        let hint_tokens: Vec<String> = crate::patterns::tokenize(hint)
            .into_iter()
            .filter(|t| !t.chars().all(|c| c.is_ascii_digit()))
            .collect();
        if hint_tokens.len() < SUPERSEDES_MIN_SHARED_TOKENS {
            return Ok(Vec::new());
        }
        let new_dates = date_tokens(new_item_text);
        let mut sql = String::from(
            "SELECT ce.id, cf.text
             FROM causal_edges ce
             JOIN chunks cf ON cf.id = ce.from_id
             WHERE ce.valid_to IS NULL AND cf.id != ?1 AND ce.event_time <= ?2",
        );
        let mut bind: Vec<Box<dyn rusqlite::ToSql>> = vec![
            Box::new(exclude_chunk_id.to_string()),
            Box::new(item_event_time),
        ];
        if let Some(tag) = task_tag {
            sql.push_str(" AND ce.task_tag = ?");
            bind.push(Box::new(tag.to_string()));
        }
        let mut stmt = conn.prepare(&sql)?;
        let bind_refs: Vec<&dyn rusqlite::ToSql> = bind.iter().map(|b| b.as_ref()).collect();
        let rows = stmt.query_map(bind_refs.as_slice(), |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
        })?;
        let hint_set: HashSet<&String> = hint_tokens.iter().collect();
        let mut killed: Vec<(i64, String)> = Vec::new();
        for row in rows {
            let (edge_id, text) = row.map_err(|e| anyhow!("Query failed: {e}"))?;
            // Retraction records are never kill targets (double negation).
            if is_retraction_record(&text) {
                continue;
            }
            // Same-fact exemption: shared absolute date => restatement.
            if !new_dates.is_empty() && !date_tokens(&text).is_disjoint(&new_dates) {
                continue;
            }
            let cand_tokens = crate::patterns::tokenize(&text);
            let shared = cand_tokens.iter().filter(|t| hint_set.contains(t)).count();
            if shared < SUPERSEDES_MIN_SHARED_TOKENS {
                continue;
            }
            let sim = containment_similarity(&hint_tokens, &cand_tokens);
            if sim >= SUPERSEDES_SIM_THRESHOLD {
                killed.push((edge_id, text));
            }
        }
        for (edge_id, _) in &killed {
            conn.execute(
                "UPDATE causal_edges SET valid_to = ?1 WHERE id = ?2 AND valid_to IS NULL",
                params![now, edge_id],
            )?;
        }
        Ok(killed)
    }

    /// Semantic extension of the contradiction short-circuit: soft-invalidate
    /// valid edges whose decision text DIFFERS from `decision` (same-text
    /// edges are the exact-match path's job) but whose embedding is highly
    /// similar to `query_embedding`, when the old outcome contradicts
    /// `new_outcome`. Pure sync — the caller (MCP/CLI layer) supplies the
    /// embedding, or skips this entirely when embeddings are unavailable.
    /// Returns the number of invalidated edges.
    pub fn invalidate_semantic_contradictions(
        &self,
        decision: &str,
        new_outcome: &str,
        new_polarity: Option<&str>,
        query_embedding: &[f32],
        min_similarity: f64,
    ) -> Result<usize> {
        let conn = self.conn.lock().map_err(|e| anyhow!("DB lock: {e}"))?;
        let now = chrono::Utc::now().timestamp();
        let mut stmt = conn.prepare(
            "SELECT ce.id, ct.text, ce.outcome_polarity, ee.vector
             FROM causal_edges ce
             JOIN chunks cf ON cf.id = ce.from_id
             JOIN chunks ct ON ct.id = ce.to_id
             JOIN edge_embeddings ee ON ee.edge_id = ce.id
             WHERE cf.text != ?1 AND ce.valid_to IS NULL",
        )?;
        let rows = stmt.query_map(params![decision], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, Vec<u8>>(3)?,
            ))
        })?;
        let new_eff = effective_polarity(new_polarity, new_outcome);
        let mut invalidated = 0;
        for row in rows {
            let (edge_id, old_outcome, old_polarity, blob) =
                row.map_err(|e| anyhow!("Query failed: {e}"))?;
            let Ok(vec) = crate::embed::blob_to_vec(&blob) else {
                continue;
            };
            if crate::embed::cosine_similarity(query_embedding, &vec) < min_similarity {
                continue;
            }
            let old_eff = effective_polarity(old_polarity.as_deref(), &old_outcome);
            if old_eff == Some(false) && new_eff == Some(true) {
                conn.execute(
                    "UPDATE causal_edges SET valid_to = ?1 WHERE id = ?2",
                    params![now, edge_id],
                )?;
                invalidated += 1;
            }
        }
        Ok(invalidated)
    }

    /// Soft-invalidate an edge: set valid_to = now. Returns true if a row was
    /// actually invalidated; false if the edge does not exist or was already
    /// invalidated (no-op).
    pub fn invalidate_edge(&self, edge_id: i64) -> Result<bool> {
        let conn = self.conn.lock().map_err(|e| anyhow!("DB lock: {e}"))?;
        let now = chrono::Utc::now().timestamp();
        let n = conn.execute(
            "UPDATE causal_edges SET valid_to = ?1 WHERE id = ?2 AND valid_to IS NULL",
            params![now, edge_id],
        )?;
        Ok(n > 0)
    }
}
