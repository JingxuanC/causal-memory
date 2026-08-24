#!/usr/bin/env python3
"""Render the causal-memory demo video (mp4) + still PNGs from captured outputs."""
import json, textwrap
from PIL import Image, ImageDraw, ImageFont
import imageio.v2 as imageio

W, H = 1280, 800
FPS = 12
BG = (13, 17, 23)
FG = (201, 209, 217)
DIM = (139, 148, 158)
GREEN = (126, 231, 135)
RED = (255, 123, 114)
YELLOW = (210, 153, 34)
CYAN = (121, 192, 255)
GOLD = (245, 180, 0)

FONT_PATH = "/System/Library/Fonts/PingFang.ttc"
F = ImageFont.truetype(FONT_PATH, 19, index=0)
F_BIG = ImageFont.truetype(FONT_PATH, 64, index=0)
F_MID = ImageFont.truetype(FONT_PATH, 26, index=0)
LH = 30  # line height

EMOJI = {"⚠️": "[!]", "⚠": "[!]", "✅": "[ok]", "📊": "[facts]", "📚": "[lessons]",
         "📭": "[empty]", "🔍": "", "ℹ️": "[i]", "🚫": "[x]", "🧠": "", "→": "->"}

def clean(s):
    for k, v in EMOJI.items():
        s = s.replace(k, v)
    # drop any remaining non-BMP glyphs (emoji) the font can't draw
    return "".join(ch for ch in s if ord(ch) < 0x10000)

def width_units(s):
    return sum(2 if ord(c) > 0x2E7F else 1 for c in s)

def wrap(s, maxu=64):
    out, cur, w = [], "", 0
    for ch in s:
        u = 2 if ord(ch) > 0x2E7F else 1
        if w + u > maxu:
            out.append(cur); cur, w = "", 0
        cur += ch; w += u
    out.append(cur)
    return out

def prep(text, max_lines):
    lines = []
    for ln in clean(text).splitlines():
        lines.extend(wrap(ln) if ln.strip() else [""])
    return lines[:max_lines]

def color_for(line):
    if line.startswith(">") or line.startswith("$"): return GREEN
    if "DANGER" in line or "[x]" in line: return RED
    if "UNKNOWN" in line or "warning" in line.lower(): return YELLOW
    if line.startswith("[ok]"): return GREEN
    if line.startswith(("Chain", "[facts]", "[lessons]", "[")): return CYAN
    if line.startswith(("hop", "edge confidence", "confidence")): return DIM
    return FG

def frame(lines, title="agent @ causal-memory — MCP tools"):
    im = Image.new("RGB", (W, H), BG)
    d = ImageDraw.Draw(im)
    # title bar
    d.rectangle([0, 0, W, 46], fill=(22, 27, 34))
    for i, c in enumerate([(255, 95, 87), (254, 188, 46), (40, 200, 64)]):
        d.ellipse([22 + i * 26, 17, 34 + i * 26, 29], fill=c)
    tw = d.textlength(title, font=F)
    d.text(((W - tw) / 2, 13), title, font=F, fill=DIM)
    y = 66
    for ln in lines:
        d.text((40, y), ln, font=F, fill=color_for(ln))
        y += LH
    return im

def end_card():
    im = Image.new("RGB", (W, H), (248, 249, 250))
    d = ImageDraw.Draw(im)
    t = "causal-memory"
    d.text(((W - d.textlength(t, font=F_BIG)) / 2, 300), t, font=F_BIG, fill=(26, 26, 46))
    s = "让每个 AI Agent 都拥有因果记忆"
    d.text(((W - d.textlength(s, font=F_MID)) / 2, 410), s, font=F_MID, fill=(200, 134, 11))
    u = "github.com/JingxuanC/causal-memory"
    d.text(((W - d.textlength(u, font=F)) / 2, 470), u, font=F, fill=(108, 117, 125))
    return im

out = json.load(open("/tmp/demo_out.json"))

scenes = [
    (["> intervention_query(\"skip tests and push directly to main\")", ""]
     + prep(out["intervention"], 11), 3.0),
    (["> search_causal(\"build release makefile toolchain\")", ""]
     + prep(out["search"], 10), 2.5),
    (["> counterfactual_query(\"reuse main model, huge max_tokens\",", "                       \"dedicated small max_tokens model\")", ""]
     + prep(out["counterfactual"], 9), 2.5),
    (["> record_decision(\"参赛 demo 用真实库演示行动前预警\",", "                  \"预警链路清晰可见，数据可溯源\", enabled)", ""]
     + prep(out["record"], 4) + ["",
       "> search_causal(\"参赛 demo 预警\")", ""]
     + prep(out["verify"], 6), 3.0),
]

frames = []
stills = {}
for si, (lines, hold) in enumerate(scenes):
    prompt_len = sum(1 for l in lines if l.startswith(">"))
    # reveal: first the > lines one by one, then output 2 lines/frame
    revealed = []
    for i, ln in enumerate(lines):
        revealed.append(ln)
        if ln.startswith(">") or not ln:
            frames.append(frame(revealed)); frames.append(frame(revealed))
        else:
            frames.append(frame(revealed))
            if i % 2:
                frames.append(frame(revealed))
    full = frame(lines)
    if si == 0:
        stills["demo_intervention"] = full
    for _ in range(int(hold * FPS)):
        frames.append(full)

card = end_card()
stills["demo_card"] = card
for _ in range(3 * FPS):
    frames.append(card)

import os
os.makedirs("docs/demo", exist_ok=True)
writer = imageio.get_writer(
    "docs/demo/causal-memory-demo.mp4", fps=FPS, codec="libx264",
    quality=8, macro_block_size=16,
    ffmpeg_params=["-g", "6", "-pix_fmt", "yuv420p"])
for fr in frames:
    writer.append_data(__import__("numpy").asarray(fr))
writer.close()
for name, im in stills.items():
    im.save(f"docs/demo/{name}.png")
print(f"video: {len(frames)} frames, {len(frames)/FPS:.1f}s")
print("stills:", list(stills))
