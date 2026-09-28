"""Build QuotaDeck's eight-glyph WezTerm fallback.

Usage: python scripts/build-icon-font.py /path/to/HerdrAgentIconsMax-Regular.ttf
Requires fonttools. The input is MIT-licensed qintmb/herdr-icon-agent-ui.
"""
import sys
from pathlib import Path
from fontTools import subset
from fontTools.ttLib import TTFont
from fontTools.pens.boundsPen import BoundsPen
from fontTools.pens.ttGlyphPen import TTGlyphPen
from fontTools.pens.cu2quPen import Cu2QuPen
from fontTools.pens.transformPen import TransformPen
from fontTools.svgLib.path import parse_path
from fontTools.pens.recordingPen import RecordingPen
from xml.etree import ElementTree

root = Path(__file__).resolve().parent.parent
codes = [0xE1A0, 0xE1A1, 0xE1A2, 0xE1A3, 0xE1AA, 0xE1AE, 0xE1B1]
font = TTFont(sys.argv[1], recalcTimestamp=False)
assert font['head'].unitsPerEm == 1000
assert all(c in font.getBestCmap() for c in codes)
options = subset.Options()
options.name_IDs = ['*']
subsetter = subset.Subsetter(options=options)
subsetter.populate(unicodes=codes)
subsetter.subset(font)
# OpenRouter is not present in the upstream font; use our existing CC0 path.
# It goes to U+E500, well clear of the upstream font's sequential run from
# U+E1A0: a slot next to that run is taken by whatever logo upstream adds
# next, and a terminal that loads the upstream font first would draw that.
OPENROUTER = 0xE500
svg = RecordingPen()
# Take only the mark: the documentation SVG also has a background rectangle.
path = ElementTree.parse(root / 'docs/icons/openrouter.svg').find('.//{http://www.w3.org/2000/svg}path')
parse_path(path.attrib['d'], svg)
bounds = BoundsPen(None)
svg.replay(bounds)
x0, y0, x1, y1 = bounds.bounds
scale = min(560 / (x1-x0), 760 / (y1-y0))
pen = TTGlyphPen(None)
svg.replay(TransformPen(Cu2QuPen(pen, 1.0, reverse_direction=True),
    (scale, 0, 0, -scale, (600-(x1-x0)*scale)/2-x0*scale, 365+(y1-y0)*scale/2+y0*scale)))
name = 'openrouter'
font.setGlyphOrder(font.getGlyphOrder() + [name])
font['glyf'][name] = pen.glyph()
font['hmtx'][name] = (600, 0)
for table in font['cmap'].tables:
    if table.isUnicode():
        table.cmap[OPENROUTER] = name
for record in font['name'].names:
    replacements = {1:'QuotaDeck Icons', 2:'Regular', 3:'QuotaDeckIcons-1.0',
        4:'QuotaDeck Icons Regular', 6:'QuotaDeckIcons-Regular', 16:'QuotaDeck Icons', 17:'Regular'}
    if record.nameID in replacements:
        record.string = replacements[record.nameID].encode(record.getEncoding())
output = root / 'docs/icons/QuotaDeckIcons-Regular.ttf'
font.save(output)
check = TTFont(output)
assert set(check.getBestCmap()) == set(codes + [OPENROUTER])
assert all(check['glyf'][n].numberOfContours > 0 for n in check.getBestCmap().values())
print('Verified all eight provider glyphs:', output.name)
