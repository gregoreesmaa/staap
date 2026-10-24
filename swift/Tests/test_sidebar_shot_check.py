"""Unit tests for sidebar_shot_check.py (the macOS sidebar overlap gate).

The script itself runs against CI captures; these tests pin its building
blocks with synthetic images so the gate cannot silently rot: if row
grouping, divider finding, or polarity handling breaks, the suite fails
long before a bad capture ships.

Run:  python3 -m unittest discover -s swift/Tests -p "test_*.py"
Requires Pillow (same dependency as the check itself).
"""

import os
import sys
import unittest

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

from PIL import Image

import sidebar_shot_check as chk


def band_image(width, height, bg, bars):
    """Grayscale sidebar band: flat bg with dark text bars.

    bars: list of (y0, y1, min_x) text runs; overlapping runs merge.
    """
    img = Image.new("L", (width, height), bg)
    px = img.load()
    for y0, y1, min_x in bars:
        for y in range(y0, min(y1 + 1, height)):
            for x in range(min_x, width):
                px[x, y] = 0
    return img


def three_row_bars():
    """Button + search + header chrome, then three two-line rows.

    Row 2's title carries a descender that bridges into its subtitle,
    so it renders as one tall blob instead of two lines.
    """
    return [
        (12, 22, 25),    # new-session button
        (30, 38, 25),    # search field
        (48, 56, 25),    # section header
        (66, 74, 60),    # row 1 title
        (73, 78, 27),    # row 1 glyph/dot rows
        (82, 88, 60),    # row 1 subtitle
        (98, 108, 60),   # row 2 title
        (106, 111, 27),  # row 2 glyph/dot rows
        (112, 113, 65),  # row 2 descender bridging into the subtitle
        (114, 120, 60),  # row 2 subtitle
        (150, 158, 60),  # row 3 title
        (157, 162, 27),  # row 3 glyph/dot rows
        (166, 172, 60),  # row 3 subtitle
    ]


class DividerTests(unittest.TestCase):
    def test_light_hairline_found_without_dark_line(self):
        # Light chrome: sidebar 242, detail 255, faint gradient edge, no
        # dark spanning column at all (the old mid-gray scan scored ~0.06
        # here and infra-failed every light capture).
        w, h = 400, 300
        img = Image.new("L", (w, h), 255)
        px = img.load()
        for y in range(h):
            for x in range(150):
                px[x, y] = 242
        for i, v in enumerate((244, 247, 250, 253)):
            for y in range(h):
                px[150 + i, y] = v
        x, score = chk.find_divider(img, w, h, 0)
        self.assertGreaterEqual(score, 0.45)
        self.assertGreaterEqual(x, 145)
        self.assertLessEqual(x, 160)

    def test_window_narrow_infra_fails(self):
        img = Image.new("L", (100, 100), 200)
        with self.assertRaises(SystemExit):
            chk.find_divider(img, 100, 100, 90)


class WindowBottomTests(unittest.TestCase):
    def test_dock_excluded(self):
        # Window (bright) rows 100..300, black desktop, dock-colored
        # strip below: the bottom edge must land at the window, not the
        # dock, or dock rows pollute the band.
        w, h = 400, 400
        img = Image.new("L", (w, h), 0)
        px = img.load()
        for y in range(100, 300):
            for x in range(400):
                px[x, y] = 240
        for y in range(330, 380):
            for x in range(400):
                px[x, y] = 150
        bottom = chk.window_bottom(img, w, 60, 200, 110, 390)
        self.assertGreaterEqual(bottom, 295)
        self.assertLessEqual(bottom, 315)


class RowModelTests(unittest.TestCase):
    def setUp(self):
        self.width = 139
        self.inset = 0.08 * self.width

    def rows(self, bars, light=True):
        band = band_image(self.width, 190, 242, bars)
        lines = chk.text_lines(band, 0.008, 0.25, light)
        return chk.analyze_lines(lines, 0.65, self.inset)

    def test_three_rows_count(self):
        multis, others, _, _, _, _ = self.rows(three_row_bars())
        self.assertEqual(len(multis), 3)
        self.assertEqual(others, [])

    def test_overprinted_rows_flagged(self):
        # Three rows smeared into one tall blob (cells painted on top of
        # each other): must NOT pass as three clean rows.
        bars = [(12, 22, 25), (30, 38, 25), (48, 56, 25), (66, 126, 60)]
        multis, others, _, _, _, _ = self.rows(bars)
        self.assertTrue(others or len(multis) != 3)

    def test_missing_row_flagged(self):
        bars = [
            (12, 22, 25), (30, 38, 25), (48, 56, 25),
            (66, 79, 60), (82, 88, 60),
            (98, 111, 60), (114, 120, 60),
        ]
        multis, others, _, _, _, _ = self.rows(bars)
        self.assertEqual(len(multis), 2)
        self.assertEqual(others, [])

    def test_dark_mode_polarity(self):
        # Dark chrome: bright glyphs on a dark band take the >100 path.
        img = Image.new("L", (self.width, 190), 45)
        px = img.load()
        for y in range(20, 30):
            for x in range(60, 100):
                px[x, y] = 230
        for y in range(33, 39):
            for x in range(60, 100):
                px[x, y] = 230
        lines = chk.text_lines(img, 0.008, 0.25, False)
        self.assertEqual(len(lines), 2)
        self.assertEqual(lines[0][2], 60)

    def test_frame_edge_runs_ignored(self):
        # A dark window frame touching the band edge must not create
        # phantom rows nor drag min_x to the edge.
        img = Image.new("L", (self.width, 190), 242)
        px = img.load()
        for y in range(190):
            px[0, y] = 10
            px[1, y] = 12
        for y in range(20, 30):
            for x in range(60, 100):
                px[x, y] = 0
        lines = chk.text_lines(img, 0.008, 0.25, True)
        self.assertEqual(len(lines), 1)
        self.assertGreater(lines[0][2], 0)


class AnalyzeLinesTests(unittest.TestCase):
    def test_tall_single_counts(self):
        # A descender-bridged title+subtitle renders as one tall blob;
        # with chrome anchoring the height reference it still counts.
        lines = [(12, 22, 25), (30, 38, 25), (48, 56, 25),
                 (66, 79, 60), (82, 88, 60),
                 (98, 122, 60)]
        multis, others, _, _, _, _ = chk.analyze_lines(lines, 0.65, 11.1)
        self.assertEqual(len(multis), 2)
        self.assertEqual(others, [])


if __name__ == "__main__":
    unittest.main()
