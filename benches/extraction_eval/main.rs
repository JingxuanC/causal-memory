//! Extraction calibration harness (hardening §3.1) — answers "提取的因果
//! 关系准确吗？" by closing the loop the refuter/intervention calibrations
//! leave open: those plant edges directly into the store, bypassing the
//! natural-language → remember/Distiller → causal-edge step. This harness
//! measures exactly that step against known ground truth.
//!
//! Methodology:
//! 1. Generate synthetic agent episodes (seeded SplitMix64, deterministic)
//!    in five classes with known ground-truth relation:
//!      caused / prevented / enabled — true typed edges
//!      no_effect  — outcome has an explicit other cause (false-edge probe)
//!      confounded — a common cause drives both action and outcome; the
//!                   correct extraction is `co_occurrence` (PR #28), NOT
//!                   `caused` — regression gauge for the §1.3 finding that
//!                   confounded episodes were 100% DANGER-overclaimed.
//!    Text rendering: templates + slot pools per domain, bilingual (~50/50
//!    zh/en), 2-6 turns per episode with seeded filler turns.
//! 2. Inject: each episode's transcript goes through the REAL `remember`
//!    path (Distiller + LLM) into a fresh tempdir store.
//! 3. Score: extracted edges are matched back to episodes by normalized
//!    containment / char-bigram Jaccard; report extraction recall, the
//!    relation confusion matrix (confounded→caused overclaim rate is the
//!    headline), polarity accuracy, and the no_effect false-edge rate.
//!
//! Subcommands:
//!   gen      --episodes N --seed S --out FILE [--narrate-llm]
//!                       zero-LLM episode JSONL generation (narrate-llm is
//!                       the optional LLM paraphrase + verification pass)
//!   selftest [--episodes N --seed S]
//!                       zero-LLM full-pipeline check: a mock extractor with
//!                       known injected error rates writes through the real
//!                       store; asserts the scorer recovers those rates
//!   run      --episodes N --seed S [--out DIR]
//!                       live run — needs DEEPSEEK_API_KEY (or
//!                       CAUSAL_MEMORY_LLM_API + CAUSAL_MEMORY_LLM_KEY)

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

// ─── Seeded PRNG (SplitMix64 — same discipline as refuter_calibration) ────

struct Rng(u64);
impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed)
    }
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E3779B97F4A7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
        z ^ (z >> 31)
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
    fn chance(&mut self, pct: u64) -> bool {
        self.next() % 100 < pct
    }
    fn pick<'a, T>(&mut self, xs: &'a [T]) -> &'a T {
        &xs[self.below(xs.len())]
    }
}

// ─── Episode model ─────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum EpisodeKind {
    Caused,
    Prevented,
    Enabled,
    NoEffect,
    Confounded,
}

impl EpisodeKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Caused => "caused",
            Self::Prevented => "prevented",
            Self::Enabled => "enabled",
            Self::NoEffect => "no_effect",
            Self::Confounded => "confounded",
        }
    }
    /// The relation a correct extractor should write; "none" for no_effect
    /// (no causal edge should exist between decision and outcome at all).
    fn expected_relation(self) -> &'static str {
        match self {
            Self::Caused => "caused",
            Self::Prevented => "prevented",
            Self::Enabled => "enabled",
            Self::NoEffect => "none",
            Self::Confounded => "co_occurrence",
        }
    }
    /// Class mix: caused 30 / prevented 15 / enabled 15 / no_effect 20 /
    /// confounded 20 (the two error probes together weigh as much as the
    /// positive classes — false edges are the expensive failure mode).
    fn sample(rng: &mut Rng) -> Self {
        match rng.below(100) {
            0..=29 => Self::Caused,
            30..=44 => Self::Prevented,
            45..=59 => Self::Enabled,
            60..=79 => Self::NoEffect,
            _ => Self::Confounded,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Episode {
    id: usize,
    kind: EpisodeKind,
    lang: String,
    domain: String,
    /// Ground-truth decision / outcome phrases (canonical, pre-narration).
    decision: String,
    outcome: String,
    expected_relation: String,
    expected_polarity: String,
    /// (speaker, text) transcript — what `remember` ingests.
    turns: Vec<(String, String)>,
}

// ─── Domain slot pools (bilingual) ─────────────────────────────────────────

struct DomainPool {
    name: &'static str,
    /// Decision phrases ("added a Redis cache layer" / "给接口加了 Redis 缓存").
    acts: &'static [&'static str],
    /// Good outcome phrases (improvement observed).
    good: &'static [&'static str],
    /// Bad outcome phrases (something broke).
    bad: &'static [&'static str],
    /// Explicit other causes (for no_effect).
    other: &'static [&'static str],
    /// Common causes (for confounded).
    cc: &'static [&'static str],
    /// Service/target names that keep decision texts unique per episode
    /// (identical decision text would trigger v9 chunk reuse + the
    /// contradiction short-circuit, corrupting ground truth).
    targets: &'static [&'static str],
}

const EN_POOL: &[DomainPool] = &[
    DomainPool {
        name: "caching",
        acts: &[
            "added a Redis cache layer",
            "enabled CDN caching",
            "introduced a local in-memory cache",
        ],
        good: &[
            "p99 latency dropped from 800ms to 90ms",
            "the timeout errors disappeared",
        ],
        bad: &[
            "a cache stampede took the API down",
            "stale entries served wrong prices",
        ],
        other: &[
            "the upstream provider fixed their gateway",
            "traffic simply dropped after the sale ended",
        ],
        cc: &[
            "the lead ordered both the cache work and a cluster scale-up",
            "the infra team upgraded the nodes the same day",
        ],
        targets: &["checkout", "search", "profile", "orders", "gateway", "feed"],
    },
    DomainPool {
        name: "concurrency",
        acts: &[
            "added a mutex around the shared counter",
            "switched to channel-based ownership",
            "introduced a bounded worker pool",
        ],
        good: &[
            "the race condition stopped appearing",
            "throughput doubled under load",
        ],
        bad: &[
            "a deadlock froze the service",
            "workers starved and queues piled up",
        ],
        other: &[
            "the client stopped sending duplicate requests",
            "a flaky dependency was replaced by another team",
        ],
        cc: &[
            "the on-call also raised the instance count that night",
            "a platform-wide runtime upgrade shipped the same week",
        ],
        targets: &["ingest", "billing", "notify", "sync", "audit", "relay"],
    },
    DomainPool {
        name: "deployment",
        acts: &[
            "set up canary deployments",
            "added a pre-deploy test gate",
            "enabled blue-green rollout",
        ],
        good: &[
            "bad releases stopped reaching all users",
            "rollbacks took seconds instead of hours",
        ],
        bad: &[
            "a broken release hit every user at once",
            "the deploy corrupted active user sessions",
        ],
        other: &[
            "the faulty commit was reverted by its author",
            "the incident window coincided with a traffic lull",
        ],
        cc: &[
            "the release freeze was also lifted that day",
            "QA doubled their regression suite the same sprint",
        ],
        targets: &["web", "api", "worker", "cron", "admin", "mobile"],
    },
    DomainPool {
        name: "database",
        acts: &[
            "added a connection pool",
            "created an index on the events table",
            "moved reads to a replica",
        ],
        good: &[
            "query latency fell below 50ms",
            "the connection-exhaustion alerts stopped",
        ],
        bad: &[
            "the migration locked the table for minutes",
            "replica lag served stale data",
        ],
        other: &[
            "the DBA vacuumed the bloated tables",
            "a hot customer finished their bulk export",
        ],
        cc: &[
            "the database was also upgraded to a newer version that week",
            "storage was migrated to faster disks in parallel",
        ],
        targets: &["events", "users", "orders", "metrics", "sessions", "logs"],
    },
    DomainPool {
        name: "debugging",
        acts: &[
            "added structured logging",
            "instrumented tracing spans",
            "set up a metrics dashboard",
        ],
        good: &[
            "the next incident was root-caused in minutes",
            "the retry storm finally became visible",
        ],
        bad: &[
            "log volume tripled the bill",
            "the agent overhead slowed requests noticeably",
        ],
        other: &[
            "the vendor published the root-cause analysis",
            "a coincidental deploy fixed the underlying bug",
        ],
        cc: &[
            "an external consultant reviewed the system at the same time",
            "the team also started weekly incident reviews then",
        ],
        targets: &["core", "edge", "batch", "stream", "auth", "cache"],
    },
];

const ZH_POOL: &[DomainPool] = &[
    DomainPool {
        name: "caching",
        acts: &[
            "给接口加了 Redis 缓存",
            "打开了 CDN 缓存",
            "引入了本地内存缓存",
        ],
        good: &["p99 延迟从 800ms 降到 90ms", "超时错误彻底消失了"],
        bad: &["缓存雪崩把 API 打挂了", "旧缓存返回了错误价格"],
        other: &["上游服务商修好了他们的网关", "大促结束流量自然回落了"],
        cc: &[
            "leader 同时要求加缓存和扩容机器",
            "运维同一天升级了节点配置",
        ],
        targets: &["结算", "搜索", "主页", "订单", "网关", "推荐"],
    },
    DomainPool {
        name: "concurrency",
        acts: &[
            "给共享计数器加了互斥锁",
            "改成了 channel 所有权模型",
            "引入了有界 worker 池",
        ],
        good: &["竞态条件不再出现", "高负载下吞吐翻倍"],
        bad: &["死锁把服务冻住了", "worker 饿死、队列越堆越高"],
        other: &["客户端那边停止了重复请求", "有问题的依赖被别的组换掉了"],
        cc: &["值班同学当晚同时扩了实例数", "平台同一周升级了运行时"],
        targets: &["采集", "计费", "通知", "同步", "审计", "转发"],
    },
    DomainPool {
        name: "deployment",
        acts: &["上线了金丝雀发布", "加了部署前测试门禁", "启用了蓝绿发布"],
        good: &["坏版本不再全量影响用户", "回滚从几小时变成几秒"],
        bad: &["故障版本一次性影响了所有用户", "发布把在线用户会话弄坏了"],
        other: &[
            "有问题的提交被作者自己 revert 了",
            "事故窗口正好撞上流量低谷",
        ],
        cc: &[
            "当天同时解除了发布冻结",
            "QA 同一 sprint 把回归套件翻了一倍",
        ],
        targets: &["前端", "接口", "任务", "定时", "后台", "推送"],
    },
    DomainPool {
        name: "database",
        acts: &["加了连接池", "给 events 表建了索引", "读请求切到了从库"],
        good: &["查询延迟降到 50ms 以下", "连接耗尽的告警消失了"],
        bad: &["迁移锁表好几分钟", "从库延迟导致读到旧数据"],
        other: &["DBA 清理了膨胀的表", "大客户正好跑完了批量导出"],
        cc: &["数据库同一周也升级了新版本", "存储并行迁移到了更快的盘"],
        targets: &["事件", "用户", "订单", "指标", "会话", "日志"],
    },
    DomainPool {
        name: "debugging",
        acts: &["补上了结构化日志", "接入了链路追踪", "搭了指标看板"],
        good: &["下次事故几分钟就定位了根因", "重试风暴终于能看见了"],
        bad: &["日志量让账单翻了三倍", "agent 开销明显拖慢了请求"],
        other: &["供应商发布了根因分析报告", "一次巧合的发布修好了底层 bug"],
        cc: &["外部顾问同期做了系统评审", "团队那时也开始做每周事故复盘"],
        targets: &["核心", "边缘", "批处理", "流式", "鉴权", "缓存"],
    },
];

const EN_FILLERS: &[&str] = &[
    "By the way, tomorrow's standup moves half an hour earlier.",
    "The CI runners were upgraded yesterday, builds feel faster.",
    "Lunch is on the team lead today.",
    "Reminder: fill in the quarterly review form by Friday.",
];
const ZH_FILLERS: &[&str] = &[
    "对了，明天站会提前半小时。",
    "CI  runner 昨天升级了，构建快了不少。",
    "今天午饭 lead 请客。",
    "提醒：周五前填季度复盘表。",
];

// ─── Episode generation ────────────────────────────────────────────────────

/// Render one episode: pick domain/kind/lang, then lay out a 2-6 turn
/// transcript whose causal semantics match the ground-truth label.
fn gen_episode(rng: &mut Rng, id: usize) -> Episode {
    let zh = rng.chance(50);
    let pool = if zh { ZH_POOL } else { EN_POOL };
    let domain = rng.pick(pool);
    let kind = EpisodeKind::sample(rng);
    let act = rng.pick(domain.acts);
    let target = rng.pick(domain.targets);
    // The target suffix keeps decision texts unique across episodes —
    // identical decision chunks would trigger contradiction supersession.
    let (decision, good, bad, other, cc) = if zh {
        (
            format!("{act}（{target} 服务）"),
            *rng.pick(domain.good),
            *rng.pick(domain.bad),
            *rng.pick(domain.other),
            *rng.pick(domain.cc),
        )
    } else {
        (
            format!("{act} for the {target} service"),
            *rng.pick(domain.good),
            *rng.pick(domain.bad),
            *rng.pick(domain.other),
            *rng.pick(domain.cc),
        )
    };

    // Negative-caused episodes: the decision itself backfires.
    let negative = kind == EpisodeKind::Caused && rng.chance(40);
    let (outcome, polarity) = if negative {
        (bad.to_string(), "negative")
    } else {
        (good.to_string(), "positive")
    };

    let mut turns: Vec<(String, String)> = if zh {
        zh_transcript(rng, kind, &decision, &good, &bad, &other, &cc, negative)
    } else {
        en_transcript(rng, kind, &decision, &good, &bad, &other, &cc, negative)
    };
    // Pad to 2-6 turns with seeded unrelated filler (retrieval noise).
    let fillers = if zh { ZH_FILLERS } else { EN_FILLERS };
    let pad = rng.below(3); // 0-2 filler turns
    for _ in 0..pad {
        if turns.len() >= 6 {
            break;
        }
        let pos = rng.below(turns.len() + 1);
        let speaker = if rng.chance(50) { "user" } else { "assistant" };
        turns.insert(pos, (speaker.into(), rng.pick(fillers).to_string()));
    }

    Episode {
        id,
        kind,
        lang: if zh { "zh".into() } else { "en".into() },
        domain: domain.name.into(),
        decision,
        outcome,
        expected_relation: kind.expected_relation().into(),
        expected_polarity: polarity.into(),
        turns,
    }
}

#[allow(clippy::too_many_arguments)]
fn en_transcript(
    rng: &mut Rng,
    kind: EpisodeKind,
    dec: &str,
    good: &str,
    bad: &str,
    other: &str,
    cc: &str,
    negative: bool,
) -> Vec<(String, String)> {
    let u = |s: String| ("user".to_string(), s);
    let a = |s: String| ("assistant".to_string(), s);
    match (kind, negative, rng.below(2)) {
        (EpisodeKind::Caused, false, 0) => vec![
            u("The API has been timing out since the last release.".into()),
            a(format!("I {dec} to cut the load.")),
            u(format!("That fixed it — {good}.")),
        ],
        (EpisodeKind::Caused, false, _) => vec![
            a(format!("Root cause was the missing capacity; I {dec}.")),
            u(format!("Confirmed: {good}.")),
        ],
        (EpisodeKind::Caused, true, _) => vec![
            a(format!("I {dec} right before the deploy.")),
            u(format!("That backfired — {bad}.")),
            a("Rolling back now; my change directly caused it.".into()),
        ],
        (EpisodeKind::Prevented, _, _) => vec![
            u("We're exposed — one bad build could hit every user.".into()),
            a(format!("I {dec} as a guard.")),
            u(format!(
                "Yesterday's broken build never reached production; {good}."
            )),
        ],
        (EpisodeKind::Enabled, _, _) => vec![
            a(format!("I {dec}.")),
            u(format!("That unblocked us — {good}.")),
            a("Right, this simply wasn't possible before.".into()),
        ],
        (EpisodeKind::NoEffect, _, _) => vec![
            a(format!("I {dec} anyway.")),
            u(format!(
                "{good} — but that's because {other}; our change had nothing to do with it."
            )),
        ],
        (EpisodeKind::Confounded, _, _) => vec![
            u("Why did things suddenly improve?".into()),
            a(format!("I {dec}. Note: {cc}.")),
            u(format!(
                "So the improvement ({good}) may come from either — attribution is unclear."
            )),
        ],
    }
}

#[allow(clippy::too_many_arguments)]
fn zh_transcript(
    rng: &mut Rng,
    kind: EpisodeKind,
    dec: &str,
    good: &str,
    bad: &str,
    other: &str,
    cc: &str,
    negative: bool,
) -> Vec<(String, String)> {
    let u = |s: String| ("user".to_string(), s);
    let a = |s: String| ("assistant".to_string(), s);
    match (kind, negative, rng.below(2)) {
        (EpisodeKind::Caused, false, 0) => vec![
            u("上次发布后接口一直超时。".into()),
            a(format!("我{dec}，先降下来负载。")),
            u(format!("修好了——{good}。")),
        ],
        (EpisodeKind::Caused, false, _) => vec![
            a(format!("根因是容量不够，我{dec}。")),
            u(format!("确认了：{good}。")),
        ],
        (EpisodeKind::Caused, true, _) => vec![
            a(format!("我在发布前{dec}。")),
            u(format!("结果出事了——{bad}。")),
            a("正在回滚；这次是我的改动直接导致的。".into()),
        ],
        (EpisodeKind::Prevented, _, _) => vec![
            u("我们现在很裸奔——一个坏版本可能打到所有用户。".into()),
            a(format!("我{dec}兜底。")),
            u(format!("昨天那个坏构建根本没到生产；{good}。")),
        ],
        (EpisodeKind::Enabled, _, _) => vec![
            a(format!("我{dec}。")),
            u(format!("这把我们盘活了——{good}。")),
            a("对，之前根本做不到。".into()),
        ],
        (EpisodeKind::NoEffect, _, _) => vec![
            a(format!("我还是{dec}。")),
            u(format!("{good}——但那是因为{other}，跟我们的改动没关系。")),
        ],
        (EpisodeKind::Confounded, _, _) => vec![
            u("怎么突然变好了？".into()),
            a(format!("我{dec}。注意：{cc}。")),
            u(format!("所以变好（{good}）可能是两者之一——归因说不清。")),
        ],
    }
}

fn generate_episodes(n: usize, seed: u64) -> Vec<Episode> {
    let mut rng = Rng::new(seed);
    let mut used: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        // Decision texts must be unique across episodes: identical text hits
        // v9 chunk reuse + the contradiction short-circuit, which would
        // silently invalidate another episode's ground-truth edge. The slot
        // pool is finite, so after a few re-rolls fall back to a "round"
        // distinguisher (natural for recurring work).
        let mut ep = gen_episode(&mut rng, i);
        let mut round = 2;
        while !used.insert(ep.decision.clone()) {
            if round > 6 {
                ep.decision = if ep.lang == "zh" {
                    format!("{}，第 {round} 轮", ep.decision)
                } else {
                    format!("{} (round {round})", ep.decision)
                };
                round += 1;
            } else {
                ep = gen_episode(&mut rng, i);
                round += 1;
            }
        }
        out.push(ep);
    }
    out
}

// ─── Matching (extracted edge ↔ ground-truth episode) ─────────────────────

/// Normalize for matching: lowercase, alphanumeric+CJK only, single spaces.
fn norm(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut ws = false;
    for c in s.chars() {
        if c.is_alphanumeric() {
            out.push(c.to_ascii_lowercase());
            ws = false;
        } else if !ws {
            out.push(' ');
            ws = true;
        }
    }
    out.trim().to_string()
}

/// Char-bigram Jaccard — language-agnostic (word tokens fail on Chinese).
fn bigram_jaccard(a: &str, b: &str) -> f64 {
    use std::collections::HashSet;
    let grams = |s: &str| -> HashSet<(char, char)> {
        let cs: Vec<char> = s.chars().collect();
        cs.windows(2).map(|w| (w[0], w[1])).collect()
    };
    let (ga, gb) = (grams(a), grams(b));
    if ga.is_empty() || gb.is_empty() {
        return 0.0;
    }
    let inter = ga.intersection(&gb).count() as f64;
    inter / (ga.len() + gb.len()) as f64 * 2.0 // Dice coefficient
}

/// An extracted decision matches an episode when one normalized text
/// contains the other, or the Dice bigram similarity clears 0.5 (LLM
/// paraphrases keep most content words).
fn decisions_match(extracted: &str, truth: &str) -> bool {
    let (a, b) = (norm(extracted), norm(truth));
    if a.is_empty() || b.is_empty() {
        return false;
    }
    a.contains(&b) || b.contains(&a) || bigram_jaccard(&a, &b) >= 0.5
}

// ─── Scoring ───────────────────────────────────────────────────────────────

/// Per-episode scoring row (also the results-JSONL line shape).
#[derive(Debug, Clone, Serialize)]
struct ScoreRow {
    episode_id: usize,
    kind: String,
    lang: String,
    domain: String,
    expected_relation: String,
    matched: bool,
    /// (edge_id, extracted decision text, extracted relation) — first match.
    extracted: Option<(i64, String, String)>,
    /// "correct_relation" | "wrong_relation" | "missed" | "false_edge" |
    /// "clean" (no_effect with no spurious edge).
    verdict: String,
    polarity: Option<(String, String, bool)>, // (expected, got, correct)
}

fn score_episodes(
    episodes: &[Episode],
    edges: &[causal_memory::store::CausalEntry],
) -> Vec<ScoreRow> {
    // Global greedy assignment: episodes share act phrases across targets,
    // so many (episode, edge) pairs clear the fuzzy threshold. Score every
    // candidate pair (decision similarity + outcome tie-break) and claim
    // best-first — each edge attributes to at most one episode, each
    // episode takes its best unclaimed edge. Without this, a dropped
    // episode can "steal" a sibling's edge and read as a false positive.
    let mut candidates: Vec<(usize, usize, f64)> = Vec::new();
    for (ei, ep) in episodes.iter().enumerate() {
        for (gi, e) in edges.iter().enumerate() {
            if decisions_match(&e.decision_text, &ep.decision) {
                let score = bigram_jaccard(&norm(&e.decision_text), &norm(&ep.decision))
                    + 0.5 * bigram_jaccard(&norm(&e.outcome_text), &norm(&ep.outcome));
                candidates.push((ei, gi, score));
            }
        }
    }
    candidates.sort_by(|a, b| b.2.partial_cmp(&a.2).unwrap_or(std::cmp::Ordering::Equal));
    let mut episode_claim: Vec<Option<usize>> = vec![None; episodes.len()];
    let mut edge_claimed = vec![false; edges.len()];
    for (ei, gi, _) in candidates {
        if episode_claim[ei].is_none() && !edge_claimed[gi] {
            episode_claim[ei] = Some(gi);
            edge_claimed[gi] = true;
        }
    }

    episodes
        .iter()
        .enumerate()
        .map(|(ei, ep)| {
            let hit = episode_claim[ei].map(|gi| &edges[gi]);
            let (matched, extracted, verdict, polarity) = match hit {
                Some(e) => {
                    let v = if ep.expected_relation == "none" {
                        "false_edge"
                    } else if e.relation == ep.expected_relation {
                        "correct_relation"
                    } else {
                        "wrong_relation"
                    };
                    let pol = e.outcome_polarity.clone().map(|got| {
                        (
                            ep.expected_polarity.clone(),
                            got.clone(),
                            got == ep.expected_polarity,
                        )
                    });
                    (
                        true,
                        Some((e.edge_id, e.decision_text.clone(), e.relation.clone())),
                        v,
                        pol,
                    )
                }
                None => (
                    false,
                    None,
                    if ep.expected_relation == "none" {
                        "clean"
                    } else {
                        "missed"
                    },
                    None,
                ),
            };
            ScoreRow {
                episode_id: ep.id,
                kind: ep.kind.as_str().into(),
                lang: ep.lang.clone(),
                domain: ep.domain.clone(),
                expected_relation: ep.expected_relation.clone(),
                matched,
                extracted,
                verdict: verdict.into(),
                polarity,
            }
        })
        .collect()
}

#[derive(Default)]
struct Report {
    total: usize,
    matched: usize, // episodes with ≥1 extracted edge (any relation)
    confusion: HashMap<(String, String), usize>,
    verdicts: HashMap<String, usize>,
    polarity_correct: usize,
    polarity_scored: usize,
}

impl Report {
    fn tally(rows: &[ScoreRow]) -> Self {
        let mut r = Report::default();
        for row in rows {
            r.total += 1;
            *r.verdicts.entry(row.verdict.clone()).or_default() += 1;
            if row.matched {
                r.matched += 1;
                let extracted = row
                    .extracted
                    .as_ref()
                    .map(|(_, _, rel)| rel.clone())
                    .unwrap_or_default();
                *r.confusion
                    .entry((row.expected_relation.clone(), extracted))
                    .or_default() += 1;
            }
            if let Some((_, _, ok)) = &row.polarity {
                r.polarity_scored += 1;
                r.polarity_correct += usize::from(*ok);
            }
        }
        r
    }

    fn vcount(&self, v: &str) -> usize {
        self.verdicts.get(v).copied().unwrap_or(0)
    }

    /// Headline metric: share of matched confounded episodes extracted as
    /// `caused` (the §1.3 overclaim failure mode, baseline 100%).
    fn confounded_overclaim(&self) -> (usize, usize) {
        let mut over = 0;
        let mut total = 0;
        for ((exp, got), n) in &self.confusion {
            if exp == "co_occurrence" {
                total += n;
                if got == "caused" {
                    over += n;
                }
            }
        }
        (over, total)
    }

    fn render(&self) -> String {
        let pct = |n: usize, d: usize| {
            if d == 0 {
                "—".to_string()
            } else {
                format!("{:.1}%", n as f64 * 100.0 / d as f64)
            }
        };
        let causal_eps = self.total - self.vcount("clean") - self.vcount("false_edge");
        let (over, cf_total) = self.confounded_overclaim();
        let mut s = String::new();
        s.push_str(&format!("episodes:            {}\n", self.total));
        s.push_str(&format!(
            "extraction recall:   {} (matched {}/{} causal episodes)\n",
            pct(
                self.vcount("correct_relation") + self.vcount("wrong_relation"),
                causal_eps
            ),
            self.vcount("correct_relation") + self.vcount("wrong_relation"),
            causal_eps
        ));
        s.push_str(&format!(
            "relation accuracy:   {} ({} of matched)\n",
            pct(
                self.vcount("correct_relation"),
                self.vcount("correct_relation") + self.vcount("wrong_relation")
            ),
            self.vcount("correct_relation")
        ));
        s.push_str(&format!(
            "confounded→caused overclaim: {} ({}/{})   ← §1.3 baseline was 100%\n",
            pct(over, cf_total),
            over,
            cf_total
        ));
        s.push_str(&format!(
            "no_effect false-edge rate:   {} ({} episodes)\n",
            pct(
                self.vcount("false_edge"),
                self.vcount("false_edge") + self.vcount("clean")
            ),
            self.vcount("false_edge") + self.vcount("clean")
        ));
        s.push_str(&format!(
            "polarity accuracy:   {} ({}/{} scored)\n",
            pct(self.polarity_correct, self.polarity_scored),
            self.polarity_correct,
            self.polarity_scored
        ));
        s.push_str("\nrelation confusion (expected × extracted):\n");
        let mut keys: Vec<_> = self.confusion.keys().collect();
        keys.sort();
        for k in keys {
            s.push_str(&format!(
                "  {:>14} → {:<14} {}\n",
                k.0, k.1, self.confusion[k]
            ));
        }
        s
    }
}

// ─── Selftest: mock extractor with known injected error rates ─────────────

/// Injected error knobs (percent). The selftest asserts the scorer recovers
/// rates close to these — validating generation → injection → matching →
/// scoring end-to-end without any LLM.
const MISS_PCT: u64 = 15;
const REL_ERR_PCT: u64 = 10;
const CONFOUND_OVERCLAIM_PCT: u64 = 40;
const NOEFFECT_SPURIOUS_PCT: u64 = 20;
const PARAPHRASE_PCT: u64 = 30;

/// What the mock extractor decided to do with an episode (injected truth).
#[derive(Debug, Clone, PartialEq)]
enum MockAct {
    Drop,
    Produce { relation: &'static str },
}

fn mock_extract(rng: &mut Rng, ep: &Episode) -> MockAct {
    if ep.kind == EpisodeKind::NoEffect {
        // A bad extractor sometimes invents a causal edge anyway.
        return if rng.chance(NOEFFECT_SPURIOUS_PCT) {
            MockAct::Produce { relation: "caused" }
        } else {
            MockAct::Drop
        };
    }
    if rng.chance(MISS_PCT) {
        return MockAct::Drop;
    }
    let truth = ep.kind.expected_relation();
    let relation = if ep.kind == EpisodeKind::Confounded && rng.chance(CONFOUND_OVERCLAIM_PCT) {
        "caused" // the classic overclaim
    } else if rng.chance(REL_ERR_PCT) {
        const RELS: &[&str] = &["caused", "enabled", "prevented", "co_occurrence"];
        loop {
            let r = RELS[rng.below(RELS.len())];
            if r != truth {
                break r;
            }
        }
    } else {
        truth
    };
    MockAct::Produce { relation }
}

fn selftest(episodes: usize, seed: u64) -> Result<()> {
    let eps = generate_episodes(episodes, seed);
    let store = causal_memory::store::CausalStore::open_in_memory()?;

    // Mock-extract and inject through the REAL store write path.
    let mut rng = Rng::new(seed ^ 0xDEAD);
    let mut injected: Vec<MockAct> = Vec::new();
    let mut injected_conf_over = 0usize;
    let mut injected_conf_total = 0usize;
    let mut injected_false_edges = 0usize;
    for ep in &eps {
        let act = mock_extract(&mut rng, ep);
        if let MockAct::Produce { relation } = &act {
            // Paraphrase noise: wrap the decision so containment matching
            // (not just equality) is exercised.
            let dec = if rng.chance(PARAPHRASE_PCT) {
                if ep.lang == "zh" {
                    format!("决定：{}", ep.decision)
                } else {
                    format!("decided to {}", ep.decision)
                }
            } else {
                ep.decision.clone()
            };
            store.record_decision_full(
                &dec,
                &ep.outcome,
                relation,
                Some(&ep.domain),
                0.7,
                "rule",
                1_700_000_000 + ep.id as i64,
                Some(&ep.expected_polarity),
                None,
                None,
            )?;
            if ep.kind == EpisodeKind::Confounded {
                injected_conf_total += 1;
                injected_conf_over += usize::from(*relation == "caused");
            }
            if ep.kind == EpisodeKind::NoEffect {
                injected_false_edges += 1;
            }
        }
        injected.push(act);
    }

    // Score the store's actual contents.
    let edges = store.all_valid_edges()?;
    let rows = score_episodes(&eps, &edges);
    let report = Report::tally(&rows);

    // Per-episode verdicts must reflect the injected truth exactly
    // (deterministic seed ⇒ no tolerance needed).
    for (ep, (act, row)) in eps.iter().zip(injected.iter().zip(rows.iter())) {
        let want = match act {
            MockAct::Drop => {
                if ep.kind == EpisodeKind::NoEffect {
                    "clean"
                } else {
                    "missed"
                }
            }
            MockAct::Produce { relation } => {
                if ep.kind == EpisodeKind::NoEffect {
                    "false_edge"
                } else if *relation == ep.expected_relation {
                    "correct_relation"
                } else {
                    "wrong_relation"
                }
            }
        };
        assert_eq!(
            &row.verdict,
            want,
            "episode {} ({}) scored {want}, got {}",
            ep.id,
            ep.kind.as_str(),
            row.verdict
        );
    }

    // Headline rates must land near the injected knobs.
    let (over, cf_total) = report.confounded_overclaim();
    assert_eq!(
        cf_total, injected_conf_total,
        "all produced confounded edges must be matched back"
    );
    assert_eq!(
        over, injected_conf_over,
        "scorer must recover the injected overclaim count exactly"
    );
    assert_eq!(
        report.vcount("false_edge"),
        injected_false_edges,
        "scorer must recover the injected false-edge count exactly"
    );
    assert!(
        report.polarity_scored > 0 && report.polarity_correct == report.polarity_scored,
        "mock injects no polarity errors: scorer must read polarity back 1:1"
    );

    println!("selftest OK — scorer recovered injected error rates exactly:");
    print!("{}", report.render());
    println!(
        "(injected knobs: miss={MISS_PCT}% rel_err={REL_ERR_PCT}% confounded_overclaim={CONFOUND_OVERCLAIM_PCT}% noeffect_spurious={NOEFFECT_SPURIOUS_PCT}%)"
    );
    Ok(())
}

// ─── Live run (real Distiller path) ─────────────────────────────────────────

fn episode_text(ep: &Episode) -> String {
    ep.turns
        .iter()
        .map(|(sp, t)| format!("{sp}: {t}"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn live_store_dir(episode_id: usize) -> PathBuf {
    std::env::temp_dir().join(format!(
        "extraction_eval_{}_{episode_id}",
        std::process::id()
    ))
}

async fn run_live(episodes: usize, seed: u64, out_root: &Path) -> Result<()> {
    if causal_memory::distill::Distiller::from_env().is_none() {
        bail!(
            "live run needs an LLM: set DEEPSEEK_API_KEY, or \
             CAUSAL_MEMORY_LLM_API + CAUSAL_MEMORY_LLM_KEY. \
             (zero-LLM alternatives: `gen` and `selftest`)"
        );
    }
    let eps = generate_episodes(episodes, seed);
    let run_dir = bench_common::run_dir(out_root);
    std::fs::create_dir_all(&run_dir)?;
    let jsonl_path = run_dir.with_extension("jsonl");
    let mut jsonl = String::new();
    let mut all_rows: Vec<ScoreRow> = Vec::new();

    for (i, ep) in eps.iter().enumerate() {
        let dir = live_store_dir(ep.id);
        std::fs::create_dir_all(&dir)?;
        // Fresh store per episode: attribution stays unambiguous and the
        // contradiction short-circuit can't cross episodes.
        let memory = causal_memory::memory::Memory::open(dir.join("causal.db"))?;
        memory.remember(&episode_text(ep), Some("2026-09-30"));
        let edges = memory.store().all_valid_edges()?;
        let rows = score_episodes(std::slice::from_ref(ep), &edges);
        for row in &rows {
            jsonl.push_str(&serde_json::to_string(row)?);
            jsonl.push('\n');
        }
        all_rows.extend(rows);
        drop(memory);
        let _ = std::fs::remove_dir_all(&dir);
        if (i + 1) % 10 == 0 || i + 1 == eps.len() {
            eprintln!("  {}/{} episodes distilled", i + 1, eps.len());
        }
    }

    std::fs::write(&jsonl_path, &jsonl)?;
    let report = Report::tally(&all_rows);
    let summary = format!(
        "# Extraction calibration — live run\n\nepisodes={episodes} seed={seed}\n\n```\n{}```\n",
        report.render()
    );
    std::fs::write(bench_common::summary_path(&run_dir), &summary)?;
    println!("{summary}");
    println!("rows: {}", jsonl_path.display());
    Ok(())
}

// ─── Optional LLM narration for gen (paraphrase + ground-truth check) ─────

async fn narrate_llm(cfg: &bench_common::LlmConfig, eps: &mut [Episode]) -> Result<()> {
    for ep in eps.iter_mut() {
        let text = episode_text(ep);
        let rewritten = bench_common::chat(
            cfg,
            "You rewrite synthetic agent-work conversations to sound natural. \
             Keep every factual claim and the causal meaning unchanged. \
             Keep the speaker: text line format and the original language.",
            &format!("Rewrite this conversation:\n\n{text}"),
            800,
        )
        .await
        .with_context(|| format!("narrate episode {}", ep.id))?;
        // Verification pass (causal_eval narrate discipline): the rewrite
        // must preserve decision / relation / outcome, else keep template.
        let check = format!(
            "Conversation:\n{rewritten}\n\nDoes it still say that \"{}\" relates to \"{}\" \
             as: {}? Answer yes or no.",
            ep.decision, ep.outcome, ep.expected_relation
        );
        let (verdict, _) = bench_common::judge(cfg, "You verify text semantics.", &check, 8).await;
        if verdict == bench_common::Verdict::Yes {
            ep.turns = rewritten
                .lines()
                .filter(|l| !l.trim().is_empty())
                .map(|l| match l.split_once(':') {
                    Some((sp, t)) => (sp.trim().to_string(), t.trim().to_string()),
                    None => ("user".to_string(), l.trim().to_string()),
                })
                .collect();
        }
    }
    Ok(())
}

// ─── CLI ───────────────────────────────────────────────────────────────────

const USAGE: &str = "causal-memory-extraction-eval <subcommand> [opts]
  gen      --episodes N --seed S --out FILE [--narrate-llm]
  selftest [--episodes N] [--seed S]     (zero-LLM pipeline check)
  run      --episodes N --seed S [--out DIR]   (needs DEEPSEEK_API_KEY)";

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(cmd) = args.first() else {
        eprintln!("{USAGE}");
        std::process::exit(2);
    };
    let get = |flag: &str| -> Option<String> {
        args.iter()
            .position(|a| a == flag)
            .and_then(|i| args.get(i + 1))
            .cloned()
    };
    let episodes = get("--episodes")
        .and_then(|v| v.parse().ok())
        .unwrap_or(100);
    let seed = get("--seed").and_then(|v| v.parse().ok()).unwrap_or(42);

    match cmd.as_str() {
        "gen" => {
            let mut eps = generate_episodes(episodes, seed);
            if args.iter().any(|a| a == "--narrate-llm") {
                let cfg = bench_common::LlmConfig::from_env()
                    .context("--narrate-llm needs DEEPSEEK_API_KEY (or CAUSAL_MEMORY_LLM_KEY)")?;
                narrate_llm(&cfg, &mut eps).await?;
            }
            let out = get("--out").unwrap_or_else(|| "episodes.jsonl".into());
            let mut buf = String::new();
            for ep in &eps {
                buf.push_str(&serde_json::to_string(ep)?);
                buf.push('\n');
            }
            std::fs::write(&out, &buf)?;
            let mut by_kind: HashMap<&str, usize> = HashMap::new();
            for ep in &eps {
                *by_kind.entry(ep.kind.as_str()).or_default() += 1;
            }
            let zh = eps.iter().filter(|e| e.lang == "zh").count();
            println!("wrote {} episodes to {out}", eps.len());
            let mut kinds: Vec<_> = by_kind.iter().collect();
            kinds.sort();
            for (k, n) in kinds {
                println!("  {k:<12} {n}");
            }
            println!("  zh {zh} / en {}", eps.len() - zh);
            Ok(())
        }
        "selftest" => selftest(episodes, seed),
        "run" => {
            let out = get("--out")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("benches/extraction_eval/results"));
            run_live(episodes, seed, &out).await
        }
        other => {
            eprintln!("unknown subcommand: {other}\n{USAGE}");
            std::process::exit(2);
        }
    }
}
