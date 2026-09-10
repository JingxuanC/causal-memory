#!/usr/bin/env python3
"""session_to_turns.py — convert agent session logs to the causal-memory
`distill` input format.

WHY: `causal-memory extract/judge/reasoning` only parse agent formats the
Rust CLI ships a SessionParser for. The universal interchange format is the
`distill` turns JSON — convert ANY agent's session to it, then:

    causal-memory distill <out.json>            # write into the memory store
    causal-memory distill <out.json> --dry-run  # preview, no writes

═══ TURNS JSON SPEC (the contract) ═══

{
  "date": "YYYY-MM-DD",                // session date; chunks get it as
                                       // their id prefix (date-tNNN)
  "turns": [
    ["user", "..."],                   // [speaker, message] pairs, in order
    ["assistant", "..."],
    ...
  ]
}

Rules:
- `turns` is an ordered array of [speaker, message]; speaker is a free-form
  string ("user"/"assistant" are the convention — the distiller treats any
  non-"user" speaker as the agent side).
- Merge consecutive same-speaker fragments into one turn (streaming
  deltas must be stitched first).
- Include the assistant's REASONING (thinking blocks), not just final
  answers — "decisions that never became actions" live there. Prefix them
  with "[think] " so the distiller can tell deliberation from reply.
- Tool calls/results may be inlined as assistant turns like
  "[tool: name] args → result" when they carry the outcome of a decision
  (e.g. a failed deploy). Skip noisy reads.
- One file per session/day; for a directory of such files, `distill <dir>`
  processes every *.json in it.

Output (what distill does with it): one LLM call per ~15 messages routes
facts/preferences → the fact layer (record_fact) and lessons/causal
relations → causal edges (record_decision). Use --dry-run to inspect the
items before writing.

ADAPTING TO YOUR AGENT: write a converter like the one below for your
session format — the kimi-code wire v1.5 parser here is the reference. It
is ~60 lines; the only real work is (a) finding user messages, (b) stitching
assistant text/think parts in order.
"""
import json, sys, datetime


def convert_kimi_wire(path):
    """kimi-code CLI wire.jsonl (protocol 1.5) → turns JSON dict."""
    date = None
    turns = []

    def flush(buf, speaker):
        if buf:
            turns.append([speaker, "\n".join(buf)])
            buf.clear()

    user_buf, asst_buf = [], []
    for line in open(path, encoding="utf-8"):
        line = line.strip()
        if not line:
            continue
        try:
            e = json.loads(line)
        except json.JSONDecodeError:
            continue
        ts = e.get("time") or e.get("created_at")
        if date is None and ts:
            date = datetime.datetime.fromtimestamp(ts / 1000).strftime("%Y-%m-%d")

        etype = e.get("type")
        # ── user turn ──
        if etype == "context.append_message":
            msg = e.get("message", {})
            if msg.get("role") == "user" and msg.get("origin", {}).get("kind") == "user":
                text = "".join(p.get("text", "") for p in msg.get("content", [])
                               if p.get("type") == "text").strip()
                if text:
                    flush(asst_buf, "assistant")
                    user_buf.append(text)
                    flush(user_buf, "user")
        # ── assistant text / reasoning ──
        elif etype == "context.append_loop_event":
            ev = e.get("event", {})
            if ev.get("type") == "content.part":
                part = ev.get("part", {})
                if part.get("type") == "text" and part.get("text", "").strip():
                    flush(user_buf, "user")
                    asst_buf.append(part["text"].strip())
                elif part.get("type") == "think" and part.get("think", "").strip():
                    flush(user_buf, "user")
                    asst_buf.append("[think] " + part["think"].strip())

    flush(user_buf, "user")
    flush(asst_buf, "assistant")
    return {"date": date or datetime.date.today().isoformat(), "turns": turns}


def main():
    if len(sys.argv) != 3 or sys.argv[1] in ("-h", "--help"):
        print(__doc__)
        print("Usage: session_to_turns.py <session-file> <out.json>")
        print("       (input: kimi-code wire.jsonl; adapt convert_* for other agents)")
        sys.exit(0 if len(sys.argv) == 2 else 1)
    src, dst = sys.argv[1], sys.argv[2]
    doc = convert_kimi_wire(src)
    if not doc["turns"]:
        print(f"no conversation turns found in {src}", file=sys.stderr)
        sys.exit(1)
    with open(dst, "w", encoding="utf-8") as f:
        json.dump(doc, f, ensure_ascii=False, indent=1)
    print(f"{dst}: {len(doc['turns'])} turns, date={doc['date']}")
    print(f"next: causal-memory distill {dst} --dry-run")


if __name__ == "__main__":
    main()
