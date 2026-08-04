# Generate promo.html from template
import textwrap

html = r'''<!DOCTYPE html>
<html lang="zh-CN"><head><meta charset="UTF-8"><meta name="viewport" content="width=device-width,initial-scale=1,maximum-scale=1,user-scalable=no">
<title>causal-memory · 从源码拆解到因果记忆引擎</title>
<style>
*{margin:0;padding:0;box-sizing:border-box;-webkit-tap-highlight-color:transparent}
:root{--bg:#f8f9fa;--text:#1a1a2e;--muted:#6c757d;--dim:#adb5bd;--gold:#c8860b;--gold-l:#f5b400;--gold-bg:rgba(245,180,0,.07);--green:#0ca678;--red:#e03131;--blue:#1971c2;--purple:#6741d9;--card:#fff;--border:#e9ecef;--shadow:0 2px 12px rgba(0,0,0,.05);--mono:'SF Mono','JetBrains Mono',Menlo,monospace}
body{background:var(--bg);color:var(--text);font-family:-apple-system,BlinkMacSystemFont,'PingFang SC',sans-serif;-webkit-font-smoothing:antialiased;line-height:1.65}
.container{max-width:920px;margin:0 auto;padding:0 28px}
.hero{padding:80px 28px 56px;text-align:center;background:linear-gradient(180deg,var(--gold-bg),transparent)}
.hero-badge{display:inline-flex;gap:6px;padding:6px 16px;border-radius:20px;background:var(--gold-bg);border:1px solid rgba(200,134,11,.2);font-size:12px;color:var(--gold);letter-spacing:2px;margin-bottom:24px}
.hero-title{font-size:clamp(44px,13vw,88px);font-weight:900;letter-spacing:-3px;line-height:.95;margin-bottom:16px}
.hero-title .gradient{background:linear-gradient(135deg,var(--text) 20%,var(--gold));-webkit-background-clip:text;-webkit-text-fill-color:transparent}
.hero-sub{font-size:clamp(15px,4vw,20px);color:var(--muted);margin-bottom:28px;line-height:1.5}
.hero-stats{display:flex;flex-wrap:wrap;gap:16px;justify-content:center;margin-bottom:20px}
.hero-stat{text-align:center}
.hero-stat-num{font-size:28px;font-weight:900;font-family:var(--mono);color:var(--gold)}
.hero-stat-lbl{font-size:11px;color:var(--muted)}
.hero-tags{display:flex;flex-wrap:wrap;gap:8px;justify-content:center}
.hero-tag{font-size:11px;padding:5px 14px;border-radius:16px;background:var(--card);border:1px solid var(--border);color:var(--muted);box-shadow:var(--shadow)}
.section{padding:56px 0}
.section-num{font-family:var(--mono);font-size:14px;font-weight:700;color:var(--gold);opacity:.4;margin-bottom:4px}
.section-kicker{font-size:12px;letter-spacing:3px;text-transform:uppercase;color:var(--gold);margin-bottom:6px;font-weight:700}
.section-title{font-size:clamp(26px,6.5vw,44px);font-weight:800;letter-spacing:-1px;line-height:1.2;margin-bottom:24px}
.section-title .hl{color:var(--gold)}.section-title .hl-red{color:var(--red)}.section-title .hl-green{color:var(--green)}
.prose{font-size:17px;line-height:1.8;margin-bottom:16px}.prose strong{font-weight:700}
.card{background:var(--card);border:1px solid var(--border);border-radius:18px;padding:28px 32px;margin-bottom:14px;box-shadow:var(--shadow)}
.card-row{display:flex;gap:16px;align-items:flex-start}
.card-num{font-family:var(--mono);font-size:28px;font-weight:900;color:var(--gold);opacity:.2;flex-shrink:0;line-height:1}
.card-icon{font-size:24px;flex-shrink:0;line-height:1.3}
.card-body h4{font-size:17px;margin-bottom:6px}.card-body p{font-size:15px;color:var(--muted);line-height:1.65}
.card-body .src{font-size:12px;color:var(--dim);margin-top:8px;font-family:var(--mono)}
.tag{display:inline-block;font-size:11px;padding:3px 8px;border-radius:4px;background:rgba(12,166,120,.1);color:var(--green);margin-top:8px;font-weight:600}
.tag-red{background:rgba(224,49,49,.1)!important;color:var(--red)!important}
.insight-card{background:linear-gradient(135deg,var(--gold-bg),rgba(255,255,255,.5));border:1px solid rgba(200,134,11,.15);border-radius:18px;padding:28px 32px;margin-bottom:14px}
.insight-card .label{font-size:12px;color:var(--gold);letter-spacing:2px;text-transform:uppercase;margin-bottom:10px;font-weight:700}
.insight-card .quote{font-size:19px;font-weight:700;line-height:1.5}
.insight-card .desc{font-size:15px;color:var(--muted);margin-top:10px;line-height:1.65}
.divider{display:flex;align-items:center;gap:12px;margin:40px 0;color:var(--dim)}.divider::before,.divider::after{content:'';flex:1;height:1px;background:var(--border)}.divider span{font-size:13px;letter-spacing:2px}
.arch{background:var(--card);border:1px solid var(--border);border-radius:20px;padding:28px;box-shadow:var(--shadow);margin-bottom:14px}
.arch-layer-title{font-size:14px;font-weight:700;color:var(--gold);text-align:center;padding:12px;background:var(--gold-bg);border-radius:10px;margin-bottom:10px}
.arch-row{display:flex;gap:8px;margin-bottom:8px}
.arch-block{flex:1;text-align:center;padding:16px 8px;border-radius:10px;border:1px solid var(--border);background:var(--bg)}
.arch-block .icon{font-size:24px;display:block;margin-bottom:4px}.arch-block .name{font-size:13px;font-weight:700;display:block}.arch-block .desc{font-size:11px;color:var(--muted);display:block;margin-top:2px;line-height:1.3}
.arch-block.hl{border-color:rgba(200,134,11,.3);background:var(--gold-bg)}.arch-arrow{text-align:center;color:var(--dim);font-size:16px;margin:4px 0}
.edge-item{display:flex;align-items:center;gap:14px;padding:16px 20px;margin-bottom:8px;background:var(--card);border:1px solid var(--border);border-radius:14px;box-shadow:var(--shadow)}
.edge-item.hl{border-color:rgba(224,49,49,.2);background:rgba(224,49,49,.02)}
.edge-dot{width:14px;height:14px;border-radius:50%;flex-shrink:0}.edge-name{font-size:16px;font-weight:700;min-width:90px}.edge-desc{font-size:14px;color:var(--muted);flex:1}.edge-val{font-family:var(--mono);font-size:16px;font-weight:700}
.bench-block{margin-bottom:20px}.bench-head{font-size:14px;font-weight:700;margin-bottom:8px}.bench-row{margin-bottom:8px}
.bench-lbl{display:flex;justify-content:space-between;margin-bottom:4px}.bench-lbl .name{font-size:14px}.bench-lbl .name.muted{color:var(--muted)}.bench-lbl .score{font-family:var(--mono);font-size:14px;font-weight:700}
.bench-track{height:32px;background:var(--bg);border-radius:8px;overflow:hidden;border:1px solid var(--border)}
.bench-fill{height:100%;border-radius:8px;width:0;transition:width 1.5s cubic-bezier(.16,1,.3,1);display:flex;align-items:center;justify-content:flex-end;padding-right:10px;font-size:12px;font-weight:800}
.bench-fill.gold{background:linear-gradient(90deg,rgba(245,180,0,.3),var(--gold-l));color:#000}.bench-fill.gray{background:var(--border);color:var(--muted)}
.stat-row{display:flex;gap:12px;margin-top:20px}.stat-card{flex:1;background:var(--card);border:1px solid var(--border);border-radius:14px;padding:20px 10px;text-align:center;box-shadow:var(--shadow)}
.stat-num{font-size:28px;font-weight:900;font-family:var(--mono)}.stat-lbl{font-size:11px;color:var(--muted);margin-top:4px;line-height:1.3}
.code-window{background:#1e1e2e;border-radius:16px;overflow:hidden;margin-bottom:14px;box-shadow:0 8px 30px rgba(0,0,0,.12)}
.code-header{display:flex;align-items:center;gap:6px;padding:12px 16px;border-bottom:1px solid rgba(255,255,255,.1)}
.code-dot{width:10px;height:10px;border-radius:50%}.code-dot.r{background:#ff5f57}.code-dot.y{background:#febc2e}.code-dot.g{background:#28c840}
.code-title{font-size:12px;color:#6c7086;margin-left:8px;font-family:var(--mono)}
.code-body{padding:18px;font-family:var(--mono);font-size:14px;line-height:1.8;color:#cdd6f4}
.code-body .c{color:#6c7086}.code-body .k{color:#cba6f7}.code-body .s{color:#a6e3a1}.code-body .f{color:#89b4fa}.code-body .w{color:#f38ba8}.code-body .g{color:#f9e2af}
.plant-table{width:100%;border-collapse:collapse;margin:14px 0;font-size:14px}
.plant-table th{background:var(--gold-bg);color:var(--gold);font-size:12px;padding:10px;text-align:left}
.plant-table td{padding:10px;border-bottom:1px solid var(--border)}.plant-table td:first-child{font-weight:600}
.timeline{position:relative;padding-left:24px;margin:20px 0}
.timeline::before{content:'';position:absolute;left:6px;top:0;bottom:0;width:2px;background:var(--border)}
.timeline-item{position:relative;margin-bottom:20px}
.timeline-item::before{content:'';position:absolute;left:-22px;top:6px;width:10px;height:10px;border-radius:50%;background:var(--gold);border:2px solid var(--bg)}
.timeline-phase{font-size:12px;color:var(--gold);font-weight:700;letter-spacing:1px;text-transform:uppercase}
.timeline-title{font-size:16px;font-weight:700;margin:4px 0}
.timeline-desc{font-size:14px;color:var(--muted);line-height:1.6}
.cta{padding:64px 28px 120px;text-align:center;background:linear-gradient(0deg,var(--gold-bg),transparent)}
.cta-title{font-size:clamp(30px,7.5vw,52px);font-weight:900;line-height:1.15;margin-bottom:14px}.cta-title .hl{color:var(--gold)}
.cta-sub{font-size:16px;color:var(--muted);margin-bottom:32px}
.cta-btn{display:inline-block;padding:16px 48px;background:var(--gold);color:#fff;border-radius:14px;font-size:18px;font-weight:800;text-decoration:none;box-shadow:0 8px 30px rgba(200,134,11,.3)}
.cta-gh{font-size:14px;color:var(--muted);font-family:var(--mono);margin-top:16px}
</style></head><body>

<div class="hero">
<div class="hero-badge">⚡ 22,500 行研究 · 从拆解到创造</div>
<h1 class="hero-title"><span class="gradient">causal-memory</span></h1>
<p class="hero-sub">AI Agent 的因果记忆引擎<br>从 42 篇源码拆解到 AGI 哲学推导到 Rust 工程实现</p>
<div class="hero-stats">
<div class="hero-stat"><div class="hero-stat-num">42</div><div class="hero-stat-lbl">源码拆解</div></div>
<div class="hero-stat"><div class="hero-stat-num">17</div><div class="hero-stat-lbl">研究笔记</div></div>
<div class="hero-stat"><div class="hero-stat-num">20+</div><div class="hero-stat-lbl">论文追踪</div></div>
<div class="hero-stat"><div class="hero-stat-num">206</div><div class="hero-stat-lbl">工程测试</div></div>
</div>
<div class="hero-tags">
<span class="hero-tag">🧠 海马体架构</span><span class="hero-tag">⚡ MCP Server</span>
<span class="hero-tag">🔬 Rust 实现</span><span class="hero-tag">🚫 0% 重复犯错</span>
<span class="hero-tag">📊 LongMemEval 71.2%</span></div>
</div>

<div class="container">

<!-- ═══ 01: 拆解 ═══ -->
<div class="section">
<div class="section-num">01</div>
<div class="section-kicker">🔭 起点</div>
<h2 class="section-title">拆了 <span class="hl">42 篇</span><br>Agent 框架源码</h2>
<p class="prose">一切从一个问题开始：<strong>为什么 Agent 这么难做好？</strong></p>
<p class="prose">如果你给 LLM 10 个工具让它循环跑，它<strong>立刻就会出问题</strong>：20 轮后 context 爆了，30 轮后开始重复，50 轮后忘了用户要什么，100 轮后 token 烧了几十块还没做完。</p>
<p class="prose">这不是 LLM 不够聪明。这是<strong>信息论问题</strong>。为了理解它，我们系统性拆解了 7 个框架：</p>
<div class="card"><div class="card-row"><div class="card-icon">📂</div><div class="card-body">
<h4>kimi-code（月之暗面）· 25 篇拆解</h4>
<p>~100K 行 TypeScript。依赖注入架构 + 事件源持久化（wire.jsonl）。25 个子系统逐文件分析：架构 / swarm / goal 状态机 / wire 协议 / context 记忆 / 循环 / skills / MCP / TUI 渲染 / 错误处理 / 插件 / hooks / cron / 测试……</p>
</div></div></div>
<div class="card"><div class="card-row"><div class="card-icon">📂</div><div class="card-body">
<h4>Grok Build（SpaceXAI）· 10 篇拆解</h4>
<p>~1.34M 行 Rust，70+ crates。Actor 架构 + SQLite journaling。分析：架构 / doom loop 检测 / skeptic 对抗面板 / 权限沙箱 / 采样器 / 两阶段压缩 / worktree 池……</p>
</div></div></div>
<div class="card"><div class="card-row"><div class="card-icon">📂</div><div class="card-body">
<h4>Codex · Claude Code · OpenAI ADK · Google Pi</h4>
<p>双阶段记忆 / 7×24 架构 / context 系统 / 多 agent 执行策略 / 压缩退化。加上大厂设计哲学对比（Anthropic / OpenAI / Google / Kimi / 智谱 / DeepSeek）。</p>
</div></div></div>
<p class="prose" style="margin-top:12px">总计 <strong>~14,200 行拆解文档</strong>。这不是走马观花——是逐文件、逐函数、逐设计决策的深度分析。</p>
</div>

<div class="divider"><span>理论建立</span></div>

<!-- ═══ 02: 反熵增 ═══ -->
<div class="section">
<div class="section-num">02</div>
<div class="section-kicker">📐 核心理论</div>
<h2 class="section-title">Agent 的<span class="hl">第二定律</span></h2>
<p class="prose">拆完 42 篇后，核心发现浮出水面：<strong>LLM 是无状态纯函数。它的宇宙 = 当前 context。Context 之外皆不存在。</strong></p>
<p class="prose">每一轮推理前，框架从外部状态库重新组装 context。组装过程必然产生信息损失（Shannon 熵）。这个损失如果不被对抗，会<strong>累积到系统失效</strong>。这就是 Agent 的反熵增——所有工程努力都是在对抗系统的自然退化。</p>
<div class="insight-card"><div class="label">幻觉的第一性定义</div>
<div class="quote">"幻觉不是 bug，是信息不足时的数学必然"</div>
<div class="desc">Context 缺一块，那一块的事实就在模型的宇宙里被抹掉。模型不能说"我不知道"，它只能用训练先验去填补——这个填补动作就是幻觉。三种幻觉：事实性（缺事实）、状态性（缺任务状态）、目标性（缺用户意图）。同一个根因：<strong>context 组装时的信息缺失</strong>。</div></div>
<div class="insight-card"><div class="label">所有"记忆"都是检索 + 注入</div>
<div class="quote">"agent 圈在说'给模型加记忆'。但只要 LLM 还是无状态，'记忆'这个词就是误导"</div>
<div class="desc">物理上发生的只有一件事：每轮推理前，从模型外部的存储里选一部分信息注入 context。Mem0 / Zep / Letta / OpenViking——所有"记忆方案"都是这个动作的不同策略。</div></div>
</div>

<!-- ═══ 03: 植物类比 ═══ -->
<div class="section">
<div class="section-num">03</div>
<div class="section-kicker">🌿 跨域类比</div>
<h2 class="section-title">Agent 与<span class="hl">植物</span></h2>
<p class="prose">一个 Agent 能持续工作而不崩溃，和一株植物能持续生长而不枯萎，在<strong>信息处理层面有结构性相似</strong>（诚实标注：这是修辞工具不是热力学同构——我们专门写了自我反驳来指出这个类比的局限）：</p>
<table class="plant-table">
<tr><th>🌱 植物</th><th>🤖 Agent</th><th>功能角色</th></tr>
<tr><td>☀️ 阳光</td><td>LLM 算力</td><td>信息处理能力的来源</td></tr>
<tr><td>🌿 光合作用</td><td>Compaction / handoff</td><td>把混乱合成秩序（局部熵减）</td></tr>
<tr><td>🌳 根系吸收</td><td>System prompt + 记忆注入</td><td>信息输入（维持结构）</td></tr>
<tr><td>💨 蒸腾作用</td><td>旧消息丢弃 / 折叠</td><td>排出冗余</td></tr>
<tr><td>🧬 DNA 修复</td><td>Skeptic panel + 错误归一化</td><td>错误纠正（对抗变异累积）</td></tr>
<tr><td>🍂 细胞凋亡</td><td>Abort + rewind + kill</td><td>程序性丢弃（牺牲局部保护整体）</td></tr>
<tr><td>🌻 向光性</td><td>Goal continuation driver</td><td>趋向性（朝目标方向）</td></tr>
<tr><td>🛡️ 免疫系统</td><td>Permission + sandbox</td><td>防御系统（对抗外部入侵）</td></tr>
</table>
<p class="prose" style="margin-top:16px"><strong>推论 1</strong>：Agent 需要"代谢"。植物不存储阳光——它持续转化。Agent 不能存一次 context 就用到底——它必须持续压缩、持续验证、持续恢复。<strong>停止代谢 = 崩溃。</strong></p>
<p class="prose"><strong>推论 2</strong>：Agent 的"寿命"由反退化能力决定。一棵 5000 年的狐尾松不是因为基因好——是因为它修复损伤的效率极高。同样，Agent 能跑多久不崩溃，不由 LLM 智商决定，由<strong>反退化措施效率</strong>决定。</p>
<div class="card"><div class="card-row"><div class="card-icon">⚠️</div><div class="card-body">
<h4>诚实的自我反驳</h4>
<p>我们专门写了 insight/08 来反驳自己的框架——五个致命缺陷：热力学偷换概念 / 过度归类 / 修辞非论证 / 解释不了创造 / 不可证伪。植物类比保留是因为它有<strong>沟通价值</strong>，但不作为论证基础。反熵增理论后来用 Shannon 信息论重构，建立在函数签名上，不建立在修辞上。</p>
<div class="src">insight/04-anti-entropy.md · insight/08-self-rebuttal.md · insight/09-stateless-function.md</div>
</div></div></div>
</div>

<div class="divider"><span>哲学探索</span></div>

<!-- ═══ 04: 哲学 ═══ -->
<div class="section">
<div class="section-num">04</div>
<div class="section-kicker">🧠 哲学</div>
<h2 class="section-title">从代码到<span class="hl">意识</span><br>的七重探索</h2>
<p class="prose">拆完源码后，七个工程背后的根本问题浮出水面。我们带着这些问题，走入了 Nature 2026 的论文、Parfit 的形而上学、CMU 的心智理论、Lakoff 的认知语言学。</p>
<div class="insight-card"><div class="label">发现 1 · 机器经验主义（回应"LLM 理解了吗"）</div>
<div class="quote">"LLM 的理解不是人类的理解，也不是纯统计。它是从文本环境中建构出的主观现实"</div>
<div class="desc">Nature 2026（Masi et al.）跳出"理解 vs 模式匹配"的二元对立。Lakoff 的经验主义认知论：人类理解来自身体与环境的交互。LLM 没有身体，但它有文本环境和 Transformer。Agent 的天花板<strong>不取决于 LLM 理解多少，取决于你给它多丰富的环境反馈</strong>——这就是为什么记忆系统如此关键。</div></div>
<div class="insight-card"><div class="label">发现 2 · 忒修斯之船（回应"500 次压缩后还是同一个 Agent 吗"）</div>
<div class="quote">"身份不在于内容相同，在于因果链不断"</div>
<div class="desc">Parfit（1984）：今天的你之所以是你，不是因为细胞和昨天一样（全换了），而是因为今天的记忆因果地产生于昨天。Agent 压缩打断因果链。所以需要一个不被压缩的身份层——记录"我为什么做了这个决策"的因果锚点。这正是 causal-memory 的 caused 边在做的事。</div></div>
<div class="insight-card"><div class="label">发现 3 · 认知望远镜（回应"为什么需要 Agent"）</div>
<div class="quote">"Agent 不是替代品或工具。Agent 是人类的认知望远镜——扩展人类能看到的问题空间"</div>
<div class="desc">LLM 处理过的数据量远超任何人类一生的阅读量。它能发现人类注意不到的关联。就像望远镜不是"替代眼睛"，是让眼睛看到原本看不到的东西。</div></div>
<div class="card"><div class="card-row"><div class="card-icon">⚖️</div><div class="card-body">
<h4>AGI 可达性 · 三层拆解</h4>
<p style="font-size:16px"><strong>L1 工程版 AGI</strong>（7×24 连续运行 + 专家级单领域）：路径清晰，<strong style="color:var(--green)">5-10 年大概率</strong>。瓶颈在记忆基础设施。</p>
<p style="font-size:16px;margin-top:8px"><strong>L2 通用版 AGI</strong>（跨域迁移 + 自主目标）：<strong style="color:var(--gold)">10-30 年不确定</strong>。障碍是理论缺失。</p>
<p style="font-size:16px;margin-top:8px"><strong>L3 真正的智能</strong>（创新、理解、自我意识）：<strong style="color:var(--red)">未知</strong>。</p>
<div class="src">insight/07-philosophy-deep-dive.md · insight/15-agi-feasibility.md</div>
</div></div></div>
</div>

<div class="divider"><span>记忆赛道调研</span></div>

<!-- ═══ 05: 记忆公司 ═══ -->
<div class="section">
<div class="section-num">05</div>
<div class="section-kicker">🔬 市场诊断</div>
<h2 class="section-title">8 家记忆公司<br>全是<span class="hl-red">记事本</span></h2>
<p class="prose">如果"记忆 = 检索 + 注入"是对的，那应该有人专门做这种检索系统。事实是：<strong>这个市场已经存在，至少八家公司在认真做</strong>。但没有一家做了因果记忆。</p>
<div class="card"><div class="card-row"><div class="card-num">01</div><div class="card-body">
<h4>Mem0 — 后台自动抽取事实 + MCP 挂载</h4>
<p>每轮后 LLM 自动抽取"值得记的事实"，三层混合存储（vector + graph + KV），相似度召回。用户必须在 AGENTS.md 里强制写"Do NOT ask before searching"——因为 agent 不会主动调记忆工具。<strong>盲召回，不理解任务。</strong></p>
</div></div></div>
<div class="card"><div class="card-row"><div class="card-num">02</div><div class="card-body">
<h4>Zep / Graphiti — 时序知识图谱</h4>
<p>每条事实带时间窗口的边。事实变化时不覆盖旧边——标记 valid-to + 创建新边。保留完整时间历史。但<strong>存的是实体关系（属于/喜欢），不是因果关系（导致/阻止）</strong>。</p>
</div></div></div>
<div class="card"><div class="card-row"><div class="card-num">03</div><div class="card-body">
<h4>Letta / MemGPT — Agent 自管理记忆（OS 范式）</h4>
<p>最激进：Agent 用工具调用自己管理记忆（core memory + archival memory）。押注"token 空间持续学习比模型权重更重要"。但记忆仍然是扁平事实 + 摘要，<strong>没有因果层</strong>。</p>
</div></div></div>
<div class="card"><div class="card-row"><div class="card-num">04</div><div class="card-body">
<h4>OpenViking / Cognee / M3 / MemOS / OpenMemory</h4>
<p>虚拟文件系统 / ECL 管道 / 多模态 / 记忆操作系统 / MCP server——架构各异，但<strong>全部是检索式，无一做重构式。全部存事实，无一存因果。</strong></p>
<div class="src">insight/10-memory-frameworks.md · 8 家公司全景诊断</div>
</div></div></div>
<div class="insight-card"><div class="label">最大空白</div>
<div class="quote">"实体关系图 ≠ 因果图。没有任何一家生产级记忆公司做了因果状态库。"</div>
<div class="desc">Zep 存"用户在 Pro 套餐"（实体关系），不存"因为选了 Redis 所以缓存击穿"（因果关系）。三个 7×24 核心需求——身份持久化 / 失败归因 / 任务感知检索——都指向同一个答案：<strong>因果状态库</strong>。</div></div>
</div>

<div class="divider"><span>论文持续追踪</span></div>

<!-- ═══ 06: 论文 ═══ -->
<div class="section">
<div class="section-num">06</div>
<div class="section-kicker">📄 前沿研究</div>
<h2 class="section-title">20+ 篇论文的<br><span class="hl">持续追踪</span></h2>
<p class="prose">我们建立了每日论文挖掘机制，持续追踪 AI 记忆领域的所有重要进展。以下是最关键的发现：</p>
<div class="card"><div class="card-row"><div class="card-num">01</div><div class="card-body">
<h4>HeLa-Mem（ACL 2026）— 最直接的竞争者</h4>
<p>Hebbian 学习 + spreading activation + 巩固。和我们高度重叠——但<strong>它做了兴奋侧（Hebbian 正向权重），我们做了抑制侧（prevented 负扩散）</strong>。完整的人脑需要两者。</p>
<div class="src">arXiv:2604.16839 · IAAR-Shanghai</div>
</div></div></div>
<div class="card"><div class="card-row"><div class="card-num">02</div><div class="card-body">
<h4>Anthropic Dreams API — 巩固的工业化标准</h4>
<p>产出新 memory store（不修改原始 store）+ 可配置 instructions。我们的 SWR consolidate 直接对齐这个设计。</p>
<div class="src">platform.claude.com · 官方文档</div>
</div></div></div>
<div class="card"><div class="card-row"><div class="card-num">03</div><div class="card-body">
<h4>Nemori — Free Energy Principle 驱动的惊讶门控</h4>
<p>Friston 的 FEP：只在 agent 预测和现实差距大时才写入记忆。LoCoMo 71-82%。比我们的 Shannon entropy 更有理论基础。</p>
<div class="src">arXiv:2508.03341</div>
</div></div></div>
<div class="card"><div class="card-row"><div class="card-num">04</div><div class="card-body">
<h4>LoCoMo Benchmark 审计 — 6.4% 答案是错的</h4>
<p>Penfield Labs 独立审计：LoCoMo 的 1,540 个答案中 99 个（6.4%）是错的。<strong>所有基于 LoCoMo 的排名都需要重新审视。</strong></p>
<div class="src">arXiv:2607.21962</div>
</div></div></div>
<div class="card"><div class="card-row"><div class="card-num">05</div><div class="card-body">
<h4>Oracle Agent Memory — LongMemEval 93.8% + LazyMem 213 token</h4>
<p>Oracle 的 lifecycle 管理 + reversible consolidation 达到 93.8%（新 SOTA）。LazyMem 证明不写入时构建、只查询时构建，用 68.7x 更少 token 达到 0.85。</p>
<div class="src">arXiv:2607.13157 · arXiv:2607.22690</div>
</div></div></div>
<div class="card"><div class="card-row"><div class="card-num">06</div><div class="card-body">
<h4>世界模型 — caused 边 = 转移函数样本</h4>
<p>Physical Intelligence 定义世界模型为"有限计算资源下对状态转移的压缩建模"。causal-memory 的 caused 边 [决策→结果] 就是转移函数 f(state,action)→outcome 的样本。向后走是归因，向前走是模拟——<strong>零竞品的蓝海</strong>。</p>
<div class="src">arXiv:2607.06401 · arXiv:2604.27895</div>
</div></div></div>
</div>

<div class="divider"><span>核心洞察</span></div>

<!-- ═══ 07: 核心洞察 ═══ -->
<div class="section">
<div class="section-num">07</div>
<div class="section-kicker">💡 设计决策</div>
<h2 class="section-title">三个<span class="hl">改变方向</span><br>的发现</h2>
<div class="insight-card"><div class="label">洞察 1 · 统一记忆（One Graph）</div>
<div class="quote">"所有记忆类型——事实、因果、偏好、模式——都是同一张图上的 typed edge"</div>
<div class="desc">不是多个存储拼在一起，是一个引擎统一检索、统一传播。事实层和因果层共享同一套 spreading activation。四种记忆架构（外部检索 / Agent 自管理 / 自检索+目录 / 重构式）不是互斥的，是同一张图上的四种检索/写入模式。</div></div>
<div class="insight-card"><div class="label">洞察 2 · 兴奋 / 抑制二元性</div>
<div class="quote">"caused = 谷氨酸兴奋 · prevented = GABA 抑制"</div>
<div class="desc">人脑同时有兴奋性和抑制性突触——缺一不可。兴奋侧记住有用的（LTP），抑制侧压制有害的（LTD/GABA）。HeLa-Mem 做了兴奋侧，我们做了抑制侧。<strong>完整的系统需要两者</strong>。prevented 负扩散是<span style="color:var(--red);font-weight:700">无竞品的能力</span>。</div></div>
<div class="insight-card"><div class="label">洞察 3 · 记忆 → 世界模型</div>
<div class="quote">"caused 边 = 转移函数样本 · 向后走 = 归因 · 向前走 = 模拟"</div>
<div class="desc">记忆系统从"记事本"到"模拟器"的转变，是比 QA benchmark 分数更锋利的定位。intervention_query 是唯一的前向模拟能力——<span style="color:var(--gold);font-weight:700">零竞品、零 benchmark 的蓝海</span>。</div></div>
</div>

<div class="divider"><span>工程实现</span></div>

<!-- ═══ 08: 架构 ═══ -->
<div class="section">
<div class="section-num">08</div>
<div class="section-kicker">⚙️ 系统</div>
<h2 class="section-title">四层<span class="hl">统一架构</span></h2>
<div class="arch">
<div class="arch-layer-title">✏️ 写入层 WRITE — 三条通道</div>
<div class="arch-row"><div class="arch-block"><span class="icon">📝</span><span class="name">RAW Ingest</span><span class="desc">每轮 verbatim · 可逆</span></div><div class="arch-block"><span class="icon">🧪</span><span class="name">Distill</span><span class="desc">LLM 提取因果/事实</span></div><div class="arch-block"><span class="icon">🔌</span><span class="name">MCP Tools</span><span class="desc">Agent 运行时写入</span></div></div>
<div class="arch-arrow">↓</div>
<div class="arch-layer-title">💾 存储层 STORE — SQLite v7 · 统一图</div>
<div class="arch-row"><div class="arch-block"><span class="icon">📄</span><span class="name">chunks</span><span class="desc">原始对话</span></div><div class="arch-block"><span class="icon">📋</span><span class="name">agent_facts</span><span class="desc">事实 + 偏好</span></div><div class="arch-block"><span class="icon">🔗</span><span class="name">causal_edges</span><span class="desc">因果边 (7种)</span></div></div>
<div class="arch-arrow">↓</div>
<div class="arch-layer-title">🧠 引擎层 ENGINE — 海马体 CSR 稀疏矩阵</div>
<div class="arch-row"><div class="arch-block"><span class="icon">⚡</span><span class="name">Spreading</span><span class="desc">typed edge 激活扩散</span></div><div class="arch-block"><span class="icon">🌙</span><span class="name">SWR 巩固</span><span class="desc">LTP/LTD/GC + Q-value</span></div><div class="arch-block"><span class="icon">📈</span><spanclass="name">在线学习</span><span class="desc">Hebbian + novelty</span></div></div>
<div class="arch-arrow">↓</div>
<div class="arch-layer-title">🔍 检索层 RETRIEVE — 13 MCP 工具</div>
<div class="arch-row"><div class="arch-block"><span class="icon">🔎</span><span class="name">search_memory</span><span class="desc">RRF 统一检索</span></div><div class="arch-block"><span class="icon">⬅️</span><span class="name">trace_cause</span><span class="desc">向后归因</span></div><div class="arch-block hl"><span class="icon">➡️</span><span class="name">intervention</span><span class="desc">⚡ 向前模拟（独家）</span></div></div>
</div>
</div>

<!-- ═══ 09: 边类型 ═══ -->
<div class="section">
<div class="section-num">09</div>
<div class="section-kicker">🔗 统一记忆</div>
<h2 class="section-title">7 种<span class="hl">typed edge</span></h2>
<div class="edge-item"><div class="edge-dot" style="background:var(--green)"></div><div class="edge-name">caused</div><div class="edge-desc">A 导致了 B（决策→结果）</div><div class="edge-val" style="color:var(--green)">+1.0</div></div>
<div class="edge-item"><div class="edge-dot" style="background:var(--blue)"></div><div class="edge-name">fact</div><div class="edge-desc">语义事实（偏好/属性）</div><div class="edge-val" style="color:var(--blue)">+0.8</div></div>
<div class="edge-item"><div class="edge-dot" style="background:#adb5bd"></div><div class="edge-name">meta</div><div class="edge-desc">跨情景统计模式</div><div class="edge-val" style="color:#adb5bd">+0.6</div></div>
<div class="edge-item"><div class="edge-dot" style="background:#69db7c"></div><div class="edge-name">enabled</div><div class="edge-desc">A 使 B 成为可能</div><div class="edge-val" style="color:#69db7c">+0.5</div></div>
<div class="edge-item"><div class="edge-dot" style="background:var(--purple)"></div><div class="edge-name">co-occur</div><div class="edge-desc">Hebbian 共现学习（动态）</div><div class="edge-val" style="color:var(--purple)">+0.2</div></div>
<div class="edge-item hl"><div class="edge-dot" style="background:var(--red);box-shadow:0 0 8px var(--red)"></div><div class="edge-name">prevented</div><div class="edge-desc">A 阻止 B · GABA 抑制性负扩散</div><div class="edge-val" style="color:var(--red)">−0.3</div></div>
</div>

<!-- ═══ 10: Benchmark ═══ -->
<div class="section">
<div class="section-num">10</div>
<div class="section-kicker">📊 实测</div>
<h2 class="section-title">数据<span class="hl">说话</span></h2>
<div class="bench-block"><div class="bench-head">LongMemEval（500 题）</div>
<div class="bench-row"><div class="bench-lbl"><span class="name">causal-memory</span><span class="score" style="color:var(--gold)">71.2%</span></div><div class="bench-track"><div class="bench-fill gold" data-w="71"></div></div></div>
<div class="bench-row"><div class="bench-lbl"><span class="name muted">Mem0</span><span class="score" style="color:var(--muted)">~64%</span></div><div class="bench-track"><div class="bench-fill gray" data-w="64"></div></div></div></div>
<div class="bench-block"><div class="bench-head">LoCoMo（记忆基准）</div>
<div class="bench-row"><div class="bench-lbl"><span class="name">causal-memory</span><span class="score" style="color:var(--gold)">84.1%</span></div><div class="bench-track"><div class="bench-fill gold" data-w="84"></div></div></div>
<div class="bench-row"><div class="bench-lbl"><span class="name muted">行业最强</span><span class="score" style="color:var(--muted)">91.6%</span></div><div class="bench-track"><div class="bench-fill gray" data-w="92"></div></div></div></div>
<div class="stat-row">
<div class="stat-card"><div class="stat-num" style="color:var(--gold)">0%</div><div class="stat-lbl">重复犯错率<br>（无记忆 20%）</div></div>
<div class="stat-card"><div class="stat-num" style="color:var(--green)">+20.8</div><div class="stat-lbl">压缩生存率<br>提升 pp</div></div>
<div class="stat-card"><div class="stat-num" style="color:var(--blue)">88.6%</div><div class="stat-lbl">证据命中率</div></div>
</div>
</div>

<!-- ═══ 11: 独家 ═══ -->
<div class="section">
<div class="section-num">11</div>
<div class="section-kicker">⚡ 独家</div>
<h2 class="section-title">别人<span class="hl">做不到</span>的</h2>
<div class="card"><div class="card-row"><div class="card-num">01</div><div class="card-body"><h4>🚫 Prevented 负扩散</h4><p>GABA 抑制性类比。错误/恶意记忆被自动抑制。天然防御记忆注入——这是人脑抑制性突触的 AI 实现。</p><span class="tag tag-red">无竞品</span></div></div></div>
<div class="card"><div class="card-row"><div class="card-num">02</div><div class="card-body"><h4>🔮 前向模拟 (intervention_query)</h4><p>行动前预测后果。沿因果图 propagation。把记忆从"记事本"升级为"世界模型"。</p><span class="tag">零竞品 · 蓝海</span></div></div></div>
<div class="card"><div class="card-row"><div class="card-num">03</div><div class="card-body"><h4>🌙 SWR 睡眠巩固</h4><p>LTP/LTD/GC 四阶段。产出新图不改原图——可审计、可回滚。与 Anthropic Dreams 对齐。</p><span class="tag">Dreams 对齐</span></div></div></div>
<div class="card"><div class="card-row"><div class="card-num">04</div><div class="card-body"><h4>⚡ CSR 稀疏矩阵 + 在线进化</h4><p>248K 节点实时检索。Q-value Bellman + Hebbian LTP + novelty entropy——7×24 持续进化。</p><span class="tag">Rust 性能</span></div></div></div>
</div>

</div>

<div class="cta">
<h2 class="cta-title">让每个 AI Agent<br>都拥有<span class="hl">因果记忆</span></h2>
<p class="cta-sub">42 篇源码拆解 · 17 篇跨学科研究 · 20+ 篇论文追踪 · 206 个工程测试</p>
<a href="https://github.com/JingxuanC/causal-memory" class="cta-btn">⭐ GitHub Star</a>
<div class="cta-gh">github.com/JingxuanC/causal-memory</div>
</div>

<script>
const obs=new IntersectionObserver(e=>{e.forEach(en=>{if(en.isIntersecting){en.target.querySelectorAll('.bench-fill').forEach(f=>{const w=f.dataset.w;f.style.width='0';setTimeout(()=>f.style.width=w+'%',200)})}})},{threshold:.2});
document.querySelectorAll('.bench-block').forEach(el=>obs.observe(el));
</script>
</body></html>'''

with open('promo.html', 'w') as f:
    f.write(html)
print(f"Written {len(html)} bytes")
