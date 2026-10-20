# README media

The README's first screen is its only chance: a chat bot has to be *seen* before
it is read. This directory holds the screenshots and the hero GIF; the README
references them by relative path, and the markup for the hero slot is already in
`README.md` as a comment — uncomment it once the file exists.

Nothing here yet. Record the set below, drop the files in, and delete this
paragraph.

## Shot list

| File | What it shows | Format |
| --- | --- | --- |
| `turn-card.gif` | **Hero.** 15–20 s of one real turn: ask → reasoning and tool panels stream in → answer → permission card → tap 「允许」 → done with the change index. | phone screen recording → GIF, 340 px wide, ≤ 8 MB |
| `permission-card.png` | A permission request card with 「允许 / 总是允许 / 拒绝」 visible. | phone, 2× scale |
| `question-card.png` | A question card with multi-select and a custom answer filled in. | phone, 2× scale |
| `file-viewer.png` | `/cat src/bridge/pollers.rs` — the file card with its pager and view toggle. | phone, 2× scale |
| `turn-diff.png` | The 「查看完整变化」 HTML diff, opened in a browser. | desktop, cropped to the diff |
| `sessions.png` | The `/switch` card listing several sessions, or a `/topic` cover card. | phone, 2× scale |

Keep the set to the hero GIF plus three or four stills; a wall of screenshots
reads as clutter.

## Rules

- **Blur anything personal** — real names, chat titles, absolute paths, tokens.
  Record against a throwaway project, not a private repo.
- **One frame, one idea.** Crop to the card: black chat background and unrelated
  messages add noise, not context.
- **GIF over video.** GitHub cannot embed video in a README; a GIF is the only
  option that plays inline. Keep it under 8 MB, or it stalls the page.

A phone recording converts cleanly with ffmpeg (palette pass keeps the text
readable at small sizes):

```bash
ffmpeg -i raw.mp4 -vf "fps=12,scale=680:-1:flags=lanczos,split[a][b];[a]palettegen=max_colors=128[p];[b][p]paletteuse=dither=bayer" -loop 0 turn-card.gif
```

## crates.io caveat

crates.io renders the README with relative image paths broken — it does not have
this directory. If the images should show there too, use absolute
`https://raw.githubusercontent.com/SXongin/cola/main/docs/images/...` URLs in the
image tags instead of relative paths.
