#!/usr/bin/env python3
"""Sidebar overlap regression check for the macOS mock screenshot.

Reference-free and resolution-independent: finds the app window by its
traffic lights, bounds the sidebar at the split divider, extracts text
baselines from a bright-pixel density profile, and asserts the roster
renders as distinct rows (no merged/overprinted lines, no missing or
duplicated rows).

Row text is indented past the glyph/dot markers while headers hug the
sidebar edge, so a group only counts as a row when its inset reaches
into the row band (relative to the band width, so 1x and 2x captures
behave the same). A row is normally a two-line title/subtitle pair;
when a descender bridges the pair into one tall blob it still counts
by height. Single-line groups (headers, filter, buttons) are ignored;
any row group of three or more lines is overprinting. Detached dots and descenders rejoin first at a fraction
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
    """Find the sidebar split divider: a vertical edge spanning height.

    Modern sidebar chrome is a faint hairline (a brightness step, not a
    dark line), so score columns by sustained horizontal gradient first
    and keep the old mid-bright scan as a fallback.
    """
    lo = win_left + 120
    hi = min(win_left + 700, w - 10)
    if hi <= lo:
        fail_infra("window too narrow for a sidebar band")
    data = list(gray.getdata())
    y0, y1 = 120, h - 30
    if y1 <= y0:
        fail_infra("screenshot too short")
    span = y1 - y0
    best_x, best_score = -1, 0.0
    for x in range(lo + 1, hi - 1):
        edge = 0
        for y in range(y0, y1):
            if abs(data[y * w + x - 1] - data[y * w + x + 1]) >= 4:
                edge += 1
        score = edge / span
        if score > best_score:
            best_score, best_x = score, x
    if best_score >= 0.45:
        return best_x, best_score
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


def window_bottom(gray, w, x0, x1, y0, y1):
    """Bottom edge of the app window: first sustained drop to desktop.

    Keeps the dock (and anything below the window) out of the sidebar
    band. Relative to the window interior, so light and dark chrome
    both work; falls back to the full height when no edge is found.
    """
    data = list(gray.getdata())
    width = max(1, x1 - x0)

    def row_mean(y):
        return sum(data[y * w + x] for x in range(x0, x1)) / width

    # Reference from the window interior (first 100 rows are always
    # chrome + sidebar), halved as the edge threshold so light and
    # dark chrome both work; absolute 60 caps it on bright themes.
    ref_rows = [row_mean(y) for y in range(y0, min(y0 + 100, y1))]
    ref_rows.sort()
    threshold = min(60.0, ref_rows[len(ref_rows) // 2] * 0.5)
    dark_run = 0
    for y in range(y0 + 100, y1):
        if row_mean(y) < threshold:
            dark_run += 1
            if dark_run >= 20:
                return y - dark_run + 1
        else:
            dark_run = 0
    return y1


def text_lines(band, density, repair_ratio, light_bg):
    """Text runs as (y0, y1, min_x) triples.

    The band background sets the polarity: dark glyphs on a light
    sidebar, bright glyphs on a dark one. Detached dots and descenders
    rejoin first: gaps at or below a fraction of the row pitch are
    glyph fragments, not line breaks.
    """
    w, h = band.size
    data = list(band.getdata())

    def is_text(v):
        # Thresholds sit between the sidebar chrome and the glyphs:
        # light sidebar (~242, hairline above it) vs primary/secondary/
        # tertiary glyphs (at or below ~200); dark sidebar vs glyphs
        # above 100. Both polarities keep headers left of the row inset.
        return v < 200 if light_bg else v > 100

    rows = []
    for y in range(h):
        base = y * w
        runs = []
        x = 0
        while x < w:
            if is_text(data[base + x]):
                s = x
                while x < w and is_text(data[base + x]):
                    x += 1
                runs.append((s, x - s))
            else:
                x += 1
        # Window chrome touching the band edge (frame/shadow) is not
        # text: drop runs starting at x=0 so they neither trigger rows
        # nor drag min_x to the edge.
        runs = [r for r in runs if r[0] > 0]
        bright = sum(r[1] for r in runs)
        min_x = runs[0][0] if runs else w
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


def analyze_lines(lines, join_ratio, inset_cut):
    """Group text lines into sidebar rows.

    Returns (multis, others, grouped, pitch, join, tall_single) where
    multis are two-line rows (or tall singles: a title/subtitle pair
    bridged by a descender) and others are 3+-line groups, i.e.
    vertically overprinted rows. Every group is kept for annotation.
    """
    gaps = [lines[i + 1][0] - lines[i][1] for i in range(len(lines) - 1)]
    pitch = sorted(gaps)[3 * len(gaps) // 4]
    join = max(2.0, join_ratio * pitch)
    tall = tall_single_for(lines)

    grouped = []
    cur = [lines[0]]
    for ln in lines[1:]:
        if ln[0] - cur[-1][1] < join:
            cur.append(ln)
        else:
            grouped.append(cur)
            cur = [ln]
    grouped.append(cur)

    multis = [g for g in grouped
              if (len(g) == 2 and is_row(g, inset_cut))
              or is_tall_single(g, inset_cut, tall)]
    others = [g for g in grouped
              if len(g) >= 3 and is_row(g, inset_cut)]
    return multis, others, grouped, pitch, join, tall


def is_row(g, inset_cut):
    return min(ln[2] for ln in g) >= inset_cut


def tall_single_for(lines):
    # Tall-blob reference: the first three lines are always chrome
    # (button, filter, first header) at single-line height, so their
    # median anchors the row height no matter how many rows follow.
    chrome = sorted(ln[1] - ln[0] + 1 for ln in lines[:3])
    return 1.7 * chrome[len(chrome) // 2]


def is_tall_single(g, inset_cut, tall_single):
    return (len(g) == 1
            and g[0][1] - g[0][0] + 1 >= tall_single
            and is_row(g, inset_cut))


def main():
    ap = argparse.ArgumentParser(description="sidebar overlap check")
    ap.add_argument("png")
    ap.add_argument("--expect-rows", type=int, default=3)
    ap.add_argument("--annotate", default=None)
    ap.add_argument("--density", type=float, default=0.008)
    ap.add_argument("--join-ratio", type=float, default=0.65)
    ap.add_argument("--repair-ratio", type=float, default=0.25)
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
    bottom = window_bottom(gray, w, x0, x1, y0, h - 10)
    band = gray.crop((x0, y0, x1, bottom))
    inset_cut = ROW_INSET_RATIO * band.size[0]

    # Sidebar polarity follows the system appearance: the band mean
    # says whether glyphs read dark-on-light or light-on-dark.
    pixels = list(band.getdata())
    light_bg = sum(pixels) / max(1, len(pixels)) > 128

    lines = text_lines(band, args.density, args.repair_ratio, light_bg)
    if len(lines) < 2:
        print("sidebar_shot_check FAIL: only %d text line(s) in sidebar "
              "(window=(%d,%d) divider=%d)" % (len(lines), win_left,
                                               win_top, divider))
        sys.exit(1)
    multis, others, grouped, pitch, join, tall_single = analyze_lines(
        lines, args.join_ratio, inset_cut)

    print("sidebar_shot_check: window=(%d,%d) divider=%d(score %.2f) "
          "band=%dx%d lines=%d groups=%d multis=%d others=%d "
          "p75_gap=%.1f join<%.1f inset_cut=%.1f tall>=%.1f"
          % (win_left, win_top, divider, divider_score, band.size[0],
             band.size[1], len(lines), len(grouped), len(multis),
             len(others), pitch, join, inset_cut, tall_single))
    for g in grouped:
        print("  group y=%d..%d nlines=%d row=%s" % (y0 + g[0][0],
                                                     y0 + g[-1][1], len(g),
                                                     is_row(g, inset_cut)))

    if args.annotate:
        out = img.convert("RGB")
        draw = ImageDraw.Draw(out)
        for g in grouped:
            if ((len(g) == 2 and is_row(g, inset_cut))
                    or is_tall_single(g, inset_cut, tall_single)):
                color = (80, 200, 80)
            elif len(g) >= 3 and is_row(g, inset_cut):
                color = (230, 70, 70)
            else:
                color = (220, 180, 60)
            draw.rectangle([x0, y0 + g[0][0], x1, y0 + g[-1][1]],
                           outline=color, width=3)
        out.save(args.annotate)
        print("sidebar_shot_check: annotated " + args.annotate)

    problems = []
    if len(multis) != args.expect_rows:
        problems.append("expected %d rows, found %d"
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
