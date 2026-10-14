#!/usr/bin/env python3
"""Generates docs/consensus_pipeline.svg: `scripts/pipeline_svg.py docs/consensus_pipeline.svg`."""
import sys
from xml.sax.saxutils import escape

W, H = 2320, 1160

# Columns, 290 apart, nodes 180 wide.
C0, C1, C2, C3, C4, C5, C6, C7 = 30, 260, 550, 840, 1130, 1420, 1710, 2010
NW = 180

# id: (label lines, kind, x, y, w, h)
NODES = {
    # black boxes
    "rpc": (["RPC clients"], "ext", C0, 40, 160, 56),
    "consensus": (["Consensus", "(black box)"], "ext", C0, 200, 160, 620),
    "statesync": (["State sync"], "ext", C7, 40, NW, 56),
    # submission
    "tv": (["TransactionValidator"], "proc", C1, 40, NW + 10, 56),
    "sv": (["SignatureVerifier"], "proc", C2, 40, NW, 56),
    "sc": (["SubmitChecker"], "proc", C3, 40, NW, 56),
    # voting and commits
    "voter": (["BlockVoter", "(× N)"], "proc", C1, 230, NW, 60),
    "decoder": (["CommitDecoder"], "proc", C1, 500, NW, 60),
    "handler": (["CommitHandler", "dedup · owned locks", "schedule · ordinals", "chunks · prologue"], "proc", C2, 460, NW, 120),
    # execution
    "resolver": (["VersionResolver"], "proc", C3, 500, NW, 60),
    "funds": (["FundsReserver", "bounds per version"], "proc", C4, 320, NW, 60),
    "inputwait": (["input waiter"], "waiter", C4, 500, NW, 60),
    "exec": (["Executor", "(× N)"], "proc", C5, 500, NW, 60),
    "effwait": (["effects waiter"], "waiter", C5, 170, NW, 60),
    # settlement and checkpoints
    "settle": (["SettlementScheduler"], "proc", C6, 320, NW + 10, 60),
    "ckwait": (["checkpoint waiter"], "waiter", C6, 640, NW, 60),
    "contents": (["ContentsBuilder", "(× N)"], "proc", C6, 790, NW, 60),
    "seqwait": (["sequence waiter"], "waiter", C7, 790, NW, 60),
    "summary": (["SummaryBuilder"], "proc", C7, 640, NW, 60),
    "hasher": (["StateHasher"], "proc", C5, 790, NW, 60),
    "sumwait": (["summary waiter"], "waiter", C7, 400, NW, 60),
    "agg": (["CheckpointAggregator"], "proc", C7, 220, NW + 10, 60),
    # stores
    "cache": (["decoded-tx cache"], "store", C1, 980, NW, 64),
    "objects": (["object store", "objects · effects"], "store", C3, 980, NW, 64),
    "statuses": (["consensus status cache"], "store", C4, 980, NW, 64),
    "gens": (["generation table"], "store", C5, 980, NW, 64),
    "chunks": (["chunk table"], "store", C6, 980, NW, 64),
    "ckstore": (["checkpoint store"], "store", C7, 980, NW, 64),
}

def mid(n):
    _, _, x, y, w, h = NODES[n]
    return x + w / 2

# (from, to, kind, label, options)
EDGES = [
    # submission
    ("rpc", "tv", "queue", "submit", {}),
    ("tv", "sv", "queue", "", {}),
    ("sv", "sc", "queue", "", {}),
    ("sc", "rpc", "ready", "checked: executed · rejected · to submit", {"from_side": "top", "via": [(mid("sc"), 26), (110, 26)], "to_side": "top"}),
    ("rpc", "consensus", "queue", "submit (TransactionClient) → positions", {"from_side": "bottom", "to_side": "top"}),
    ("rpc", "effwait", "wait", "wait_for_effects: Effects(digest), racing the position's status", {"from_side": "bottom_right", "via": [(190, 150), (1300, 150)], "to_side": "left_at", "to_y": 200}),
    ("effwait", "rpc", "ready", "reply with effects", {"from_side": "top", "via": [(mid("effwait"), 16), (110, 16)], "to_side": "top"}),
    # voting
    ("consensus", "voter", "queue", "blocks", {"from_side": "right_at", "from_y": 245, "to_side": "left_at", "to_y": 245, "label_dy": -11}),
    ("voter", "consensus", "queue", "votes", {"from_side": "left_at", "from_y": 275, "to_side": "right_at", "to_y": 275, "label_dy": 12}),
    # commits
    ("consensus", "decoder", "queue", "commits", {"from_side": "right_at", "from_y": 530, "to_side": "left"}),
    ("decoder", "handler", "queue", "decoded commit", {"to_side": "left_at", "to_y": 530}),
    ("decoder", "sumwait", "wait", "checkpoint signatures: LocalSummary(seq)", {"from_side": "top", "via": [(mid("decoder"), 430), (700, 425), (1950, 425)], "to_side": "left"}),
    ("handler", "resolver", "queue", "chunk txs", {"from_side": "right_at", "from_y": 530, "to_side": "left"}),
    ("handler", "settle", "queue", "chunk settlement", {"from_side": "top", "via": [(mid("handler"), 300), (1500, 300), (1690, 330)], "to_side": "left_at", "to_y": 340}),
    ("handler", "ckwait", "wait", "pending checkpoint: ChunkSettled(h)…", {"from_side": "bottom", "via": [(mid("handler"), 670)], "to_side": "left"}),
    # execution
    ("resolver", "inputwait", "wait", "inputs · Reserved(digest)", {"to_side": "left", "label_dy": -40}),
    ("resolver", "funds", "queue", "reservations", {"from_side": "top", "via": [(mid("resolver"), 350)], "to_side": "left_at", "to_y": 350}),
    ("funds", "inputwait", "notify", "Reserved(digest) → status", {"from_side": "bottom", "to_side": "top", "label_dy": 22}),
    ("exec", "funds", "queue", "deposits · credits", {"from_side": "top_left", "via": [(1430, 400), (1290, 400)], "to_side": "bottom_right"}),
    ("inputwait", "exec", "ready", "txs + input values", {"from_side": "right", "to_side": "left"}),
    ("exec", "inputwait", "notify", "ObjectVersion · Generation(k+1) → objects", {"from_side": "bottom", "via": [(mid("exec"), 620), (mid("inputwait"), 620)], "to_side": "bottom"}),
    ("exec", "effwait", "notify", "Effects(digest)", {"from_side": "top", "to_side": "bottom"}),
    # settlement
    ("effwait", "settle", "ready", "chunk effects", {"from_side": "right", "via": [(1640, 200)], "to_side": "top_left"}),
    ("settle", "effwait", "wait", "chunk txs, settlements: Effects", {"from_side": "top_right", "via": [(1880, 290), (1880, 140), (1560, 140)], "to_side": "top_right"}),
    ("settle", "inputwait", "wait", "settlement, barrier txs", {"from_side": "bottom", "via": [(mid("settle"), 455), (1250, 455)], "to_side": "top_right"}),
    ("settle", "funds", "queue", "Settled(v)", {"from_side": "left_at", "from_y": 350, "to_side": "right_at", "to_y": 350, "label_dy": -18}),
    ("settle", "ckwait", "notify", "ChunkSettled(h)", {"from_side": "bottom_left", "to_side": "top"}),
    # checkpoints: contents at once, the summary after the previous one
    ("ckwait", "contents", "ready", "chunks settled", {"from_side": "bottom", "to_side": "top"}),
    ("contents", "hasher", "queue", "effects", {"from_side": "left", "to_side": "right"}),
    ("contents", "seqwait", "wait", "CheckpointBuilt(seq-1)", {"from_side": "right", "to_side": "left", "label_dy": -42}),
    ("seqwait", "summary", "ready", "previous built", {"from_side": "top", "to_side": "bottom"}),
    ("summary", "seqwait", "notify", "CheckpointBuilt(seq)", {"from_side": "right", "via": [(2250, 670), (2250, 820)], "to_side": "right"}),
    ("summary", "sumwait", "notify", "LocalSummary(seq)", {"from_side": "top", "to_side": "bottom"}),
    ("summary", "consensus", "queue", "signature (CheckpointSignatureV2)", {"from_side": "left_at", "from_y": 690, "via": [(1960, 690), (1960, 930), (200, 930)], "to_side": "right_at", "to_y": 805}),
    ("sumwait", "agg", "ready", "signatures", {"from_side": "top", "to_side": "bottom"}),
    ("agg", "statesync", "queue", "certified checkpoint", {"from_side": "top", "to_side": "bottom"}),
    # stores (straight, beneath the nodes)
    ("voter", "cache", "store", "", {"from_side": "bottom_left", "to_side": "top_left"}),
    ("handler", "statuses", "store", "", {"from_side": "bottom_right", "to_side": "top_left"}),
    ("decoder", "cache", "store", "", {"from_side": "bottom", "to_side": "top"}),
    ("voter", "objects", "store", "", {"from_side": "bottom_right", "to_side": "top_left"}),
    ("sc", "objects", "store", "", {"from_side": "bottom_right", "to_side": "top_right"}),
    ("inputwait", "objects", "check", "", {"from_side": "bottom_left", "to_side": "top_right"}),
    ("inputwait", "gens", "check", "", {"from_side": "bottom_right", "to_side": "top_left"}),
    ("effwait", "objects", "check", "", {"from_side": "bottom_left", "to_side": "top"}),
    ("exec", "objects", "store", "", {"from_side": "bottom_left", "to_side": "top_right"}),
    ("exec", "gens", "store", "", {"from_side": "bottom", "to_side": "top"}),
    ("settle", "chunks", "store", "", {"from_side": "bottom_right", "to_side": "top_right"}),
    ("ckwait", "chunks", "check", "", {"from_side": "bottom_left", "to_side": "top_left"}),
    ("contents", "chunks", "store", "", {"from_side": "bottom", "to_side": "top"}),
    ("summary", "ckstore", "store", "", {"from_side": "bottom_right", "via": [(2140, 760)], "to_side": "top_right"}),
    ("seqwait", "ckstore", "check", "", {"from_side": "bottom", "to_side": "top"}),
]

STYLE = {
    "queue": dict(stroke="#2b2b2b", width=1.6, dash=None, marker="arrow-queue"),
    "wait": dict(stroke="#7a3fc0", width=1.6, dash="7,4", marker="arrow-wait"),
    "notify": dict(stroke="#1a8f3c", width=1.6, dash="2,3", marker="arrow-notify"),
    "ready": dict(stroke="#e07b00", width=2.4, dash=None, marker="arrow-ready"),
    "store": dict(stroke="#a0a0a0", width=1.0, dash=None, marker="arrow-store"),
    "check": dict(stroke="#a0a0a0", width=1.0, dash="3,3", marker="arrow-store"),
}

FILL = {
    "proc": ("#dbe9ff", "#2f62b5"),
    "waiter": ("#ffe7c7", "#d07400"),
    "store": ("#eeeeee", "#7a7a7a"),
    "ext": ("#3b3f46", "#1e2024"),
}


def center(n):
    _, _, x, y, w, h = NODES[n]
    return x + w / 2, y + h / 2


def anchor(n, toward, side=None, at=None):
    """A point on node n's border facing `toward`, or on the side named."""
    _, _, x, y, w, h = NODES[n]
    cx, cy = x + w / 2, y + h / 2
    sides = {
        "left": (x, cy),
        "right": (x + w, cy),
        "top": (cx, y),
        "bottom": (cx, y + h),
        "top_left": (x + w * 0.25, y),
        "top_right": (x + w * 0.75, y),
        "bottom_left": (x + w * 0.25, y + h),
        "bottom_right": (x + w * 0.75, y + h),
        "left_at": (x, at),
        "right_at": (x + w, at),
    }
    if side in sides:
        return sides[side]
    tx, ty = toward
    dx, dy = tx - cx, ty - cy
    if dx == 0 and dy == 0:
        return cx, cy
    sx = (w / 2) / abs(dx) if dx else float("inf")
    sy = (h / 2) / abs(dy) if dy else float("inf")
    s = min(sx, sy)
    return cx + dx * s, cy + dy * s


def node_svg(n):
    lines, kind, x, y, w, h = NODES[n]
    fill, stroke = FILL[kind]
    out = []
    if kind == "proc":
        out.append(f'<rect x="{x}" y="{y}" width="{w}" height="{h}" rx="10" fill="{fill}" stroke="{stroke}" stroke-width="1.6"/>')
    elif kind == "ext":
        out.append(f'<rect x="{x}" y="{y}" width="{w}" height="{h}" rx="4" fill="{fill}" stroke="{stroke}" stroke-width="1.6"/>')
    elif kind == "waiter":
        c = 16
        pts = [(x + c, y), (x + w - c, y), (x + w, y + h / 2), (x + w - c, y + h), (x + c, y + h), (x, y + h / 2)]
        out.append('<polygon points="%s" fill="%s" stroke="%s" stroke-width="1.8"/>' % (" ".join(f"{a},{b}" for a, b in pts), fill, stroke))
    elif kind == "store":
        e = 9
        out.append(f'<path d="M{x},{y+e} a{w/2},{e} 0 0,1 {w},0 v{h-2*e} a{w/2},{e} 0 0,1 -{w},0 z" fill="{fill}" stroke="{stroke}" stroke-width="1.4"/>')
        out.append(f'<path d="M{x},{y+e} a{w/2},{e} 0 0,0 {w},0" fill="none" stroke="{stroke}" stroke-width="1.4"/>')
    color = "#ffffff" if kind == "ext" else "#14213d"
    n_lines = len(lines)
    top = y + h / 2 - (n_lines - 1) * 8 + 4
    for i, line in enumerate(lines):
        weight = "700" if i == 0 else "400"
        size = 14 if i == 0 else 11.5
        out.append(f'<text x="{x + w/2}" y="{top + i*16}" text-anchor="middle" font-size="{size}" font-weight="{weight}" fill="{color}">{escape(line)}</text>')
    return "\n".join(out)


def edge_svg(a, b, kind, label, opts):
    st = STYLE[kind]
    via = opts.get("via", [])
    first_target = via[0] if via else center(b)
    last_source = via[-1] if via else center(a)
    p0 = anchor(a, first_target, opts.get("from_side"), opts.get("from_y"))
    p1 = anchor(b, last_source, opts.get("to_side"), opts.get("to_y"))
    pts = [p0] + via + [p1]
    # A smooth path through the points: quadratic segments via midpoints.
    if len(pts) == 2:
        d = f"M{pts[0][0]:.1f},{pts[0][1]:.1f} L{pts[1][0]:.1f},{pts[1][1]:.1f}"
    else:
        d = f"M{pts[0][0]:.1f},{pts[0][1]:.1f}"
        for i in range(1, len(pts) - 1):
            mx = (pts[i][0] + pts[i + 1][0]) / 2
            my = (pts[i][1] + pts[i + 1][1]) / 2
            if i == len(pts) - 2:
                mx, my = pts[i + 1]
            d += f" Q{pts[i][0]:.1f},{pts[i][1]:.1f} {mx:.1f},{my:.1f}"
    dash = f' stroke-dasharray="{st["dash"]}"' if st["dash"] else ""
    out = [f'<path d="{d}" fill="none" stroke="{st["stroke"]}" stroke-width="{st["width"]}"{dash} marker-end="url(#{st["marker"]})"/>']
    if label:
        # The label at the middle of the polyline.
        seg = max(range(len(pts) - 1), key=lambda i: (pts[i + 1][0] - pts[i][0]) ** 2 + (pts[i + 1][1] - pts[i][1]) ** 2)
        lx = (pts[seg][0] + pts[seg + 1][0]) / 2
        ly = (pts[seg][1] + pts[seg + 1][1]) / 2 + opts.get("label_dy", 0)
        tw = 6.3 * len(label) + 10
        out.append(f'<rect x="{lx - tw/2:.1f}" y="{ly - 9:.1f}" width="{tw:.1f}" height="16" rx="3" fill="#ffffff" fill-opacity="0.92" stroke="{st["stroke"]}" stroke-width="0.6"/>')
        out.append(f'<text x="{lx:.1f}" y="{ly + 3:.1f}" text-anchor="middle" font-size="11" fill="{st["stroke"]}">{escape(label)}</text>')
    return "\n".join(out)


def legend_svg():
    x, y = 30, 1090
    items = [
        ("queue", "queue (work item)"),
        ("wait", "wait for (items + keys)"),
        ("notify", "notify key"),
        ("ready", "ready items"),
        ("store", "store read / write"),
        ("check", "availability check"),
    ]
    out = [f'<text x="{x}" y="{y - 6}" font-size="13" font-weight="700" fill="#14213d">Edges</text>']
    for i, (kind, text) in enumerate(items):
        st = STYLE[kind]
        ix = x + (i % 3) * 230
        iy = y + (i // 3) * 26 + 10
        dash = f' stroke-dasharray="{st["dash"]}"' if st["dash"] else ""
        out.append(f'<line x1="{ix}" y1="{iy}" x2="{ix + 46}" y2="{iy}" stroke="{st["stroke"]}" stroke-width="{st["width"]}"{dash} marker-end="url(#{st["marker"]})"/>')
        out.append(f'<text x="{ix + 56}" y="{iy + 4}" font-size="12" fill="#333">{escape(text)}</text>')
    nx = x + 720
    out.append(f'<text x="{nx}" y="{y - 6}" font-size="13" font-weight="700" fill="#14213d">Nodes</text>')
    for i, (kind, text) in enumerate([("proc", "processor"), ("waiter", "waiter"), ("store", "store / table"), ("ext", "black box")]):
        fill, stroke = FILL[kind]
        ix = nx + i * 150
        iy = y + 2
        if kind == "waiter":
            out.append(f'<polygon points="{ix+6},{iy} {ix+34},{iy} {ix+40},{iy+11} {ix+34},{iy+22} {ix+6},{iy+22} {ix},{iy+11}" fill="{fill}" stroke="{stroke}"/>')
        elif kind == "store":
            out.append(f'<rect x="{ix}" y="{iy}" width="40" height="22" rx="8" fill="{fill}" stroke="{stroke}"/>')
        else:
            out.append(f'<rect x="{ix}" y="{iy}" width="40" height="22" rx="{6 if kind=="proc" else 2}" fill="{fill}" stroke="{stroke}"/>')
        out.append(f'<text x="{ix + 48}" y="{iy + 15}" font-size="12" fill="#333">{escape(text)}</text>')
    return "\n".join(out)


def marker(id_, color):
    return f'<marker id="{id_}" viewBox="0 0 10 10" refX="9" refY="5" markerWidth="7" markerHeight="7" orient="auto-start-reverse"><path d="M0,0 L10,5 L0,10 z" fill="{color}"/></marker>'


def main(path):
    parts = [
        f'<svg xmlns="http://www.w3.org/2000/svg" width="{W}" height="{H}" viewBox="0 0 {W} {H}" font-family="Helvetica, Arial, sans-serif">',
        "<title>Validator pipeline: processors and waiters</title>",
        "<defs>",
        marker("arrow-queue", "#2b2b2b"),
        marker("arrow-wait", "#7a3fc0"),
        marker("arrow-notify", "#1a8f3c"),
        marker("arrow-ready", "#e07b00"),
        marker("arrow-store", "#a0a0a0"),
        "</defs>",
        f'<rect width="{W}" height="{H}" fill="#ffffff"/>',
    ]
    # Stores' edges first, so they sit beneath.
    order = sorted(EDGES, key=lambda e: 0 if e[2] in ("store", "check") else 1)
    for a, b, kind, label, opts in order:
        parts.append(edge_svg(a, b, kind, label, opts))
    for n in NODES:
        parts.append(node_svg(n))
    parts.append(legend_svg())
    parts.append("</svg>")
    with open(path, "w") as f:
        f.write("\n".join(parts) + "\n")


if __name__ == "__main__":
    main(sys.argv[1])
