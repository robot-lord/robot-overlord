# Robot Overlord website

The marketing site for [robot-overlord.app](https://robot-overlord.app). It
is a static site built with [Astro](https://astro.build): `astro build`
writes plain HTML and CSS to `dist/`. There is no server code, no tracking
and no analytics.

This folder is a self-contained project. Nothing in the Rust workspace
depends on it.

## Run it locally

Needs Node 22.23 or newer.

```sh
cd site
npm install
npm run dev        # http://localhost:4321, reloads on save
```

To check the production build:

```sh
npm run build      # writes dist/
npm run preview    # serves dist/
```

## Deploy on Vercel

1. Import the `ayersb/robot-overlord` repository in Vercel.
2. Set **Root Directory** to `site`.
3. Leave **Framework Preset** on **Astro**. Vercel detects it and uses
   `npm run build` with `dist` as the output folder. No `vercel.json` is
   needed.
4. Add the domain `robot-overlord.app` under Settings → Domains and point
   its DNS at Vercel.

`astro.config.mjs` sets `site` to `https://robot-overlord.app`. Canonical
links and the Open Graph image URL are built from it, so change it there if
the domain changes.

## Layout

| Path | What it holds |
|---|---|
| `src/pages/` | One file per page: home, `how-it-works`, `install`, `404` |
| `src/layouts/Base.astro` | The page shell: meta tags, header, footer, the theme toggle and copy buttons |
| `src/components/` | `Command` (a command with a copy button) and `Shot` (a screenshot) |
| `src/styles/global.css` | All styles. The palette is at the top, with the light and dark values. |
| `src/site.ts` | Links and commands used on more than one page |
| `public/` | Images, favicons, `og.jpg`, `robots.txt` |

## Content

Every claim on the site comes from the repository's `README.md` and
`docs/guide.md`. When those change, update the pages to match.

## Images

The screenshots are WebP copies of `docs/images/*.png`, in two widths. After
retaking the screenshots (`scripts/demo/screenshots.sh`), regenerate them
from the repository root:

```sh
for n in overlord waiting list; do
  cwebp -lossless -z 9 docs/images/$n.png -o site/public/img/$n-1600.webp
  cwebp -q 82 -resize 800 0 docs/images/$n.png -o site/public/img/$n-800.webp
done
```

The banner (`logo-*.webp`), the header mark, the favicons and `og.jpg` are
made from `docs/images/logo.jpg`.

## Dependencies

`astro` and `@fontsource-variable/archivo` (the heading font, served from
the site itself) are pinned to exact versions in `package.json`.
