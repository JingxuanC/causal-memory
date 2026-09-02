//! Causal-edge write path: recording decisions, distilled items, and invalidation.

use std::collections::HashSet;
use std::sync::atomic::Ordering;

use anyhow::{anyhow, Result};
use rusqlite::{params, Connection, OptionalExtension};

use super::{
    containment_similarity, date_tokens, effective_polarity, entry_from_row, is_retraction_record,
    CausalStore, ForkPair, PendingPrediction, PredictionStats, ENTRY_COLUMNS, ID_COUNTER,
    SUPERSEDES_MIN_SHARED_TOKENS, SUPERSEDES_SIM_THRESHOLD,
};

/// One C7 falsification candidate: (old_edge_id, new_edge_id, old_decision,
/// old_outcome, new_decision, new_outcome). The decision texts are identical
/// by construction (the join is on chunk reuse) — `new_decision` is selected
/// anyway so judge call sites stay symmetric. The new-edge id is what soft
/// supersession (`annotate_superseded`) points at — provenance from the
/// superseded lesson to the evidence that corrected it.
pub type FalsificationCandidate = (i64, i64, String, String, String, String);

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
        .map(|(dec_id, _)| dec_id)
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
    ) -> Result<(String, i64)> {
        self.record_decision_full(
            decision,
            outcome,
            relation,
            task_tag,
            confidence,
            discovered_by,
            event_time,
            None,
            None,
        )
    }

    /// Record with an explicit event_time, a pre-judged outcome polarity
    /// (v4: positive/negative/mixed/neutral, judged by the LLM or the
    /// heuristic at the caller), and an optional decision context
    /// (v14: the world state the decision was made in — the abduction
    /// substrate; same task_tag+context ⇒ comparable branch).
    /// `None` polarity stores NULL — read paths then fall back to the
    /// signal-word heuristic. `None` context stores NULL — legacy
    /// behavior, excluded from fork detection.
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
        context: Option<&str>,
    ) -> Result<(String, i64)> {
        let conn = self.acquire()?;
        let db_time = chrono::Utc::now().timestamp();
        let seq = ID_COUNTER.fetch_add(1, Ordering::Relaxed);
        let dec_id = format!("d{}{}", event_time, seq);
        let out_id = format!("o{}{}", event_time, seq);

        // v9: exact-text chunk reuse — the same fact stays ONE node. Identical
        // text reuses the existing chunk id instead of creating a duplicate
        // (previously every call minted fresh nodes; tests had to work around
        // it). Contradiction/supersede queries already match on text, so reuse
        // is consistent and sharpens them. SimHash codes are persisted for
        // near-duplicate detection (observability; reuse stays exact-text to
        // avoid conflating distinct facts that share wording).
        let dec_id = Self::reuse_or_create_chunk(&conn, decision, event_time, &dec_id)?;
        let out_id = Self::reuse_or_create_chunk(&conn, outcome, event_time, &out_id)?;
        // Contradiction short-circuit (rule-based, no LLM): if the same decision
        // already has valid edges whose outcome is contradicted by the new one,
        // the old lesson is falsified by the new evidence — soft-invalidate it.
        // Must run BEFORE inserting the new edge so the new edge is never matched.
        Self::invalidate_contradicted_edges(&conn, decision, outcome, outcome_polarity, db_time)?;
        let fingerprint = context.map(|c| super::context_fingerprint(task_tag, c));
        conn.execute(
            "INSERT INTO causal_edges (from_id, to_id, relation, confidence, discovered_by, event_time, discovered_at, task_tag, outcome_polarity, context_fingerprint, context_text)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                &dec_id,
                &out_id,
                relation,
                confidence,
                discovered_by,
                event_time,
                db_time,
                task_tag,
                outcome_polarity,
                fingerprint,
                context
            ],
        )?;
        // C3: return the edge id too — callers no longer need a follow-up
        // SELECT to resolve it (the server used to re-query by from_id).
        let edge_id = conn.last_insert_rowid();
        // v14 fork detection (Rung-3 Phase A): same fingerprint + a
        // DIFFERENT decision chunk ⇒ natural experiment. Best-effort — a
        // failure here must never fail the record (same discipline as the
        // embedding path). Cap: link at most the 10 most recent siblings so
        // a hot fingerprint stays O(n) not O(n²) pairs.
        if let Some(fp) = &fingerprint {
            let _ = Self::link_forks(&conn, edge_id, &dec_id, fp, db_time);
        }
        Ok((dec_id, edge_id))
    }

    /// v14: find same-fingerprint siblings with a different decision chunk
    /// and insert id-ordered fork pairs (UNIQUE-deduped). Same decision text
    /// re-recorded is the contradiction/supersession path, NOT a fork —
    /// hence the `from_id !=` guard.
    fn link_forks(
        conn: &rusqlite::Connection,
        edge_id: i64,
        from_id: &str,
        fingerprint: &str,
        db_time: i64,
    ) -> Result<usize> {
        let mut stmt = conn.prepare(
            "SELECT id FROM causal_edges
             WHERE context_fingerprint = ?1 AND valid_to IS NULL AND from_id != ?2
             ORDER BY discovered_at DESC, id DESC LIMIT 10",
        )?;
        let siblings: Vec<i64> = stmt
            .query_map(params![fingerprint, from_id], |r| r.get(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(stmt);
        let mut n = 0;
        for sib in siblings {
            let (a, b) = if sib < edge_id {
                (sib, edge_id)
            } else {
                (edge_id, sib)
            };
            n += conn.execute(
                "INSERT OR IGNORE INTO decision_forks (edge_id_a, edge_id_b, fingerprint, discovered_at)
                 VALUES (?1, ?2, ?3, ?4)",
                params![a, b, fingerprint, db_time],
            )?;
        }
        Ok(n)
    }

    /// v14: fork pairs touching any of `edge_ids`, joined to both edges'
    /// endpoint texts and polarities (for the counterfactual same-context
    /// section). Only pairs where BOTH edges are still valid are returned.
    pub fn fork_siblings_for_edges(&self, edge_ids: &[i64]) -> Result<Vec<ForkPair>> {
        if edge_ids.is_empty() {
            return Ok(Vec::new());
        }
        let conn = self.acquire()?;
        let placeholders = vec!["?"; edge_ids.len()].join(",");
        let mut stmt = conn.prepare(&format!(
            "SELECT f.id, f.edge_id_a, f.edge_id_b, f.fingerprint,
                    da.text, oa.text, ea.relation, ea.outcome_polarity,
                    db.text, ob.text, eb.relation, eb.outcome_polarity
             FROM decision_forks f
             JOIN causal_edges ea ON ea.id = f.edge_id_a AND ea.valid_to IS NULL
             JOIN causal_edges eb ON eb.id = f.edge_id_b AND eb.valid_to IS NULL
             JOIN chunks da ON da.id = ea.from_id
             JOIN chunks oa ON oa.id = ea.to_id
             JOIN chunks db ON db.id = eb.from_id
             JOIN chunks ob ON ob.id = eb.to_id
             WHERE f.edge_id_a IN ({placeholders}) OR f.edge_id_b IN ({placeholders})"
        ))?;
        let bind: Vec<&dyn rusqlite::ToSql> = edge_ids
            .iter()
            .chain(edge_ids.iter())
            .map(|i| i as &dyn rusqlite::ToSql)
            .collect();
        let rows = stmt.query_map(bind.as_slice(), |row| {
            Ok(ForkPair {
                pair_id: row.get(0)?,
                edge_id_a: row.get(1)?,
                edge_id_b: row.get(2)?,
                fingerprint: row.get(3)?,
                a_decision: row.get(4)?,
                a_outcome: row.get(5)?,
                a_relation: row.get(6)?,
                a_polarity: row.get(7)?,
                b_decision: row.get(8)?,
                b_outcome: row.get(9)?,
                b_relation: row.get(10)?,
                b_polarity: row.get(11)?,
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }
    /// C7 update-resolver candidate scan: valid edges whose decision chunk
    /// is REUSED (exact same decision text — record_decision reuses chunks)
    /// with a DIFFERENT outcome — the signal that new evidence may have
    /// falsified the old lesson. The rule-based contradiction pass only
    /// auto-invalidates the conservative "old negative -> new positive"
    /// case; everything else in this set needs the LLM judge
    /// (resolve-updates / sleep) to decide.
    pub fn find_falsified_candidates(&self, limit: usize) -> Result<Vec<FalsificationCandidate>> {
        let conn = self.acquire()?;
        let mut stmt = conn.prepare(
            "SELECT old_e.id, new_e.id, old_d.text, old_o.text, old_d.text, new_o.text
             FROM causal_edges old_e
             JOIN chunks old_d ON old_d.id = old_e.from_id
             JOIN chunks old_o ON old_o.id = old_e.to_id
             JOIN causal_edges new_e ON new_e.from_id = old_e.from_id
             JOIN chunks new_o ON new_o.id = new_e.to_id
             WHERE old_e.valid_to IS NULL AND new_e.valid_to IS NULL
               AND old_e.id != new_e.id
               -- id is monotonic (AUTOINCREMENT); event_time can collide within a second
               AND old_e.id < new_e.id
               AND old_o.text != new_o.text
               -- new decision text equals old_d.text: the join is on from_id
               -- (chunk reuse), so the decision chunk is the same row.
             ORDER BY new_e.event_time DESC
             LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit as i64], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, String>(5)?,
            ))
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(|e| anyhow!("Query failed: {e}"))
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
    /// For non-causal items: creates a self-referential `caused` edge so the
    /// item text is visible to edge-based read paths.
    ///
    /// For causal items (kind=Causal): creates a proper directed edge from
    /// the decision chunk to the outcome chunk, using the specified relation
    /// (caused/enabled/prevented). This is the key difference — causal items
    /// form real decision→outcome edges that can be traversed by the
    /// hippocampus spreading activation engine.
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
        let conn = self.acquire()?;
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
            crate::distill::ItemKind::Lesson | crate::distill::ItemKind::Causal => 0.7,
            _ => 0.6,
        };

        // Causal items create a proper directed edge: decision → outcome
        let (chunk_id, edge_id) = if item.kind == crate::distill::ItemKind::Causal {
            let decision_text = item.decision.as_deref().unwrap_or("unknown action");
            let relation = item.causal_relation.map(|r| r.as_str()).unwrap_or("caused");
            Self::insert_causal_distilled(
                &conn,
                decision_text,
                &text,
                relation,
                event_time,
                now,
                confidence,
                task_tag,
            )?
        } else {
            Self::insert_distilled_chunk(&conn, &text, event_time, now, confidence, task_tag)?
        };

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
                // v8: supersession is reversible — the killed edge records
                // WHICH edge superseded it (superseded_by), so restore_edge
                // can roll back when later evidence proves it right.
                let killed = Self::invalidate_superseded(
                    &conn, hint, task_tag, &chunk_id, &text, event_time, now, edge_id,
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
        let chunk_id = Self::insert_chunk_with_retry(conn, text, event_time, None, |seq| {
            format!("distill:{event_time}:{seq}")
        })?;
        Self::index_chunk(conn, &chunk_id, text)?;
        conn.execute(
            "INSERT INTO causal_edges
             (from_id, to_id, relation, confidence, discovered_by, event_time, discovered_at, task_tag)
             VALUES (?1, ?1, 'caused', ?2, 'distill', ?3, ?4, ?5)",
            params![&chunk_id, confidence, event_time, now, task_tag],
        )?;
        Ok((chunk_id, conn.last_insert_rowid()))
    }

    /// Insert a proper causal edge: decision chunk → outcome chunk.
    /// This is the key method for Causal items — it creates TWO chunks
    /// (decision + outcome) and a directed edge between them with the
    /// specified relation (caused/enabled/prevented).
    fn insert_causal_distilled(
        conn: &Connection,
        decision_text: &str,
        outcome_text: &str,
        relation: &str,
        event_time: i64,
        now: i64,
        confidence: f64,
        task_tag: Option<&str>,
    ) -> Result<(String, i64)> {
        let dec_id = Self::insert_chunk_with_retry(conn, decision_text, event_time, None, |seq| {
            format!("distill:d{event_time}:{seq}")
        })?;
        Self::index_chunk(conn, &dec_id, decision_text)?;
        let out_id = Self::insert_chunk_with_retry(conn, outcome_text, event_time, None, |seq| {
            format!("distill:o{event_time}:{seq}")
        })?;
        Self::index_chunk(conn, &out_id, outcome_text)?;
        conn.execute(
            "INSERT INTO causal_edges
             (from_id, to_id, relation, confidence, discovered_by, event_time, discovered_at, task_tag)
             VALUES (?1, ?2, ?3, ?4, 'distill', ?5, ?6, ?7)",
            params![&dec_id, &out_id, relation, confidence, event_time, now, task_tag],
        )?;
        // Return the outcome chunk_id (the "text" that carries the result)
        Ok((out_id, conn.last_insert_rowid()))
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
        superseded_by: i64,
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
                "UPDATE causal_edges
                 SET valid_to = ?1, superseded_by = ?2
                 WHERE id = ?3 AND valid_to IS NULL",
                params![now, superseded_by, edge_id],
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
        let conn = self.acquire()?;
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
        let conn = self.acquire()?;
        let now = chrono::Utc::now().timestamp();
        let n = conn.execute(
            "UPDATE causal_edges SET valid_to = ?1 WHERE id = ?2 AND valid_to IS NULL",
            params![now, edge_id],
        )?;
        Ok(n > 0)
    }

    /// Soft-invalidate a mined meta-causal edge (cross-task pattern): set
    /// valid_to = now. Returns true if a row was actually invalidated;
    /// false if the edge does not exist or was already invalidated
    /// (idempotent no-op). Meta edges were mine-able but not revocable —
    /// this is the revoking half (roadmap).
    pub fn invalidate_meta_edge(&self, edge_id: i64) -> Result<bool> {
        let conn = self.acquire()?;
        let now = chrono::Utc::now().timestamp();
        let n = conn.execute(
            "UPDATE meta_causal_edges SET valid_to = ?1 WHERE id = ?2 AND valid_to IS NULL",
            params![now, edge_id],
        )?;
        Ok(n > 0)
    }

    /// Record a raw conversation turn into `session_logs` (not `chunks`).
    ///
    /// This is the write-time gatekeeping path: raw turns are stored for
    /// audit/replay but are NOT searchable via BM25. Only structured
    /// memories (facts, causal edges, distilled items) enter `chunks`.
    ///
    /// `embedding` (v8) is the turn's semantic vector, the substrate of the
    /// recurrence-triggered distill check (RecMem): a session only gets
    /// distilled when its topic semantically repeats a prior one. `None`
    /// still logs the turn; the session then falls back to eager distill.
    #[allow(clippy::too_many_arguments)]
    pub fn log_session_turn(
        &self,
        id: &str,
        session_id: i64,
        turn_index: i64,
        speaker: &str,
        text: &str,
        event_time: i64,
        task_tag: Option<&str>,
        embedding: Option<&[f32]>,
    ) -> Result<()> {
        let conn = self.acquire()?;
        let embedding_blob = embedding.map(crate::embed::vec_to_blob);
        conn.execute(
            "INSERT OR IGNORE INTO session_logs
             (id, session_id, turn_index, speaker, text, event_time, task_tag, embedding)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                id,
                session_id,
                turn_index,
                speaker,
                text,
                event_time,
                task_tag,
                embedding_blob
            ],
        )?;
        Ok(())
    }

    // ─── v8: Recurrence-triggered distill substrate (RecMem) ──────────────

    /// Mark a session's turn group as distilled (recurrence check resolved).
    pub fn mark_session_distilled(&self, session_id: i64, at: Option<i64>) -> Result<()> {
        let conn = self.acquire()?;
        let now = at.unwrap_or_else(|| chrono::Utc::now().timestamp());
        conn.execute(
            "UPDATE session_logs SET distilled_at = ?1 WHERE session_id = ?2",
            params![now, session_id],
        )?;
        Ok(())
    }

    /// Session ids whose turns are still waiting for their distill decision,
    /// oldest first (batch drain order), capped at `limit`.
    pub fn undistilled_session_ids(&self, limit: usize) -> Result<Vec<i64>> {
        let conn = self.acquire()?;
        let mut stmt = conn.prepare(
            "SELECT session_id FROM session_logs
             WHERE distilled_at IS NULL
             GROUP BY session_id
             ORDER BY MIN(event_time) ASC
             LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit as i64], |row| row.get::<_, i64>(0))?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Ordered (speaker, text) turns of one session.
    pub fn session_turns(&self, session_id: i64) -> Result<Vec<(String, String)>> {
        let conn = self.acquire()?;
        let mut stmt = conn.prepare(
            "SELECT speaker, text FROM session_logs
             WHERE session_id = ?1 ORDER BY turn_index ASC",
        )?;
        let rows = stmt.query_map(params![session_id], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Session date of a turn group (first turn's event_time), if any.
    pub fn session_date(&self, session_id: i64) -> Result<Option<i64>> {
        let conn = self.acquire()?;
        let ts: Option<i64> = conn
            .query_row(
                "SELECT MIN(event_time) FROM session_logs WHERE session_id = ?1",
                params![session_id],
                |row| row.get(0),
            )
            .optional()?;
        Ok(ts)
    }

    /// The stored session embedding (ridding on turn_index 0, where
    /// `distill_recurrence` puts it), if any.
    pub fn session_embedding(&self, session_id: i64) -> Result<Option<Vec<f32>>> {
        let conn = self.acquire()?;
        let blob: Option<Vec<u8>> = conn
            .query_row(
                "SELECT embedding FROM session_logs
                 WHERE session_id = ?1 AND turn_index = 0",
                params![session_id],
                |row| row.get(0),
            )
            .optional()?;
        blob.map(|b| crate::embed::blob_to_vec(&b))
            .transpose()
            .map_err(|e| anyhow!("embedding decode: {e}"))
    }

    /// All (session_id, embedding) pairs for DISTILLED sessions that carry
    /// one — the candidate set for the recurrence check.
    pub fn sessions_with_embeddings(&self, limit: usize) -> Result<Vec<(i64, Vec<f32>)>> {
        let conn = self.acquire()?;
        let mut stmt = conn.prepare(
            "SELECT session_id, embedding FROM session_logs
             WHERE embedding IS NOT NULL AND distilled_at IS NOT NULL AND turn_index = 0
             GROUP BY session_id
             LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit as i64], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, Vec<u8>>(1)?))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (sid, blob) = row.map_err(|e| anyhow!("Query failed: {e}"))?;
            let Ok(vec) = crate::embed::blob_to_vec(&blob) else {
                continue;
            };
            out.push((sid, vec));
        }
        Ok(out)
    }

    // ─── v8: Reversible consolidation ─────────────────────────────────────

    /// Restore a superseded edge: clears `valid_to` and `superseded_by`, so
    /// the old lesson is live again. Returns true when a row was actually
    /// restored (false = no such edge, or it was never superseded).
    ///
    /// This is the rollback half of reversible consolidation (Oracle Agent
    /// Memory): when later evidence proves the old memory right, the
    /// supersession is undone instead of being permanent.
    pub fn restore_edge(&self, edge_id: i64) -> Result<bool> {
        let conn = self.acquire()?;
        let n = conn.execute(
            "UPDATE causal_edges
             SET valid_to = NULL, superseded_by = NULL
             WHERE id = ?1 AND valid_to IS NOT NULL",
            params![edge_id],
        )?;
        Ok(n > 0)
    }

    /// All superseded edges (valid_to set AND superseded_by set), newest
    /// first — the audit view of what reversible consolidation marked.
    pub fn superseded_edges(&self, limit: usize) -> Result<Vec<super::CausalEntry>> {
        let conn = self.acquire()?;
        let sql = format!(
            "SELECT {ENTRY_COLUMNS}
             FROM causal_edges ce
             JOIN chunks cf ON cf.id = ce.from_id
             JOIN chunks ct ON ct.id = ce.to_id
             WHERE ce.valid_to IS NOT NULL AND ce.superseded_by IS NOT NULL
             ORDER BY ce.valid_to DESC
             LIMIT ?1"
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(params![limit as i64], entry_from_row)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(|e| anyhow!("Query failed: {e}"))
    }

    /// Soft supersession: set `superseded_by` WITHOUT touching `valid_to`.
    ///
    /// "Superseded ≠ false" — the old lesson stays fully retrievable (a
    /// counterfactual question about what was done *before* still finds it),
    /// but carries provenance pointing at the edge that corrected it.
    /// Retrieval layers surface the annotation so consumers can tell current
    /// belief from history. Strictly weaker than hard supersede; to clear a
    /// soft mark, set `superseded_by = NULL` directly (`restore_edge` only
    /// matches hard marks).
    ///
    /// Returns true when a row was actually marked.
    pub fn annotate_superseded(&self, old_edge_id: i64, new_edge_id: i64) -> Result<bool> {
        if old_edge_id == new_edge_id {
            return Ok(false);
        }
        let conn = self.acquire()?;
        let n = conn.execute(
            "UPDATE causal_edges
             SET superseded_by = ?2
             WHERE id = ?1 AND valid_to IS NULL AND superseded_by IS NULL",
            params![old_edge_id, new_edge_id],
        )?;
        Ok(n > 0)
    }

    /// v9: exact-text chunk reuse + DG SimHash maintenance.
    ///
    /// Identical text maps to the SAME chunk id (one fact, one node). New
    /// chunks persist their SimHash sparse code; near-duplicates (hamming ≤ 2,
    /// both texts ≥ 20 chars) are detected and logged for observability —
    /// reuse itself stays exact-text so distinct facts that merely share
    /// wording are never conflated.
    fn reuse_or_create_chunk(
        conn: &Connection,
        text: &str,
        event_time: i64,
        fallback_id: &str,
    ) -> Result<String> {
        // Exact reuse: same text → same node (oldest first).
        let existing: Option<String> = conn
            .query_row(
                "SELECT id FROM chunks WHERE text = ?1 ORDER BY created_at ASC LIMIT 1",
                params![text],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(id) = existing {
            // Keep the inverted index complete even for reused chunks
            // (older rows may predate the bm25_index migration).
            Self::index_chunk(conn, &id, text)?;
            return Ok(id);
        }
        let code = crate::hippocampus::utils::simhash(text);
        // Near-duplicate observability (long texts only — short texts collide
        // in 128-bit simhash buckets far too often to be meaningful).
        // Rows written before 0.9.3 carry old-scheme (whitespace-token) codes;
        // mixed-era hamming comparisons may miss a near-dup log line until the
        // old rows age out. Observability only — real dedup is exact-text.
        if text.chars().count() >= 20 {
            let mut stmt = conn.prepare(
                "SELECT id, sparse_code FROM chunks
                 WHERE sparse_code IS NOT NULL AND length(text) >= 20",
            )?;
            let rows =
                stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
            for row in rows {
                let (id, code_hex) = row?;
                if let Ok(other) = u128::from_str_radix(&code_hex, 16) {
                    if (code ^ other).count_ones() <= 2 {
                        eprintln!(
                            "chunk reuse: near-duplicate of {id} (hamming ≤ 2) — exact text differs, keeping separate node"
                        );
                    }
                }
            }
        }
        // B2/defensive: retry on UNIQUE collision — the process-global
        // ID_COUNTER can be reset concurrently (tests simulating process
        // restart), so two writers can mint the same fallback id even
        // though fetch_add itself is atomic. A fresh seq makes the retry id
        // unique; the exact-text reuse path above already covers duplicates.
        let prefix = fallback_id.chars().next().unwrap_or('c');
        let id = Self::insert_chunk_with_retry(
            conn,
            text,
            event_time,
            Some(&format!("{:032x}", code)),
            |seq| format!("{prefix}{event_time}{seq}"),
        )?;
        Self::index_chunk(conn, &id, text)?;
        Ok(id)
    }

    /// Insert a chunks row, retrying with a fresh sequence when the id
    /// collides (defensive — see reuse_or_create_chunk). `make_id` builds
    /// the id from the fresh seq so each retry tries a new, unique id.
    fn insert_chunk_with_retry<F: Fn(u64) -> String>(
        conn: &Connection,
        text: &str,
        event_time: i64,
        sparse_code: Option<&str>,
        make_id: F,
    ) -> Result<String> {
        for _ in 0..4 {
            let seq = ID_COUNTER.fetch_add(1, Ordering::Relaxed);
            let id = make_id(seq);
            match conn.execute(
                "INSERT INTO chunks (id, text, created_at, sparse_code) VALUES (?1, ?2, ?3, ?4)",
                params![&id, text, event_time, sparse_code],
            ) {
                Ok(_) => return Ok(id),
                Err(e) if e.to_string().contains("UNIQUE") => continue,
                Err(e) => return Err(e.into()),
            }
        }
        anyhow::bail!("chunk id collision after 4 retries")
    }

    // ─── v14 prediction ledger (Rung-3 Phase A) ──────────────────────────

    /// Log a counterfactual verdict as a falsifiable prediction. Returns
    /// the prediction id (the footer reports it). Best-effort by contract:
    /// the caller (counterfactual_inner) ignores errors so a ledger failure
    /// never breaks the query itself.
    pub fn log_prediction(
        &self,
        option_a: &str,
        option_b: &str,
        task_tag: Option<&str>,
        verdict: &str,
        method: &str,
        confidence: f64,
        evidence: Option<&str>,
    ) -> Result<i64> {
        let conn = self.acquire()?;
        conn.execute(
            "INSERT INTO pending_predictions
             (created_at, option_a, option_b, task_tag, verdict, method, confidence, evidence)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                chrono::Utc::now().timestamp(),
                option_a,
                option_b,
                task_tag,
                verdict,
                method,
                confidence,
                evidence
            ],
        )?;
        Ok(conn.last_insert_rowid())
    }

    /// Resolve predictions whose option_a/option_b matches `decision_text`
    /// (exact text — consistent with chunk-reuse matching). First matching
    /// record wins: only still-pending rows update, so re-records never
    /// re-resolve. `correct` semantics (see the design doc §5):
    ///   prefer_X + took X → 1 iff positive, 0 iff negative, else NULL
    ///   prefer_X + took Y → 1 iff negative, 0 iff positive, else NULL
    ///   no_difference     → resolves with NULL (weak evidence either way)
    /// Returns the number of predictions resolved.
    pub fn resolve_predictions_for_decision(
        &self,
        decision_text: &str,
        actual_polarity: Option<&str>,
    ) -> Result<usize> {
        let conn = self.acquire()?;
        let now = chrono::Utc::now().timestamp();
        let mut stmt = conn.prepare(
            "SELECT id, verdict, option_a, option_b FROM pending_predictions
             WHERE resolved_at IS NULL AND (option_a = ?1 OR option_b = ?2)",
        )?;
        let pending: Vec<(i64, String, String, String)> = stmt
            .query_map(params![decision_text, decision_text], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(stmt);
        let polarity = actual_polarity.unwrap_or("neutral");
        let mut n = 0;
        for (id, verdict, opt_a, _opt_b) in pending {
            let took_a = opt_a == decision_text;
            let correct: Option<i64> = match verdict.as_str() {
                "no_difference" => None,
                v => {
                    let preferred_a = v == "prefer_a";
                    // Followed the preferred option? Correct iff it paid off.
                    // Took the rejected one? Correct iff it failed.
                    let followed = preferred_a == took_a;
                    match polarity {
                        "positive" => Some((followed) as i64),
                        "negative" => Some((!followed) as i64),
                        _ => None, // mixed/neutral actual → ambiguous
                    }
                }
            };
            conn.execute(
                "UPDATE pending_predictions
                 SET resolved_at = ?2, resolved_option = ?3, actual_polarity = ?4, correct = ?5
                 WHERE id = ?1 AND resolved_at IS NULL",
                params![id, now, if took_a { "a" } else { "b" }, polarity, correct],
            )?;
            n += 1;
        }
        Ok(n)
    }

    /// Calibration stats for `prediction_report`: (resolved, correct,
    /// ambiguous, pending) overall plus per-method and per-task_tag splits.
    /// Ambiguous rows (correct IS NULL after resolution) count as resolved
    /// but excluded from accuracy denominators — reporting them separately
    /// is the honest read.
    pub fn prediction_stats(&self) -> Result<PredictionStats> {
        let conn = self.acquire()?;
        let pending: i64 = conn.query_row(
            "SELECT COUNT(*) FROM pending_predictions WHERE resolved_at IS NULL",
            [],
            |r| r.get(0),
        )?;
        let mut stmt = conn.prepare(
            "SELECT method, COALESCE(task_tag, ''), COUNT(*),
                    SUM(CASE WHEN correct = 1 THEN 1 ELSE 0 END),
                    SUM(CASE WHEN correct IS NULL AND resolved_at IS NOT NULL THEN 1 ELSE 0 END)
             FROM pending_predictions
             WHERE resolved_at IS NOT NULL
             GROUP BY method, COALESCE(task_tag, '')",
        )?;
        let rows: Vec<(String, String, i64, i64, i64)> = stmt
            .query_map([], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut stats = PredictionStats {
            pending,
            ..PredictionStats::default()
        };
        for (method, tag, resolved, correct, ambiguous) in rows {
            stats.resolved += resolved;
            stats.correct += correct;
            stats.ambiguous += ambiguous;
            let entry = stats.by_method.entry(method).or_default();
            entry.resolved += resolved;
            entry.correct += correct;
            entry.ambiguous += ambiguous;
            let entry = stats
                .by_task_tag
                .entry(if tag.is_empty() {
                    "(none)".to_string()
                } else {
                    tag
                })
                .or_default();
            entry.resolved += resolved;
            entry.correct += correct;
            entry.ambiguous += ambiguous;
        }
        Ok(stats)
    }

    /// Recent still-pending predictions for the report footer.
    pub fn pending_predictions(&self, limit: usize) -> Result<Vec<PendingPrediction>> {
        let conn = self.acquire()?;
        let mut stmt = conn.prepare(
            "SELECT id, created_at, option_a, option_b, COALESCE(task_tag, ''), method
             FROM pending_predictions WHERE resolved_at IS NULL
             ORDER BY created_at DESC, id DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit as i64], |r| {
            Ok(PendingPrediction {
                id: r.get(0)?,
                created_at: r.get(1)?,
                option_a: r.get(2)?,
                option_b: r.get(3)?,
                task_tag: r.get(4)?,
                method: r.get(5)?,
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }
}
