#!/usr/bin/env python3
"""Sidebar overlap regression check for the macOS mock screenshot.

Reference-free and resolution-independent: finds the app window by its
traffic lights, bounds the sidebar at the split divider, extracts text
baselines from a bright-pixel density profile, and asserts the roster
renders as distinct two-line rows (no merged/overprinted lines, no
missing or duplicated rows).

Row text is indented past the glyph/dot markers while headers hug the
sidebar edge, so a two-line group only counts as a row when its inset
reaches into the row band (relative to the band width, so 1x and 2x
captures behave the same). Single-line groups (headers, filter,
buttons) are ignored; any row group of three or more lines is
overprinting. Detached dots and descenders rejoin first at a fraction
of the row pitch; row grouping keys off the 75th percentile gap, so a
few torn lines cannot drag the thresholds.

Known blind spot: two titles overprinted at the exact same baseline and
inset are one bright line to any profile method (needs OCR), as is a
tear smaller than a glyph fragment; torn rows in practice displace by
half a line or more and are caught.

Usage:
    python3 sidebar_shot_check.py <png> [--expect-rows N]
        [--annotate out.png] [--density F] [--join-ratio F]
        [--repair-ratio F]

Exit 0 when healthy, 1 with diagnostics on a layout failure, 2 on an
infrastructure failure (window/divider not found).
"""

import argparse
import sys

try:
    from PIL import Image, ImageDraw
except ImportError:  # pragma: no cover
    print("sidebar_shot_check: pillow is required (pip install pillow)",
          file=sys.stderr)
    sys.exit(2)

# Minimum relative inset for row text (headers sit left of this).
ROW_INSET_RATIO = 0.08


def fail_infra(msg):
    print("sidebar_shot_check INFRA-FAIL: " + msg, file=sys.stderr)
    sys.exit(2)


def find_window(rgb, w, h):
    """Locate the app window by its red-yellow-green traffic lights."""
    px = rgb.load()
    top = min(220, h // 4)
    for y in range(top):
        rx = yx = -1
        x = 0
        while x < w:
            r, g, b = px[x, y]
            if r > 170 and g < 120 and b < 120:
                run = 0
                while x + run < w:
                    r2, g2, b2 = px[x + run, y]
                    if r2 > 170 and g2 < 120 and b2 < 120:
                        run += 1
                    else:
                        break
                if run >= 3:
                    rx = x
                    break
                x += run
            x += 1
        if rx < 0:
            continue
        for name, cond in (
            ("yellow", lambda c: c[0] > 150 and c[1] > 130 and c[2] < 130),
            ("green", lambda c: c[1] > 140 and c[0] < 130),
        ):
            found = -1
            base = rx if name == "yellow" else yx
            for x in range(base + 8, min(base + 140, w)):
                if cond(px[x, y]):
                    run = 0
                    while x + run < w and cond(px[x + run, y]):
                        run += 1
                    if run >= 3:
                        found = x
                        break
                    x += run
            if found < 0:
                break
            if name == "yellow":
                yx = found
        else:
            return max(0, rx - 34), max(0, y - 34)
    fail_infra("traffic lights not found (is the app window visible?)")
    return 0, 0  # unreachable


def find_divider(gray, w, h, win_left):
    """Find the sidebar split divider: a mid-bright column spanning height."""
    lo = win_left + 120
    hi = min(win_left + 700, w - 10)
    if hi <= lo:
        fail_infra("window too narrow for a sidebar band")
    data = list(gray.getdata())
    best_x, best_score = -1, 0.0
    y0, y1 = 120, h - 30
    if y1 <= y0:
        fail_infra("screenshot too short")
    span = y1 - y0
    for x in range(lo, hi):
        mid = 0
        for y in range(y0, y1):
            v = data[y * w + x]
            if 30 < v < 120:
                mid += 1
        score = mid / span
        if score > best_score:
            best_score, best_x = score, x
    if best_score < 0.45:
        fail_infra("split divider not found (score %.2f)" % best_score)
    return best_x, best_score


def text_lines(band, density, repair_ratio):
    """Bright runs as (y0, y1, min_x) triples.

    Detached dots and descenders rejoin first: gaps at or below a
    fraction of the row pitch are glyph fragments, not line breaks.
    """
    w, h = band.size
    data = list(band.getdata())
    rows = []
    for y in range(h):
        bright = 0
        min_x = w
        base = y * w
        for x in range(w):
            if data[base + x] > 100:
                bright += 1
                if x < min_x:
                    min_x = x
        if bright / w >= density:
            rows.append((y, min_x))
    raw = []
    for y, min_x in rows:
        if raw and y - raw[-1][1] <= 1:
            raw[-1][1] = y
            if min_x < raw[-1][2]:
                raw[-1][2] = min_x
        else:
            raw.append([y, y, min_x])
    if len(raw) < 3:
        return [(a, b, m) for a, b, m in raw]
    gaps = sorted(raw[i + 1][0] - raw[i][1] for i in range(len(raw) - 1))
    pitch = gaps[3 * len(gaps) // 4]
    repair = max(2.0, repair_ratio * pitch)
    lines = [raw[0]]
    for ln in raw[1:]:
        if ln[0] - lines[-1][1] <= repair:
            lines[-1][1] = ln[1]
            if ln[2] < lines[-1][2]:
                lines[-1][2] = ln[2]
        else:
            lines.append(ln)
    return [(a, b, m) for a, b, m in lines]


def main():
    ap = argparse.ArgumentParser(description="sidebar overlap check")
    ap.add_argument("png")
    ap.add_argument("--expect-rows", type=int, default=3)
    ap.add_argument("--annotate", default=None)
    ap.add_argument("--density", type=float, default=0.008)
    ap.add_argument("--join-ratio", type=float, default=0.7)
    ap.add_argument("--repair-ratio", type=float, default=0.15)
    args = ap.parse_args()

    img = Image.open(args.png).convert("RGB")
    w, h = img.size
    gray = img.convert("L")

    win_left, win_top = find_window(img, w, h)
    divider, divider_score = find_divider(gray, w, h, win_left)
    x0 = win_left + 8
    x1 = divider - 8
    y0 = win_top + 70
    if x1 - x0 < 80:
        fail_infra("sidebar band too narrow (%dpx)" % (x1 - x0))
    band = gray.crop((x0, y0, x1, h - 10))
    inset_cut = ROW_INSET_RATIO * band.size[0]

    lines = text_lines(band, args.density, args.repair_ratio)
    if len(lines) < 2:
        print("sidebar_shot_check FAIL: only %d text line(s) in sidebar "
              "(window=(%d,%d) divider=%d)" % (len(lines), win_left,
                                               win_top, divider))
        sys.exit(1)
    gaps = [lines[i + 1][0] - lines[i][1] for i in range(len(lines) - 1)]
    pitch = sorted(gaps)[3 * len(gaps) // 4]
    join = max(2.0, args.join_ratio * pitch)

    grouped = []
    cur = [lines[0]]
    for ln in lines[1:]:
        if ln[0] - cur[-1][1] < join:
            cur.append(ln)
        else:
            grouped.append(cur)
            cur = [ln]
    grouped.append(cur)

    def is_row(g):
        return min(ln[2] for ln in g) >= inset_cut

    multis = [g for g in grouped if len(g) == 2 and is_row(g)]
    others = [g for g in grouped if len(g) >= 3 and is_row(g)]

    print("sidebar_shot_check: window=(%d,%d) divider=%d(score %.2f) "
          "band=%dx%d lines=%d groups=%d multis=%d others=%d "
          "p75_gap=%.1f join<%.1f inset_cut=%.1f"
          % (win_left, win_top, divider, divider_score, band.size[0],
             band.size[1], len(lines), len(grouped), len(multis),
             len(others), pitch, join, inset_cut))
    for g in grouped:
        print("  group y=%d..%d nlines=%d row=%s" % (y0 + g[0][0],
                                                     y0 + g[-1][1], len(g),
                                                     is_row(g)))

    if args.annotate:
        out = img.convert("RGB")
        draw = ImageDraw.Draw(out)
        for g in grouped:
            if len(g) == 2 and is_row(g):
                color = (80, 200, 80)
            elif len(g) >= 3 and is_row(g):
                color = (230, 70, 70)
            else:
                color = (220, 180, 60)
            draw.rectangle([x0, y0 + g[0][0], x1, y0 + g[-1][1]],
                           outline=color, width=3)
        out.save(args.annotate)
        print("sidebar_shot_check: annotated " + args.annotate)

    problems = []
    if len(multis) != args.expect_rows:
        problems.append("expected %d two-line rows, found %d"
                        % (args.expect_rows, len(multis)))
    if others:
        problems.append("%d row group(s) with 3+ lines (overprinted?)"
                        % len(others))
    if problems:
        print("sidebar_shot_check FAIL: " + "; ".join(problems))
        sys.exit(1)
    print("sidebar_shot_check PASS")
    sys.exit(0)


if __name__ == "__main__":
    main()
