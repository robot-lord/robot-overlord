# Website (robot-overlord.app)

**Status:** in progress

A marketing site for rlord, in `site/` (Astro, static). `site/README.md`
covers running, deploying and regenerating images.

## Done

- Home (hero, install command, screenshots, features, the overlord,
  supported agents), How it works, Install, 404.
- Light and dark mode, Open Graph image, favicons. No tracking.
- README screenshots retaken on 0.22.0-dev.8 (`scripts/demo`), and the
  site's WebP copies made from them.

## Owed

- Deploy: import the repo in Vercel with root directory `site`, add the
  domain `robot-overlord.app`. Not done yet.
- CI does not build the site. Add a job (`npm ci && npm run build` in
  `site/`) once the site is live.
- Dependabot does not watch `site/package.json`. Add an `npm` entry to
  `.github/dependabot.yml`.
- The docs link goes to `docs/guide.md` on GitHub. Decide whether the guide
  should be rendered on the site.
- The site's copy is a hand-written digest of `README.md` and
  `docs/guide.md`. Nothing checks that it stays in step with them.
- Link the site from the README and the GitHub repository's "website"
  field once it is live.
