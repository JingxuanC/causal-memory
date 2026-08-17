# Nature-Style Reviewer Assessment — `causal-memory`

> Reviewer-emulation package produced per the `nature-reviewer` skill. Three
> reviewer reports plus a cross-review synthesis, grounded only in the supplied
> manuscript material (`paper-full-draft.md` + `paper-section4-experiments.md`).
> The user declares the target venue as a generic systems venue
> (OSDI / ATC / ACL); the Nature-style axes are applied as the assessment frame,
> with venue-specific notes where the systems context sharpens a concern.

---

## Review setup

- **Input scope.** Two Markdown files comprising a full draft: Abstract,
  §1 Introduction, §2 Related Work, §3 System Design, §4 Experiments (separate
  file), §5 Discussion, §6 Conclusion, and a Reproducibility note. No figures
  are supplied (tables only). No supplementary material, no per-question result
  files, no source-code artifacts beyond the referenced repository URLs.

- **Assessment boundary.** The review is bounded to what the supplied text
  asserts and what the six tables report. Per-question JSONL results, the
  distillation prompts, the exact compaction prompt, the trap-world task
  definitions, and any figures are **not assessable** from the provided
  material; where they matter, the review flags `AUTHOR_INPUT_NEEDED` rather
  than inferring their content.

- **Shared manuscript claim summary.** The paper presents **causal-memory**, an
  agent-memory system that unifies all memory types (facts, temporal state,
  causal edges, co-occurrence, meta-patterns) as typed edges on a single CSR
  graph processed by a hippocampus-inspired spreading-activation engine. The
  headline innovation is an **excitatory / inhibitory duality**: `caused`
  edges spread positive activation (+1.0) while `prevented` edges spread
  negative activation (−0.3), a GABA analogue that no prior memory system
  implements. Five claims are advanced: (1) causal edges survive iterative
  context compaction where textual recall collapses; (2) the system approaches
  frontier LoCoMo performance (~89 % at frontier-compatible judge caliber vs
  mem0's 91.6 %); (3) multi-session recall improves via query/session
  expansion; (4) causal memory halves the repeat-mistake rate in an end-to-end
  agent ablation; (5) the causal graph functions as an explicit world model,
  enabling forward simulation (`intervention_query`) that "notebook-style"
  memory cannot offer.

- **Visible evidence base.**
  - Table 1 — compaction survival (10 conversations × 10 probes, *k* = 1–5).
  - Table 2 — LoCoMo optimization matrix (1 986 questions, six configs).
  - Table 3 — judge × prompt 2 × 2 matrix.
  - Table 4 — LongMemEval multi-session enhancement pipeline (500 questions).
  - Table 5 — agent ablation (6 trap tasks, seed 42, conditions A/B).
  - Table 6 — three-model comparison (deepseek-chat / v4-pro / glm-5.2).
  - One statistical test reported: paired bootstrap, *p* < 0.01, 1 000 resamples
    (compaction rescue only).
  - System description: seven edge types, CSR SpMV, SWR consolidation, 13 MCP
    tools, RRF fusion, BM25 + optional semantic retrieval.

- **Missing materials affecting confidence.** (a) No latency / throughput /
  memory-footprint evaluation — critical for a systems venue. (b) No ablation
  isolating the contribution of inhibitory (`prevented`) edges. (c) No
  head-to-head experimental comparison with HeLa-Mem. (d) No benchmark for the
  forward-simulation (`intervention_query`) contribution. (e) No train / test
  split on LoCoMo, raising overfitting risk for the optimization matrix.
  (f) No figures; table-only reporting limits assessment of trends and
  variance. (g) Per-question result files and distillation prompts referenced
  but not supplied.

---

## Reviewer 1

*Reviewer 1 places greatest weight on technical soundness and the experimental
evidence chain.*

### Overall assessment

The paper identifies a genuine and timely problem — the fragility of causal
information under iterative context compaction in long-running LLM agents — and
proposes an architecturally interesting solution. However, the experimental
case as currently presented has several structural weaknesses: the central
compaction experiment is close to tautological in its control design, the
headline "halves repeat-mistake rate" rests on six tasks and a single seed, the
systems-performance evaluation that a venue like OSDI/ATC would expect is
entirely absent, and the one genuinely novel mechanism — inhibitory spreading
activation — is never isolated in an ablation. The architecture is described
plausibly, but the evidence does not yet establish that the proposed mechanisms
(rather than simply storing data outside the compaction pipeline) produce the
claimed gains.

### Who would be interested in the results, and why

Developers of long-running agent runtimes who face context-window exhaustion
in production will be interested in the compaction-survival framing.
Memory-system researchers working on retrieval-augmented agent memory will be
interested in the typed-edge unification and the inhibitory-edge proposal.
Systems researchers will look for — and currently not find — end-to-end
latency, throughput, and scaling data.

### Major strengths

- The compaction-survival problem is framed as a first-class benchmark, which
  is a useful conceptual contribution regardless of the specific solution.
- The typed-edge taxonomy with explicit spread coefficients is a clean,
  implementable design that unifies heterogeneous memory types on one substrate.
- The LoCoMo optimization matrix (Table 2) transparently decomposes gains into
  prompt engineering, retrieval budget, and semantic retrieval — the
  attribution logic is commendably honest.
- The dual-judge-caliber analysis (Table 3) is a principled attempt to separate
  architecture gaps from evaluation-protocol gaps.
- Open-source release with 163 tests and reproducible harnesses is a strong
  positive for a systems venue.

### Major concerns

**R1-M1 — [experimental-design]**
Claim pointer: Causal edges maintain 100 % recall after five iterative LLM
compactions where textual recall collapses to 45 %.
Evidence pointer: §4.1, Table 1.
Concern: The control condition stores causal information "in a separate SQLite
table (`causal_edges`) that is never exposed to the compaction prompt." By
construction, any data stored outside a lossy pipeline survives that pipeline.
The experiment therefore demonstrates that *external storage is immune to a
process that only operates on internal storage* — a near-tautology — rather
than demonstrating that the causal-memory *architecture* (typed edges,
spreading activation, inhibitory dynamics) is responsible for the survival.
The 100 % vs 45 % contrast would hold for any external key-value store, a
plain text file, or a vector database. The experiment does not isolate what
causal-memory adds beyond "store it elsewhere."
Resolution test: Add a control that stores the same causal information in a
comparable external store without the typed-edge graph and the
spreading-activation engine (e.g., a flat fact table queried by keyword),
then show that the causal-graph architecture produces measurably better
combined QA accuracy than the flat external store at equal retrieval budget.
Alternatively, narrow the claim to "externalized causal storage survives
compaction" and reframe the architectural contribution separately.

**R1-M2 — [experimental-design]**
Claim pointer: An end-to-end agent ablation shows that causal memory halves the
repeat-mistake rate (67 % → 33 %) on known-trap tasks.
Evidence pointer: §4.4, Table 5.
Concern: The ablation uses 6 tasks, 1 model (glm-4-plus), and 1 seed (42).
With 6 tasks the per-condition mistake counts are in the single digits; a
difference of a few mistakes swings the rate by 10+ percentage points. No
uncertainty estimate, no seed variation, and no task-family diversity analysis
is reported. The "halves" framing implies a robust effect that the sample size
cannot support.
Resolution test: Scale to at least 20–30 trap tasks across multiple task
families, run ≥ 5 seeds, and report the repeat-mistake rate with a confidence
interval or the full distribution. If the effect is robust it will survive;
if not, the claim must be calibrated to "in a small-scale pilot."

**R1-M3 — [experimental-design]**
Claim pointer: causal-memory is a system paper; the venue is OSDI/ATC/ACL.
Evidence pointer: §3 (System Design), §4 (Experiments), Reproducibility note.
Concern: For a systems venue, the complete absence of any
systems-performance evaluation is a major gap. There is no measurement of
ingest latency, retrieval latency, graph-construction cost, SpMV throughput,
memory footprint, or scaling behaviour as the graph grows. The CSR format and
5-hop spreading activation are described architecturally but never
characterized empirically. A systems reviewer cannot assess whether the design
is practical without these numbers.
Resolution test: Add a systems-performance microbenchmark: ingest throughput
(edges/s), retrieval latency (p50/p99) at varying graph sizes (1k / 10k /
100k edges), memory footprint, and SpMV cost per query. Compare against at
least one baseline memory system under matched retrieval budget.

**R1-M4 — [experimental-design]**
Claim pointer: With prompt engineering, retrieval budget expansion, and
semantic retrieval, causal-memory approaches frontier factual-recall
performance on LoCoMo (79.1 %).
Evidence pointer: §4.2, Table 2.
Concern: The six-config optimization matrix is evaluated on the full 1 986
LoCoMo questions with no mention of a train / validation / test split. The
prompt (V2 7-step), retrieval budget (top-50), and fusion method (RRF) appear
to be tuned on the same benchmark used to report the headline number, risking
optimization-on-test-set. The per-category gains (e.g., multi-hop +16.6 pp
from the prompt) may reflect prompt overfitting to LoCoMo's question style.
Resolution test: Report whether any configuration was selected using a held-out
split. If not, re-run the optimization matrix on a held-out subset and report
final numbers on the test split, or add an external dataset (e.g., the
LongMemEval numbers serve partially but use a different question distribution).

**R1-M5 — [experimental-design]**
Claim pointer: The gap to mem0's published 91.6 % is dominated by judge caliber
and model quality, not architecture.
Evidence pointer: §4.2, Table 3; §4.5, Table 6.
Concern: The mem0 comparison is not matched. mem0 uses gpt-5 as answerer and
judge with top-200 retrieval; causal-memory uses deepseek-chat with top-50.
The paper recalibrates its own judge to be "mem0-compatible" and infers that
the remaining 7.5 pp gap is "attributable to model quality and retrieval
budget," but no experiment holds the model or retrieval budget constant across
the two systems. The attribution is plausible but unsupported by a controlled
comparison.
Resolution test: Run causal-memory's retrieval pipeline with gpt-5 as answerer
and top-200 retrieval on a subset of LoCoMo, or run mem0's pipeline with
deepseek-chat and top-50, to produce at least one point where model and budget
are matched. Even a partial crossover would strengthen the attribution
substantially.

### Technical failings that need to be addressed before the case is established

- **The inhibitory mechanism is never ablated.** The paper's "core innovation"
  is the excitatory/inhibitory duality (`prevented` edges, −0.3 spread). No
  experiment removes `prevented` edges and measures the effect on any
  benchmark. Without this ablation, the claim that inhibitory dynamics
  constitute a functional advance — rather than a biologically motivated
  design choice — is unsupported. (See R1-concern below; also raised by
  Reviewer 2.)

- **deepseek-v4-pro's 82.3 % is computed on a biased subset.** §4.5 reports
  459 API timeouts (23 % of questions). The 82.3 % "non-error accuracy"
  excludes timeouts, which are non-random (longer / harder questions time out
  preferentially). Using this number to argue that "the architecture gap is
  primarily a model gap" (§4.5) conflates a censored-sample artefact with a
  model-quality conclusion.

### Assessment against Nature-style criteria

- **Originality.** The typed-edge unification and inhibitory-edge proposal are
  architecturally novel relative to the cited prior work. The originality of
  the *compaction-survival* result is weakened by the tautological control
  (R1-M1).
- **Scientific importance.** Potentially important for the agent-memory
  subfield; the broader systems importance cannot be judged without
  performance evaluation (R1-M3).
- **Interdisciplinary readership.** The hippocampus analogy may attract
  neuro-symbolic interest, but the current evidence does not demonstrate that
  the analogy yields functional benefits (no inhibitory ablation).
- **Technical soundness.** Currently the weakest axis. Five major
  experimental-design concerns (R1-M1 through R1-M5) plus the unvalidated
  core mechanism and the censored-sample issue must be resolved.
- **Readability for nonspecialists.** Adequate; the abstract and introduction
  are clearly written for a systems audience. Some jargon (CSR, SpMV, SWR,
  RRF) is used without expansion.

### Recommendation posture

**Currently not established from the provided evidence.** The architecture is
interesting and the problem is real, but the experimental case has structural
gaps — a near-tautological control, a tiny ablation sample, no
systems-performance evaluation, and no ablation of the headline mechanism.
Resolving R1-M1 through R1-M5 and adding the inhibitory ablation would move
this to a defensible submission for a systems venue.

### Substantive concerns (traceable)

```
R1-M1 — [experimental-design]
Claim pointer: Causal edges maintain 100% recall after five compactions where
textual recall collapses to 45%.
Evidence pointer: §4.1, Table 1.
Concern: The control (external SQLite table never exposed to compaction)
survives by construction; the experiment is near-tautological and does not
isolate the causal-memory architecture's contribution.
Resolution test: Add a flat-external-store control at equal retrieval budget,
or narrow the claim to externalized storage survival.

R1-M2 — [experimental-design]
Claim pointer: Causal memory halves the repeat-mistake rate (67% → 33%).
Evidence pointer: §4.4, Table 5.
Concern: 6 tasks, 1 model, 1 seed — the sample cannot support a robust
"halves" claim; no uncertainty reported.
Resolution test: Scale to ≥20 tasks, ≥5 seeds; report distribution or CI.

R1-M3 — [experimental-design]
Claim pointer: causal-memory is a system (targeting OSDI/ATC/ACL).
Evidence pointer: §3, §4, Reproducibility.
Concern: No latency, throughput, memory-footprint, or scaling evaluation —
essential for a systems venue.
Resolution test: Add systems-performance microbenchmarks and a baseline
comparison.

R1-M4 — [experimental-design]
Claim pointer: LoCoMo optimization yields 79.1% (best config).
Evidence pointer: §4.2, Table 2.
Concern: No train/test split reported; the optimization matrix may be tuned
on the test set.
Resolution test: Report held-out evaluation or external-dataset validation.

R1-M5 — [experimental-design]
Claim pointer: The gap to mem0 is attributable to model quality and retrieval
budget, not architecture.
Evidence pointer: §4.2 Table 3, §4.5 Table 6.
Concern: No matched comparison (model and retrieval budget differ across
systems); the attribution is inferred, not demonstrated.
Resolution test: Run at least one crossover configuration with matched model
and/or budget.

R1-m1 — [statistical-rigor]
Claim pointer: +20.8pp rescue is statistically indistinguishable from
zero-compaction performance (p < 0.01).
Evidence pointer: §4.1.
Concern: Only one result in the entire paper reports a statistical test.
All other headline numbers (Tables 2, 4, 5, 6) report point estimates with
no uncertainty.
Resolution test: Add confidence intervals or bootstrap CIs for all headline
comparisons, especially the agent ablation (Table 5).

R1-m2 — [mechanism-evidence]
Claim pointer: deepseek-v4-pro achieves 82.3% per-question accuracy,
confirming the architecture gap is a model gap.
Evidence pointer: §4.5, Table 6.
Concern: 82.3% excludes 459 timeouts (23%), a non-random subset. The number
is a censored-sample artefact, not a clean model-quality comparison.
Resolution test: Report accuracy under a timeout-free protocol (lower
concurrency, longer timeout) or flag the censoring explicitly and withdraw
the inferential claim.

R1-m3 — [mechanism-evidence]
Claim pointer: The excitatory/inhibitory duality is the core architectural
insight.
Evidence pointer: §3.1, §5.1.
Concern: No ablation removes `prevented` edges and measures the effect on any
benchmark. The functional contribution of inhibitory dynamics is asserted,
not demonstrated.
Resolution test: Run LoCoMo / LongMemEval / the agent ablation with and
without `prevented` edges; report the delta.
```

---

## Reviewer 2

*Reviewer 2 places greatest weight on originality and scientific importance.*

### Overall assessment

The paper makes an ambitious originality claim — the first memory system to
implement inhibitory spreading activation, positioning agent memory as an
explicit world model — and frames this as a paradigm shift from "notebook" to
"simulator." The conceptual framing is genuinely interesting and the
biological motivation is engaging. However, the originality case rests on
distinctions from prior work (particularly HeLa-Mem) that are argued
conceptually but never tested experimentally, and several significance claims
— especially the world-model / forward-simulation contribution — are asserted
at a level the evidence does not reach. The paper would benefit from either
strengthening the evidence for its strongest claims or calibrating the claims
to match what the experiments actually show.

### Who would be interested in the results, and why

Researchers at the intersection of neuro-symbolic AI, hippocampus-inspired
computing, and agent architectures will find the excitatory/inhibitory framing
provocative. The broader ML-systems community will be interested in the
compaction-survival problem as a new evaluation dimension. The claim of
forward simulation (what-if queries) would interest the planning and
model-based RL communities — *if* it were validated, which it currently is
not.

### Major strengths

- The conceptual move from "notebook" to "simulator" memory is a compelling
  framing that could shape how the community thinks about agent memory.
- The inhibitory-edge proposal is, to my knowledge, genuinely novel in the
  agent-memory literature; the GABA/glutamate analogy is evocative and could
  attract cross-disciplinary interest.
- The Pearl causal-hierarchy positioning (Rung-1 attribution, Rung-2
  intervention, Rung-2.5 contrastive) is a principled way to scope the claims,
  and the explicit disavowal of Rung-3 counterfactuals shows intellectual
  honesty.
- The honest decomposition of the LoCoMo gap into judge-caliber, model-quality,
  and retrieval-budget components is a mature form of self-assessment.

### Major concerns

**R2-M1 — [novelty-significance]**
Claim pointer: The excitatory/inhibitory duality is the core architectural
insight; no existing memory system implements inhibitory dynamics.
Evidence pointer: Abstract; §1 (contribution 1); §2.2; §3.1; §5.1; §6.
Concern: The novelty claim is asserted through enumeration of competitors
("HeLa-Mem builds only the excitatory side"; §2.2) but never validated
functionally. The paper does not show that inhibitory edges change retrieval
outcomes in any benchmark. Without an ablation isolating `prevented`-edge
contribution, the reader cannot distinguish "a novel mechanism that matters"
from "a novel label on an edge that is never traversed in practice." The
biological analogy, however evocative, does not substitute for evidence that
the inhibitory pathway changes system behaviour.
Resolution test: Provide at least one task or benchmark where the presence of
`prevented` edges measurably changes the retrieved results or the agent's
behaviour, compared to the same system with `prevented` edges disabled or
converted to `caused` edges with negative weight. The risk-averse planning
scenario described in §5.1 is a natural candidate.

**R2-M2 — [novelty-significance]**
Claim pointer: The causal graph functions as an explicit world model; forward
traversal performs simulation; no competing memory system offers this.
Evidence pointer: §1 (contribution 4); §3.5; §5.2.
Concern: Forward simulation via `intervention_query` is listed as a headline
contribution and is central to the "notebook → simulator" framing, but it has
no benchmark and no validation of prediction accuracy. The paper itself
acknowledges this in §5.3 ("Forward simulation is unbenchmarked … our highest
priority future work"). Promoting an unvalidated feature to a headline
contribution inflates the originality claim beyond what the evidence supports.
The world-model claim ("each `caused` edge is a sample of the transition
function"; §5.2) is conceptually appealing but is not tested against held-out
transitions.
Resolution test: Either (a) design and run a forward-simulation benchmark
measuring prediction accuracy of `intervention_query` on held-out decisions,
or (b) demote forward simulation from a headline contribution to a "designed
and implemented, validation pending" feature, and adjust the abstract /
introduction / conclusion accordingly.

**R2-M3 — [experimental-design]**
Claim pointer: HeLa-Mem (ACL 2026) is the closest competitor; causal-memory
absorbs its Hebbian mechanism and adds the inhibitory side it lacks.
Evidence pointer: §2.2.
Concern: The comparison with HeLa-Mem is entirely conceptual. No experiment
runs HeLa-Mem's retrieval or consolidation on the same data and compares
against causal-memory. The paper cites HeLa-Mem's own ablation numbers
(−2.55 pp for spreading activation, −4.87 pp for consolidation) but does not
reproduce or directly compare. For a paper whose originality hinges on
surpassing HeLa-Mem, the absence of a head-to-head is a significant gap.
Resolution test: Implement or simulate HeLa-Mem's excitatory-only
spreading-activation retrieval on the same distill databases and benchmarks,
and show the delta that inhibitory edges produce. If HeLa-Mem's code is
unavailable, clearly state this and provide the closest reproducible
excitatory-only baseline.

**R2-M4 — [claim-moderation]**
Claim pointer: At frontier-compatible judge caliber, the system scores ~89 %,
narrowing the gap to mem0's 91.6 % to 2–3 pp attributable to model quality.
Evidence pointer: Abstract; §1; §4.2 (Table 3).
Concern: The ~89 % figure does not appear in any table. Table 3 reports
84.1 % under the "mem0-compatible judge" with the V2 prompt. The ~89 % appears
to be an extrapolation combining the mem0-compatible judge (84.1 %) with the
v4-pro non-error accuracy (82.3 %) or a hypothetical gpt-5 answerer, but the
extrapolation logic is not shown. Presenting an extrapolated number in the
abstract as if measured overstates the result.
Resolution test: Either run the actual configuration that produces ~89 %
(frontier answerer + mem0-compatible judge) and report it in a table, or
replace "~89 %" in the abstract with the measured 84.1 % and describe the
extrapolation as speculative.

**R2-M5 — [claim-moderation]**
Claim pointer: causal-memory is "the first memory system to implement negative
activation spread."
Evidence pointer: Abstract; §1 (contribution 1); §6.
Concern: The "first" claim is strong and depends on the scope of the
literature survey. The related-work section covers five architectural patterns
and names specific systems (Mem0, Zep, Letta, OpenViking, HeLa-Mem) but does
not survey the broader neuro-symbolic or spreading-activation literature where
inhibitory dynamics have a long history (e.g., constraint-satisfaction
networks, interactive activation models). The "first" claim may hold within
the *agent-memory* niche but is stated without scope qualification.
Resolution test: Qualify the claim to "the first *agent-memory* system to
implement inhibitory spreading activation" and briefly note why inhibitory
dynamics in classical spreading-activation models (which do exist) do not
transfer to the agent-memory setting, or cite the broader literature to
establish the boundary.

### Technical failings that need to be addressed before the case is established

- The originality case depends on the inhibitory mechanism being functionally
  consequential (R2-M1) and on forward simulation being validated (R2-M2).
  Neither is currently demonstrated. Until at least one of these is addressed,
  the strongest originality claims are unsupported.

- The conceptual distinction from HeLa-Mem (R2-M3) needs at least a
  reproducible excitatory-only baseline to be credible.

### Assessment against Nature-style criteria

- **Originality.** The inhibitory-edge and world-model framing are
  conceptually novel and potentially field-shaping, but the novelty is
  *asserted* more than *demonstrated*. The "first" claim needs scope
  qualification (R2-M5).
- **Scientific importance.** The compaction-survival framing is potentially
  important; the world-model claim is currently aspirational (R2-M2). Overall
  importance hinges on whether the unvalidated contributions can be
  substantiated.
- **Interdisciplinary readership.** The neuro-symbolic framing could attract
  broad interest, but the lack of functional validation of the biological
  analogy limits the cross-disciplinary appeal to "evocative metaphor" rather
  than "demonstrated principle."
- **Technical soundness.** Adequate on the systems that *are* benchmarked
  (LoCoMo, LongMemEval), but the *headline* contributions (inhibition,
  simulation) are unbenchmarked. See Reviewer 1 for additional
  experimental-design concerns.
- **Readability for nonspecialists.** The "notebook vs simulator" framing is
  accessible and effective. The Pearl hierarchy positioning is well explained.

### Recommendation posture

**Promising but the originality and significance case remains
underdeveloped.** The conceptual contributions are strong, but two of the five
listed contributions (inhibitory dynamics, forward simulation) lack functional
validation, the closest competitor is not compared experimentally, and one
headline number (~89 %) appears to be an unshown extrapolation. With the
inhibitory ablation (R2-M1), a forward-simulation benchmark or claim demotion
(R2-M2), and calibration of the ~89 % figure (R2-M4), this becomes a
substantially stronger submission.

### Substantive concerns (traceable)

```
R2-M1 — [mechanism-evidence]
Claim pointer: The excitatory/inhibitory duality is the core architectural
insight; no existing system implements inhibitory dynamics.
Evidence pointer: Abstract; §1 contribution 1; §3.1; §5.1.
Concern: No ablation isolates `prevented`-edge contribution; the functional
consequence of inhibition is asserted, not demonstrated.
Resolution test: Ablate `prevented` edges on at least one benchmark; show the
delta on retrieval outcomes or agent behaviour.

R2-M2 — [claim-moderation]
Claim pointer: The causal graph functions as an explicit world model; forward
traversal performs simulation (headline contribution 4).
Evidence pointer: §1 contribution 4; §3.5; §5.2; §5.3 limitation 6.
Concern: Forward simulation is unbenchmarked (admitted in §5.3) yet promoted
as a headline contribution and central to the abstract/conclusion framing.
Resolution test: Benchmark `intervention_query` prediction accuracy on
held-out decisions, or demote to "implemented, validation pending."

R2-M3 — [experimental-design]
Claim pointer: HeLa-Mem is the closest competitor; causal-memory surpasses it
by adding inhibitory dynamics.
Evidence pointer: §2.2.
Concern: No head-to-head experiment; comparison is conceptual only.
Resolution test: Run excitatory-only spreading activation (HeLa-Mem-style) on
the same data; report the delta.

R2-M4 — [claim-moderation]
Claim pointer: At frontier-compatible judge caliber the system scores ~89%.
Evidence pointer: Abstract; §1; §4.2 Table 3.
Concern: ~89% does not appear in any table (Table 3 shows 84.1%); the
extrapolation is not shown.
Resolution test: Run the actual configuration and report it, or replace with
the measured 84.1% and label the rest as speculative.

R2-M5 — [novelty-significance]
Claim pointer: causal-memory is "the first memory system to implement negative
activation spread."
Evidence pointer: Abstract; §1; §6.
Concern: The "first" claim is unscoped; inhibitory dynamics exist in classical
spreading-activation / constraint-satisfaction literature.
Resolution test: Qualify to "first agent-memory system" and address the
broader literature boundary.

R2-m1 — [data-resource-quality]
Claim pointer: A typical 419-turn conversation yields only 49 distilled causal
edges — 12% of turns.
Evidence pointer: §5.2.
Concern: The world-model claim depends on edge coverage, but 12 % coverage
means the "transition function" is very sparsely sampled. This weakens the
simulation/attribution claims proportionally.
Resolution test: Report coverage across multiple conversations (not one
example) and discuss how coverage affects simulation fidelity, or scope the
world-model claim to "sparse partial world model."

R2-m2 — [causal-vs-correlative]
Claim pointer: Each `caused` edge is a sample of the transition function
f(state, action) → outcome.
Evidence pointer: §5.2.
Concern: A single observed co-occurrence of decision and outcome is a
correlational sample, not a causal one, unless confounders are controlled.
The `caused` label is assigned by an LLM distillation prompt, not by
interventional evidence. The paper correctly disavows Rung-3 but uses
causal language ("caused," "prevented") for LLM-assigned labels.
Resolution test: Clarify that edge labels reflect the distillation model's
causal *judgment*, not verified causation, and discuss the implications for
simulation reliability.
```

---

## Reviewer 3

*Reviewer 3 places greatest weight on interdisciplinary readership interest and
readability for nonspecialists.*

### Overall assessment

The paper is clearly written for a systems audience and the "notebook vs
simulator" framing is an effective communicative device. The
hippocampus-inspired design will attract cross-disciplinary curiosity.
However, the manuscript currently asks nonspecialist readers to invest in a
biological analogy (glutamate / GABA, LTP / LTD, sharp-wave ripples) without
demonstrating that the analogy is load-bearing — i.e., that it produces
functional behaviour a simpler design would not. For an interdisciplinary
readership, the gap between the richness of the biological framing and the
thinness of the evidence that the biology *matters* is the central readability
and broad-interest barrier. The paper also under-explains several
domain-specific terms that a nonspecialist would need defined.

### Who would be interested in the results, and why

Beyond the immediate agent-memory community, the paper could interest
computational neuroscientists who model hippocampal dynamics, researchers in
model-based RL interested in lightweight world models, and practitioners
building long-horizon agent systems. The compaction-survival framing has
immediate practical appeal to anyone deploying agents that run for hours or
days. The interdisciplinary appeal is currently latent — it would be activated
by showing the biological analogy yields engineering value.

### Major strengths

- The "notebook vs simulator" conceptual framing (§5.2) is the single most
  effective passage for communicating the contribution to a broad audience.
- The Pearl causal-hierarchy mapping (§2.3) gives nonspecialist readers a
  principled scaffold for understanding what the system can and cannot claim.
- The honest limitations section (§5.3) is well calibrated in places (e.g.,
  explicitly disavowing Rung-3 counterfactuals, acknowledging Chinese
  tokenization).
- The typed-edge table (§3.1) is a compact, readable summary that a
  nonspecialist can parse.

### Major concerns

**R3-M1 — [mechanism-evidence]**
Claim pointer: Biological memory relies on both glutamatergic excitation (LTP)
and GABAergic inhibition (LTD); causal-memory implements both.
Evidence pointer: §1 (principle 2); §3.1; §5.1.
Concern: The biological analogy is presented as motivation ("This is not
metaphor: the inhibitory pathway changes the dynamics of spreading activation";
§1 principle 2), but the manuscript never shows a result where the inhibitory
pathway changes the dynamics in a way that matters for any task. A
nonspecialist reader attracted by the neuroscience framing will look for
evidence that the GABA analogue does something — and will not find it. This
creates a readability problem: the richest framing device in the paper is
unsupported by evidence, which risks alienating the exact interdisciplinary
readers it aims to attract.
Resolution test: Provide a concrete worked example or benchmark result where
`prevented` edges change the activation landscape and the retrieved results
(e.g., the risk-averse planning scenario from §5.1 with and without
inhibition), presented in a way a nonspecialist can follow.

**R3-M2 — [writing-clarity]**
Claim pointer: The system is built on three principles, with seven edge types,
CSR spreading activation, SWR consolidation, RRF fusion, BM25 + semantic
retrieval, and 13 MCP tools.
Evidence pointer: §1; §3.1–3.5.
Concern: The manuscript uses a high density of unexpanded acronyms and
domain-specific terms: CSR, SpMV, SWR (sharp-wave ripple), LTP, LTD, DG
(dentate gyrus), SimHash, RRF, BM25, Okapi, MCP, ONNX. Some are parenthetically
defined on first use (e.g., "Compressed Sparse Row"); others are not (e.g.,
SpMV is never expanded; SWR is defined only by its full name once). For a
paper claiming interdisciplinary appeal, this density is a barrier. A
nonspecialist reader would struggle with §3.2–3.4.
Resolution test: Expand all acronyms on first use; consider a brief glossary
or a simplified schematic (the editorial criteria encourage a "simple
schematic summarizing the main conclusion" for nonspecialist readers). No
figures are supplied; a single architecture diagram would substantially aid
comprehension.

**R3-M3 — [claim-moderation]**
Claim pointer: causal-memory is "an explicit world model" enabling
"decision-time what-if queries that no notebook-style memory system can
answer."
Evidence pointer: Abstract; §1; §5.2; §6.
Concern: For a broad readership, "world model" and "what-if queries" imply a
validated predictive capability. The paper implements `intervention_query` and
`counterfactual_query` but provides no evidence that their predictions are
accurate. A nonspecialist reader will reasonably infer that the system can
reliably predict outcomes — an inference the paper does not support and
partially disclaims (§5.3 limitation 6). The gap between the framing
("predicts what will happen") and the evidence (no prediction benchmark) is
a readability and claim-moderation problem.
Resolution test: Either add even a small-scale validation of prediction
accuracy, or consistently qualify the world-model language as "a structural
framework for causal reasoning whose predictive accuracy is not yet
benchmarked."

**R3-M4 — [writing-clarity]**
Claim pointer: The paper is a complete, self-contained systems paper.
Evidence pointer: Full draft.
Concern: No figures are supplied. For a systems paper, the absence of an
architecture diagram, a data-flow diagram, or a visual depiction of the
spreading-activation process is unusual and hinders comprehension. The seven
edge types, the dual CSR matrices, and the consolidation pipeline are
described in prose and tables but never visualized. Tables 1–6 convey results
but not system structure.
Resolution test: Add at least (a) an architecture diagram showing the
typed-edge graph, the spreading-activation engine, and the MCP tool layer;
(b) a small visual example of excitatory and inhibitory activation spreading
from a seed; (c) ideally a schematic summarizing the main contribution for
nonspecialist readers.

**R3-M5 — [figures-and-tables]**
Claim pointer: Tables 1–6 report the experimental results.
Evidence pointer: §4.1–4.5.
Concern: The tables report point estimates without uncertainty (except the
single *p*-value in §4.1). Table 5 (agent ablation) reports "67 %" and "33 %"
for repeat-mistake rate with no denominator visible — the reader cannot tell
whether these are 4/6 vs 2/6 or larger counts. Table 1 reports recall
percentages for "10 probe questions per conversation" across "10
conversations" but does not show the per-conversation distribution, making the
"cliff" between *k*=2 and *k*=3 hard to assess for robustness.
Resolution test: Add denominators or raw counts to Table 5; show per-conversation
recall variance for Table 1 (e.g., as a small figure or a ± range); add
confidence intervals where feasible.

### Technical failings that need to be addressed before the case is established

- The biological analogy is the paper's most distinctive interdisciplinary
  hook but is not shown to be functionally load-bearing (R3-M1). For the
  broad readership the paper targets, this is not optional — it is the
  difference between "evocative metaphor" and "demonstrated principle."

- The world-model claim (R3-M3) creates an expectation of validated prediction
  that the paper does not meet.

### Assessment against Nature-style criteria

- **Originality.** The interdisciplinary framing (hippocampus-inspired agent
  memory with inhibitory dynamics) is original and appealing; see Reviewer 2
  for concerns about whether the novelty is functionally demonstrated.
- **Scientific importance.** The compaction-survival problem and the
  notebook-to-simulator shift are potentially important framings; their
  current impact is limited by the unvalidated contributions.
- **Interdisciplinary readership.** Latent but not yet activated. The
  neuro-symbolic framing could attract computational neuroscientists and
  model-based RL researchers, but only if the biological analogy is shown to
  matter (R3-M1).
- **Technical soundness.** See Reviewers 1 and 2; from a readability
  perspective, the gap between framing richness and evidence thinness is the
  primary concern.
- **Readability for nonspecialists.** Moderate. The conceptual framing is
  accessible, but acronym density (R3-M2), missing figures (R3-M4), and tables
  without uncertainty or denominators (R3-M5) impede comprehension.

### Recommendation posture

**Promising framing, but the interdisciplinary and readability case needs
work.** The "notebook vs simulator" narrative and the hippocampus analogy are
strong hooks, but the paper does not yet deliver the evidence that would make
these hooks compelling to a broad audience. Resolving R3-M1 (show the analogy
matters), R3-M2 (expand acronyms), R3-M4 (add figures), and R3-M5 (add
denominators / uncertainty to tables) would substantially improve the paper's
accessibility and broad appeal.

### Substantive concerns (traceable)

```
R3-M1 — [mechanism-evidence]
Claim pointer: The inhibitory pathway changes the dynamics of spreading
activation, enabling risk-averse retrieval.
Evidence pointer: §1 principle 2; §5.1.
Concern: The biological analogy is presented as functionally consequential
but no result shows inhibition changing outcomes; the richest
interdisciplinary hook is unsupported.
Resolution test: Show a worked example or benchmark where `prevented` edges
change retrieved results, presented accessibly.

R3-M2 — [writing-clarity]
Claim pointer: The system design section (§3) is self-contained.
Evidence pointer: §3.1–3.5.
Concern: High density of unexpanded acronyms (SpMV, SWR, DG, RRF, MCP, ONNX);
some never expanded.
Resolution test: Expand all acronyms on first use; add a glossary or schematic.

R3-M3 — [claim-moderation]
Claim pointer: The causal graph "predicts what will happen" (world model).
Evidence pointer: Abstract; §5.2.
Concern: "Predicts" implies validated predictive accuracy; no benchmark
exists. Nonspecialists will over-infer capability.
Resolution test: Add prediction validation or consistently qualify the
world-model language.

R3-M4 — [writing-clarity]
Claim pointer: Full systems paper.
Evidence pointer: Entire draft.
Concern: No figures supplied — no architecture diagram, data-flow diagram,
or activation-spreading visualization.
Resolution test: Add an architecture diagram and a small activation-spreading
example; consider a nonspecialist schematic.

R3-M5 — [figures-and-tables]
Claim pointer: Tables 1–6 report the results.
Evidence pointer: §4.1–4.5.
Concern: No uncertainty, no denominators (Table 5), no per-conversation
variance (Table 1); the "cliff" and the "halves" claims are hard to assess.
Resolution test: Add denominators, variance, and confidence intervals.

R3-m1 — [writing-clarity]
Claim pointer: The excitatory/inhibitory duality is positioned via Pearl's
causal hierarchy.
Evidence pointer: §2.3.
Concern: Rung-2.5 ("empirical counterfactual") is non-standard terminology
invented by the authors; it may confuse readers familiar with Pearl's
three-rung hierarchy.
Resolution test: Clarify that Rung-2.5 is the authors' label for a
contrastive/empirical subset, not a standard Pearl rung, or use established
terminology.
```

---

## Cross-review synthesis

### Consensus strengths

Three reviewers converge on these strengths:

1. **The compaction-survival problem is a valuable framing.** All three
   reviewers independently identify the formalization of "causal information
   degrades under iterative compaction" as a useful conceptual contribution,
   regardless of the specific solution. *(Issue key: compaction-framing)*

2. **The typed-edge unification on a single graph is a clean, implementable
   design.** The edge-type table and the single-substrate architecture are
   praised as a coherent and reproducible system design. *(Issue key:
   typed-edge-design)*

3. **Honest self-assessment in parts.** The gain-attribution decomposition
   (Table 2), the dual-judge analysis (Table 3), and the explicit disavowal
   of Rung-3 counterfactuals are commended as mature scholarly practice.

### Consensus technical risks

The following are raised by **at least two** reviewer reports and constitute
the core risks:

1. **The inhibitory mechanism — the paper's headline innovation — is never
   ablated or functionally validated.** *(Issue key: inhibition-unvalidated)*
   Raised by R1 (R1-m3), R2 (R2-M1), and R3 (R3-M1). This is the most
   consequential consensus concern: the single most distinctive claim in the
   paper has no experimental support showing it changes any outcome.

2. **Forward simulation (`intervention_query`) is a headline contribution
   with no benchmark.** *(Issue key: simulation-unbenchmarked)* Raised by R2
   (R2-M2) and R3 (R3-M3), and acknowledged by the paper itself (§5.3
   limitation 6). Promoting an unvalidated feature to a listed contribution
   inflates the originality and significance claims.

3. **The compaction experiment does not isolate the architecture's
   contribution.** *(Issue key: compaction-control-tautological)* Raised by
   R1 (R1-M1) and R2 (implicitly via R2-M3, the HeLa-Mem baseline gap). The
   control (external SQLite table) survives by construction; the experiment
   shows that external storage is immune to compaction, not that the
   causal-graph architecture is necessary.

4. **Headline numbers lack uncertainty and denominators.** *(Issue key:
   missing-uncertainty)* Raised by R1 (R1-m1) and R3 (R3-M5). Only one result
   in the entire paper reports a statistical test; Tables 2, 4, 5, and 6
   report point estimates only, and Table 5 omits raw counts.

5. **The closest competitor (HeLa-Mem) is not compared experimentally.**
   *(Issue key: no-heLa-head-to-head)* Raised by R2 (R2-M3) and noted by R1
   (R1-M5 context). The originality case depends on surpassing HeLa-Mem, but
   the comparison is conceptual only.

### Where emphasis differs across reviewers

- **Systems-performance gap (R1 only).** Reviewer 1 uniquely emphasizes the
  total absence of latency, throughput, memory-footprint, and scaling
  evaluation, which is particularly critical for an OSDI/ATC target venue.
  Reviewers 2 and 3 do not raise this, reflecting their emphasis on
  originality and readability rather than systems characterization. This is a
  weighting difference, not a factual disagreement — all reviewers would
  expect systems numbers at a systems venue.

- **Overfitting risk on LoCoMo (R1 only).** Reviewer 1 uniquely flags the
  absence of a train/test split on the optimization matrix. Reviewers 2 and 3
  focus on the claim calibration rather than the evaluation protocol.

- **"First" novelty claim scope (R2 only).** Reviewer 2 uniquely questions
  whether the "first to implement negative activation" claim is properly
  scoped against the broader spreading-activation literature.

- **Acronym density and missing figures (R3 only).** Reviewer 3 uniquely
  foregrounds readability barriers — unexpanded acronyms and the complete
  absence of figures — as impediments to the interdisciplinary readership the
  paper targets.

- **Extrapolated ~89 % figure (R2 only).** Reviewer 2 uniquely identifies that
  the abstract's ~89 % does not correspond to any tabled result and appears
  to be an unshown extrapolation. This is preserved as a consequential
  single-reviewer concern.

- **Censored-sample issue in v4-pro result (R1 only).** Reviewer 1 uniquely
  identifies that the 82.3 % non-error accuracy (§4.5) is computed on a
  non-random subset excluding 23 % timeouts, undermining the "model gap not
  architecture gap" inference.

### Broad-interest / significance readout

The paper targets a genuinely timely problem (long-running agent memory under
compaction) and proposes an architecturally distinctive solution. The
broad-interest potential is real but **not yet established**: the two most
distinctive contributions (inhibitory dynamics, forward simulation) are
unvalidated, and the biological analogy — the primary interdisciplinary hook
— is not shown to be functionally load-bearing. If the inhibitory ablation and
a forward-simulation benchmark (even small-scale) are added, the paper moves
from "interesting architecture with claims exceeding evidence" to "a
demonstrated advance with cross-disciplinary appeal." Without them, the
significance rests on the compaction-survival and LoCoMo results, which are
useful but narrower than the framing implies.

Per the editorial criteria, the question of whether the work reaches a
sufficiently broad interdisciplinary readership is ultimately an editorial
judgment; the reviewers' assessment is that the *potential* is there but the
*evidence* is not yet.

### Most important issues to resolve before a strong case is established

Ranked by consensus weight and impact on the headline claims:

1. **Ablate the inhibitory mechanism** (R1-m3 / R2-M1 / R3-M1). Show that
   `prevented` edges change at least one benchmark or agent-behaviour outcome.
   This is the single highest-priority fix — it converts the paper's most
   distinctive claim from assertion to evidence.

2. **Validate or demote forward simulation** (R2-M2 / R3-M3). Either run a
   prediction-accuracy benchmark for `intervention_query` on held-out
   decisions, or consistently demote it from a headline contribution to an
   implemented-but-unvalidated feature across abstract, introduction, and
   conclusion.

3. **Fix the compaction control** (R1-M1). Add a flat-external-store control
   at equal retrieval budget, or reframe the claim as "externalized causal
   storage survives compaction" and separately justify the graph architecture.

4. **Add systems-performance evaluation** (R1-M3). For an OSDI/ATC submission,
   latency, throughput, footprint, and scaling data are expected; their
   absence is a venue-specific blocking gap.

5. **Calibrate the ~89 % figure and the "halves" claim** (R2-M4 / R1-M2).
   Replace the extrapolated ~89 % with the measured 84.1 % (or run the
   configuration that produces ~89 %); scale the agent ablation beyond 6
   tasks / 1 seed and report uncertainty.

6. **Add a head-to-head with HeLa-Mem or an excitatory-only baseline**
   (R2-M3). The originality case depends on it.

7. **Add uncertainty and denominators to all tables** (R1-m1 / R3-M5).

8. **Add figures** (R3-M4). An architecture diagram and an
   activation-spreading example would substantially improve accessibility.

---

## Risk / unsupported claims

- **"~89 % at frontier-compatible judge caliber" (Abstract, §1).** Not
  present in any table; Table 3 shows 84.1 % under the mem0-compatible judge.
  The ~89 % appears to be an unshown extrapolation. **Unsupported as
  reported.** AUTHOR_INPUT_NEEDED: show the configuration and result, or
  replace with the measured number.

- **"The first memory system to implement negative activation spread"
  (Abstract, §1, §6).** Novelty claim is unscoped against the broader
  spreading-activation and constraint-satisfaction literature where inhibitory
  dynamics exist. **Not assessable** without a broader literature survey;
  recommend scoping to "agent-memory systems."

- **"The inhibitory pathway changes the dynamics of spreading activation"
  (§1 principle 2).** Asserted as functional, not metaphorical, but no result
  shows inhibition changing any outcome. **Unsupported by the supplied
  evidence.**

- **"Forward traversal performs simulation — no benchmark currently measures
  this" (§1).** The paper offers `intervention_query` as a contribution but
  provides no prediction-accuracy validation. **Not assessable** — the feature
  is implemented but its contribution is unvalidated; §5.3 limitation 6
  confirms this.

- **"Causal memory halves the repeat-mistake rate (67 % → 33 %)" (Abstract,
  §1, §4.4).** Based on 6 tasks, 1 seed. **Weakly supported** — the direction
  is plausible but the magnitude is not robust at this sample size.

- **"The remaining gap is attributable to answerer model quality, not memory
  architecture" (§4.5).** The v4-pro evidence (82.3 %) is computed on a
  censored subset excluding 23 % timeouts. **Not assessable** as a clean
  attribution due to sample censoring.

- **"Statistically indistinguishable from zero-compaction performance
  (*p* < 0.01)" (§4.1).** The only statistical test in the paper. **Located
  and reported**, but its applicability to the broader claims (which lack any
  uncertainty quantification) is limited.

- **Reproducibility of proprietary-model results.** deepseek-chat,
  glm-4-plus, deepseek-v4-pro, and ZhiPu embedding-3 are proprietary; exact
  reproducibility depends on model version stability. The open-source code
  and result files (referenced at `benches/*/results/`) partially mitigate
  this, but the per-question JSONL files and distillation prompts are **not
  supplied** in the review package. AUTHOR_INPUT_NEEDED for full
  reproducibility assessment.

- **No figures supplied.** The review cannot assess visual presentation,
  architecture diagrams, or schematic quality. **Not assessable from provided
  material.**

---

*End of reviewer assessment package.*
