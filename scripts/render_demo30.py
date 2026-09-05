#!/usr/bin/env python3
"""Render the 30s single-scene demo: dangerous op -> DANGER warning -> past lesson."""
import json
from PIL import Image, ImageDraw, ImageFont
import imageio.v2 as imageio
import numpy as np
import os

W, H = 1280, 800
FPS = 12
BG = (13, 17, 23)
FG = (201, 209, 217)
DIM = (139, 148, 158)
GREEN = (126, 231, 135)
RED = (255, 123, 114)
YELLOW = (210, 153, 34)
CYAN = (121, 192, 255)

FONT_PATH = "/System/Library/Fonts/PingFang.ttc"
F = ImageFont.truetype(FONT_PATH, 19, index=0)
F_BIG = ImageFont.truetype(FONT_PATH, 64, index=0)
F_MID = ImageFont.truetype(FONT_PATH, 26, index=0)
LH = 30

EMOJI = {"⚠️": "[!]", "⚠": "[!]", "✅": "[ok]", "ℹ️": "[i]", "→": "->"}

def clean(s):
    for k, v in EMOJI.items():
        s = s.replace(k, v)
    return "".join(ch for ch in s if ord(ch) < 0x10000)

def wrap(s, maxu=66):
    # Wrap at word boundaries when possible (avoid "mai n" style mid-word
    # breaks); fall back to hard char wrap for long unbroken tokens/CJK.
    out = []
    while True:
        w = sum(2 if ord(c) > 0x2E7F else 1 for c in s)
        if w <= maxu:
            out.append(s)
            return out
        cut, acc = 0, 0
        for i, ch in enumerate(s):
            acc += 2 if ord(ch) > 0x2E7F else 1
            if acc > maxu:
                cut = i
                break
        space = s.rfind(" ", 0, cut)
        if space > maxu // 3:
            out.append(s[:space])
            s = s[space + 1:]
        else:
            out.append(s[:cut])
            s = s[cut:]

def prep(text, max_lines):
    lines = []
    for ln in clean(text).splitlines():
        lines.extend(wrap(ln) if ln.strip() else [""])
    return lines[:max_lines]

def color_for(line):
    if line.startswith(">") or line.startswith("$") or line.startswith("#"):
        return GREEN
    if "DANGER" in line or "[x]" in line:
        return RED
    if "UNKNOWN" in line or "warning" in line.lower():
        return YELLOW
    if line.startswith("[ok]"):
        return GREEN
    if line.startswith(("Chain", "[")):
        return CYAN
    if line.startswith(("hop", "edge confidence", "confidence", "pooled", "task_tag")):
        return DIM
    return FG

def frame(lines, title="agent @ causal-memory — pre-action check"):
    im = Image.new("RGB", (W, H), BG)
    d = ImageDraw.Draw(im)
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
    d.text(((W - d.textlength(t, font=F_BIG)) / 2, 290), t, font=F_BIG, fill=(26, 26, 46))
    s = "Agents that warn you before you repeat a mistake"
    d.text(((W - d.textlength(s, font=F_MID)) / 2, 400), s, font=F_MID, fill=(200, 80, 70))
    u = "github.com/JingxuanC/causal-memory"
    d.text(((W - d.textlength(u, font=F)) / 2, 465), u, font=F, fill=(108, 117, 125))
    return im

out = json.load(open("/tmp/demo30_out.json"))

# Trim intervention output to Chain 1 (DANGER) + the pooled verdict line.
iv_lines = clean(out["intervention"]).splitlines()
chain1, rest = [], []
it = iter(iv_lines)
for ln in it:
    chain1.append(ln)
    if ln.strip().startswith("edge confidence"):
        break
pooled = [ln for ln in iv_lines if ln.strip().startswith("pooled")]
iv_show = []
for ln in chain1:
    iv_show.extend(wrap(ln) if ln.strip() else [""])
iv_show += [""] + pooled

search_show = prep(out["search"], 8)

scenes = [
    # Scene 0: the agent is about to do the dangerous thing.
    dict(lines=[
        "$ git add . && git commit -m 'quick fix: login timeout'",
        "# tests take 4 min... just push with --no-verify",
    ], hold=3.0),
    # Scene 1: pre-action check -> DANGER.
    dict(lines=["> intervention_query(\"skip tests and push directly to main\")", ""] + iv_show, hold=7.0),
    # Scene 2: the lesson behind the warning.
    dict(lines=["> search_causal(\"push to main without running tests\")", ""] + search_show, hold=6.0),
]

frames = []
stills = {}

def type_out(full_lines, from_idx=0):
    """Reveal: >/#/$ lines typed char by char, output lines 2 per frame."""
    revealed = []
    for i, ln in enumerate(full_lines):
        if ln.startswith((">", "$", "#")):
            for k in range(1, len(ln) + 1, 2):
                frames.append(frame(revealed + [ln[:k]]))
            frames.append(frame(revealed + [ln]))
        else:
            revealed.append(ln)
            frames.append(frame(revealed))
            if i % 2:
                frames.append(frame(revealed))

for si, sc in enumerate(scenes):
    type_out(sc["lines"])
    full = frame(sc["lines"])
    if si == 1:
        stills["demo30_danger"] = full
    for _ in range(int(sc["hold"] * FPS)):
        frames.append(full)

card = end_card()
stills["demo30_card"] = card
for _ in range(int(3.5 * FPS)):
    frames.append(card)

os.makedirs("docs/demo", exist_ok=True)
writer = imageio.get_writer(
    "docs/demo/causal-memory-danger-30s.mp4", fps=FPS, codec="libx264",
    quality=8, macro_block_size=16,
    ffmpeg_params=["-g", "6", "-pix_fmt", "yuv420p"])
for fr in frames:
    writer.append_data(np.asarray(fr))
writer.close()
for name, im in stills.items():
    im.save(f"docs/demo/{name}.png")
print(f"video: {len(frames)} frames, {len(frames)/FPS:.1f}s")
