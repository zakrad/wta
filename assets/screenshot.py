#!/usr/bin/env python3
"""Generate assets/wta.svg — a stylized mock of the `wta` global dashboard.

Not a literal screen-grab (the dashboard is an interactive ratatui TUI); it's a
faithful, hand-composed representation of the current layout: a per-repo tree of
agents, each showing its BASE branch + tokens used + diffstat, beside a live
Preview pane. Regenerate the PNG with:

    python3 assets/screenshot.py && rsvg-convert -z 2 assets/wta.svg -o assets/wta.png
"""

W, H = 1068, 527
BG, TITLE = "#0b0f0b", "#141a14"
SIDE_W = 430          # left sidebar width
KEYBAR_H = 26
FS = 14
LH = 20               # sidebar line height
COL = {
    "muted":  "#8b978b",
    "faint":  "#6f7d6f",
    "text":   "#c9d1c9",
    "green":  "#3fb950",
    "bright": "#56d364",
    "base":   "#6f7d6f",   # base-branch label (dark gray)
    "tok":    "#d7a53f",   # tokens (yellow)
    "add":    "#3fb950",
    "del":    "#d15b52",
    "amber":  "#d29922",   # needs-input
    "cyan":   "#3aa0c9",   # merged
    "gray":   "#6e7681",   # exited
    "sel":    "#16211a",   # selected-row highlight
}

def esc(s):
    return s.replace("&", "&amp;").replace("<", "&lt;").replace(">", "&gt;")

out = []
def txt(x, y, s, fill, weight=None, size=FS, anchor="start", opacity=None):
    w = f' font-weight="{weight}"' if weight else ""
    a = f' text-anchor="{anchor}"' if anchor != "start" else ""
    o = f' fill-opacity="{opacity}"' if opacity is not None else ""
    fs = f' font-size="{size}"' if size != FS else ""
    out.append(f'<text x="{x}" y="{y}"{fs} fill="{fill}"{w}{a}{o}>{esc(s)}</text>')

def rect(x, y, w, h, fill, rx=0, opacity=None):
    r = f' rx="{rx}"' if rx else ""
    o = f' fill-opacity="{opacity}"' if opacity is not None else ""
    out.append(f'<rect x="{x}" y="{y}" width="{w}" height="{h}" fill="{fill}"{r}{o}/>')

# ---- window chrome ----
out.append(
    f'<svg xmlns="http://www.w3.org/2000/svg" width="{W}" height="{H}" '
    f'viewBox="0 0 {W} {H}" '
    f'font-family="ui-monospace,\'SFMono-Regular\',Menlo,\'DejaVu Sans Mono\',Consolas,monospace" '
    f'font-size="{FS}">'
)
rect(0, 0, W, H, BG, rx=12)
out.append(f'<path d="M0 12 A12 12 0 0 1 12 0 H{W-12} A12 12 0 0 1 {W} 12 V36 H0 Z" fill="{TITLE}"/>')
for i, c in enumerate(("#ff5f57", "#febc2e", "#28c840")):
    out.append(f'<circle cx="{20+i*20}" cy="18" r="6" fill="{c}"/>')
txt(W//2, 23, "wta — parallel AI agents · git worktree + tmux", COL["muted"], anchor="middle")

# vertical divider between sidebar and preview
out.append(f'<line x1="{SIDE_W}" y1="36" x2="{SIDE_W}" y2="{H-KEYBAR_H}" stroke="#1c241c" stroke-width="1"/>')

# ---- sidebar header ----
txt(16, 60, "Instances", COL["bright"], weight="700")
out.append(f'<line x1="14" y1="70" x2="{SIDE_W-14}" y2="70" stroke="#1c241c" stroke-width="1"/>')

# glyph color per status
GLYPH = {
    "running": ("◐", COL["green"]),
    "ready":   ("●", COL["green"]),
    "input":   ("▲", COL["amber"]),
    "merged":  ("✓", COL["cyan"]),
    "exited":  ("✗", COL["gray"]),
}

# tree: (repo, [ (n, task, status, base, tokens, adds, dels) ... ])
tree = [
    ("sooth-core (3)", [
        (1, "certora-solvency", "running", "main",        "49.9M", 2548, 14),
        (2, "api-refactor",     "ready",   "develop",     "1.3M",   214,  9),
        (3, "flaky-test",       "input",   "main",        "88k",     12,  3),
    ]),
    ("web-app (2)", [
        (4, "dark-mode",        "merged",  "main",        "430k",    96, 40),
        (5, "perf-audit",       "exited",  "release/2.0", "2.1M",   540,120),
    ]),
]

x_glyph = SIDE_W - 22          # status glyph, right-aligned
x_right = SIDE_W - 20          # +adds,-dels ends here
x_tok   = SIDE_W - 150         # tokens end here
y = 92
SELECTED = 1                   # highlight agent #1

for repo, agents in tree:
    txt(14, y, "▸ " + repo, COL["muted"], weight="700")
    y += LH + 2
    for (n, task, status, base, tok, adds, dels) in agents:
        glyph, gcol = GLYPH[status]
        if n == SELECTED:
            rect(8, y - 15, SIDE_W - 22, LH * 2 + 6, COL["sel"], rx=5)
            out.append(f'<rect x="8" y="{y-15}" width="3" height="{LH*2+6}" fill="{COL["green"]}" rx="1.5"/>')
        name_col = COL["bright"] if n == SELECTED else COL["text"]
        txt(24, y, f"{n}. {task}", name_col)
        txt(x_glyph, y, glyph, gcol, anchor="end", size=15)
        y += LH
        # line 2: Ꮧ <base>            <tokens>   +adds,-dels
        txt(30, y, "Ꮧ " + base, COL["base"])
        txt(x_tok, y, tok, COL["tok"], anchor="end")
        out.append(
            f'<text x="{x_right}" y="{y}" text-anchor="end" xml:space="preserve">'
            f'<tspan fill="{COL["add"]}">+{adds}</tspan>'
            f'<tspan fill="{COL["faint"]}">,</tspan>'
            f'<tspan fill="{COL["del"]}">-{dels}</tspan>'
            f'</text>'
        )
        y += LH + 8
    y += 4

# ---- right pane: Preview | Diff tabs ----
px = SIDE_W + 22
txt(px, 60, "Preview", COL["bright"], weight="700")
out.append(f'<line x1="{px}" y1="70" x2="{px+56}" y2="70" stroke="{COL["green"]}" stroke-width="2"/>')
txt(px + 78, 60, "Diff", COL["faint"])
txt(W - 20, 60, "certora-solvency", COL["faint"], anchor="end")
out.append(f'<line x1="{px}" y1="70" x2="{W-16}" y2="70" stroke="#1c241c" stroke-width="1"/>')

# mock live agent preview (colored, like a real claude pane)
pl = px
py = 96
def pv(y, spans):
    """spans: list of (text, color)"""
    x = pl
    parts = []
    for s, c in spans:
        parts.append(f'<tspan fill="{c}">{esc(s)}</tspan>')
    out.append(f'<text x="{x}" y="{y}" xml:space="preserve">{"".join(parts)}</text>')

pv(py,       [("● ", COL["green"]), ("I'll verify the solvency invariant across the vault", COL["text"])])
pv(py+LH,    [("  set, then run the Certora spec.", COL["text"])])
pv(py+LH*3,  [("● ", COL["green"]), ("Bash(", COL["text"]), ("certoraRun specs/Solvency.spec", COL["bright"]), (")", COL["text"])])
pv(py+LH*4,  [("  ⎿  ", COL["faint"]), ("Verifying rule solvency_preserved …", COL["muted"])])
pv(py+LH*5,  [("     ", COL["faint"]), ("✓ solvency_preserved", COL["green"]), ("   ·  12 rules, 0 violations", COL["muted"])])
pv(py+LH*7,  [("● ", COL["green"]), ("The invariant holds. Writing the summary to ", COL["text"]), ("REPORT.md", COL["bright"]), (".", COL["text"])])
pv(py+LH*9,  [("● ", COL["green"]), ("Update(", COL["text"]), ("REPORT.md", COL["bright"]), (")", COL["text"])])
pv(py+LH*10, [("  ⎿  ", COL["faint"]), ("+18 lines", COL["add"])])
pv(py+LH*12, [("  ", COL["text"]), ("◐", COL["green"]), (" Running…", COL["muted"])])

# a subtle cost line at the pane bottom (the headline change)
out.append(f'<line x1="{px}" y1="{H-KEYBAR_H-34}" x2="{W-16}" y2="{H-KEYBAR_H-34}" stroke="#1c241c" stroke-width="1"/>')
out.append(
    f'<text x="{px}" y="{H-KEYBAR_H-14}" xml:space="preserve">'
    f'<tspan fill="{COL["muted"]}">cost  </tspan>'
    f'<tspan fill="{COL["tok"]}">49.9M tok</tspan>'
    f'<tspan fill="{COL["faint"]}">  ·  ~$135.58 est  ·  </tspan>'
    f'<tspan fill="{COL["muted"]}">model </tspan>'
    f'<tspan fill="{COL["text"]}">opus-4-8</tspan>'
    f'</text>'
)

# ---- keybar ----
rect(0, H - KEYBAR_H, W, KEYBAR_H, "#10160f")
out.append(f'<path d="M0 {H-KEYBAR_H} H{W} V{H-12} A12 12 0 0 1 {W-12} {H} H12 A12 12 0 0 1 0 {H-12} Z" fill="#10160f"/>')
keys = "j/k move · Enter attach · Tab preview/diff · i send · m matrix · n new · v verify · q quit"
txt(16, H - 8, keys, COL["faint"], size=12)

out.append("</svg>")

svg = "".join(out)
import pathlib
p = pathlib.Path(__file__).with_name("wta.svg")
p.write_text(svg)
print(f"wrote {p} ({len(svg)} bytes)")
