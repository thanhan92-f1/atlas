# Social assets

The README hero and the GitHub social preview. Two variants, one palette.

| File | What it is |
|---|---|
| `atlas-share-card.svg` / `.png` / `@2x.png` | Light card, 1200x630 (and 2400x1260). The README hero and the file to upload as the GitHub social preview |
| `atlas-share-card-dark.svg` / `.png` / `@2x.png` | Dark card for GitHub's dark theme, swapped in by the README `<picture>` block |
| `build-share-cards.py` | Generates both SVGs from one palette |

## Palette

| | Light | Dark |
|---|---|---|
| Background | `#ffffff` to `#f5f5f7`, faint blue wash | `#000000` to `#0b0b0f`, blue wash |
| Text | `#1d1d1f`, secondary `#6e6e73` | `#f5f5f7`, secondary `#a1a1a6` |
| Cards | white, hairline `#d2d2d7` | `#1c1c1e`, hairline `#3a3a3c` |
| Accent | blue `#0071e3` to `#2997ff` | blue `#0a84ff` to `#5eb0ff` |

Orange (`#ff6a2a`) appears once, as a small dot on the Ceph card (the primary backend), and nowhere else.
Type is Helvetica Neue and Menlo.

## Rebuild

```bash
python3 docs/social/build-share-cards.py docs/social
for v in "" "-dark"; do
  rsvg-convert -w 1200 docs/social/atlas-share-card$v.svg -o docs/social/atlas-share-card$v.png
  rsvg-convert -w 2400 docs/social/atlas-share-card$v.svg -o docs/social/atlas-share-card$v@2x.png
done
```

Needs `rsvg-convert` (librsvg) and Python 3. Edit the stats or the backend list in `build-share-cards.py`;
the numbers must match the code (see the README) and the licence line must match `LICENSE`.

## GitHub social preview

The repository's Social preview image cannot be set through the GitHub API or `gh`. After changing the card,
upload `atlas-share-card.png` by hand under Settings > General > Social preview.
