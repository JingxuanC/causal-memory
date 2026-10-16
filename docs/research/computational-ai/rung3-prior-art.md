# Rung-3 Prior Art — Counterfactual Reasoning for Agent Memory (2026 survey)

> Web survey conducted 2026-09-01 (GitHub live search + Semantic Scholar +
> direct abstract fetches; arXiv API unreachable from this network — every
> item below was verified against its repo or abstract page).
>
> Context: `counterfactual_query` (v0.9) is an honestly-labeled contrastive
> comparison, NOT a Pearl Rung-3 counterfactual. This survey maps what
> exists between us and the real third rung, and which parts already have
> algorithms or engineering we can borrow.

## Why the gap is real: Executable Counterfactuals

**Vashishtha et al. (2025) — "Executable Counterfactuals"
([arXiv:2510.01539](https://arxiv.org/abs/2510.01539), code:
[AniketVashishtha/Executable_Counterfactuals](https://github.com/AniketVashishtha/Executable_Counterfactuals))**

Operationalizes counterfactual reasoning as code/math problems that force
all three Pearl steps: **abduction → intervention → prediction**. Key
findings:

- Existing LLM counterfactual evaluations largely **skip the abduction
  step**, which reduces them to interventional (Rung-2) reasoning and
  overestimates capability.
- SOTA models (o4-mini, Claude-4-Sonnet) drop **25–40% accuracy** moving
  from interventional to true counterfactual problems.
- RL on counterfactual code problems induces transferable counterfactual
  skill; SFT does not (gains in-domain, loses out-of-domain).

**Design relevance**: this is external, measured confirmation of our own
code comment (`ops.rs`: "NOT a Pearl Rung-3 SCM counterfactual") — the
missing ingredient is *abduction*, i.e. conditioning on the recorded world
state of the actual episode. It also supplies a synthetic-data recipe for
building a Rung-3 eval set.

## The five-rungette ladder (what we can actually build)

Full design: [docs/design/counterfactual-rung3.md](../../design/counterfactual-rung3.md).
Each phase has prior art:

### Abduction substrate (context snapshots)

No external dependency needed — schema work. The survey found no agent
memory system that records a structured pre-decision context fingerprint;
this is greenfield (and the prerequisite Pearl's math demands:
P(Y_x | X=x', Y=y', C) needs a recorded C).

### Natural-experiment graph (fork edges)

- **FlowScript → anneal-memory**
  ([phillipclapham/flowscript](https://github.com/phillipclapham/flowscript),
  archived, evolved into
  [anneal-memory](https://github.com/phillipclapham/anneal-memory)):
  a 21-marker notation where `||` (alternatives) is a first-class marker,
  with `why` / `whatIf` / `counterfactual` typed queries over reasoning
  graphs (TS SDK + MCP server). Validates "alternatives as first-class
  graph structure" for agent memory.
- **counterfactual-research**
  ([osobot-ai/counterfactual-research](https://github.com/osobot-ai/counterfactual-research),
  adapted from karpathy/autoresearch): an autonomous-research harness
  asking exactly our question — *does structured counterfactual memory
  build a better world model faster than episodic memory?* Its environment
  reveals outcomes for **chosen AND unchosen** options ("natural
  counterfactuals"), and its headline metric `counterfactual_accuracy`
  (how well the agent predicts outcomes of paths not taken) is precisely
  the calibration metric our prediction ledger needs.

### Statistical micro-SCM (abduction-action-prediction over the recorded graph)

- **DeepSCM** ([biomedia-mira/deepscm](https://github.com/biomedia-mira/deepscm),
  297★): Pawlowski et al., *Deep Structural Causal Models for Tractable
  Counterfactual Inference* — normalizing flows + VAE doing full
  abduction-action-prediction. The algorithmic blueprint for when our
  fork graph gets dense.
- **DoWhy GCM counterfactuals**
  ([user guide](https://github.com/py-why/dowhy/blob/main/docs/source/user_guide/causal_tasks/what_if/counterfactuals.rst),
  medical case notebook): the mainstream causal library's AAP API — the
  interface semantics worth mirroring (fit → abduce → intervene → predict).
- **cfid** ([santikka/cfid](https://github.com/santikka/cfid), R/CRAN):
  implements Shpitser-style **counterfactual identifiability** (ID*/shortcut):
  decides whether a counterfactual query is computable from a given causal
  graph + data at all. This is the engine for our "not identifiable →
  degrade to contrastive" gate.
- **counterfactual-identifiability-bench**
  ([zackary-masri](https://github.com/zackary-masri/counterfactual-identifiability-bench)):
  10k executable counterfactual problems labeled by identifiability
  (**point / set / not-identifiable**). The label taxonomy to adopt for
  R3 answers.

### LLM world-model simulation (Gerstenberg counterfactual simulation)

Gerstenberg et al. (2021) — already in
[research-backdrop](../research-backdrop.zh.md); no canonical code, but
`reconstruct_lesson --calibrate=N` already implements the
multi-reconstruction-agreement scoring a simulator needs. Executable
Counterfactuals (above) is the benchmark to beat.

### Executable replay (the one true SCM counterfactual available to agents)

- **stepback** ([thehalleyyoung/stepback](https://github.com/thehalleyyoung/stepback)):
  a time-travel debugger for agent runs. Records a run as a signed
  (HMAC-SHA256 + Ed25519) `.sb` trace of content-addressed steps;
  a substitution (tool output, system prompt, routing branch) replays
  through the dependency graph with **dirty-set propagation** — only
  downstream steps re-execute — plus bisection to find where the
  counterfactual run diverges. This is a working engine for closed-world
  counterfactuals (builds, tests, sandboxes): rerun history *literally*.
  Our `recall_audit` table is already the embryo of a step trace.
- **llm-counterfactual-replay**
  ([zizhao-hu](https://github.com/zizhao-hu/llm-counterfactual-replay)):
  conversation fork + counterfactual memory edits, then replay — the
  lightweight variant.

## Adjacent (worth knowing, not core)

- **restore-counterfactual**
  ([megagonlabs](https://github.com/megagonlabs/restore-counterfactual),
  CBW@COLM 2026): *What Eviction Destroys* — restore-counterfactual
  audit of forgetting in agent memory. Directly applicable to auditing
  our own GC / sleep eviction policy.
- **regret-eval** ([4Lou4](https://github.com/4Lou4/regret-eval)): offline
  evaluation of never-executed policies (posterior oracle, regret,
  temporal splits) — statistics reference for the prediction ledger.
- **Counterfactual-Memory-Auditing-for-Long-Term-Agents**
  ([TylorChan](https://github.com/TylorChan/Counterfactual-Memory-Auditing-for-Long-Term-Agents)):
  auditing whether agents *causally use* the memory they retrieve.
- **DreamArbiter**: lucid-dreaming cycles that generate counterfactuals
  during "sleep" — a cousin of our SWR consolidation loop.
- **alibi** (SeldonIO, 2.6k★): counterfactual *explanations* /
  algorithmic recourse (CFGP, CEM). Different sense of "counterfactual"
  (input perturbation for XAI), but recourse-style search is a plausible
  future advisor for "what should I have done differently".

## Competitive read

Nobody combines **recorded abduction context + fork graph +
identifiability-gated estimation/simulation + a prediction ledger + closed-
world executable replay** in one agent memory system. Adjacent projects
each own one corner (stepback: replay; anneal-memory: alternatives in the
graph; counterfactual-research: the eval protocol). The window is open
but visibly closing — this survey found four 2026-dated repos whose
descriptions overlap our design doc's phases.
