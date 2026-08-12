---
title: "Build the documentation site"
description: "Rebuild, validate, develop, and preview the Astro Starlight site using the repository Justfile."
sidebar:
  order: 4
---
The Starlight project lives under `docs/`. Generated assets are written to
`docs/dist/`; dependencies are pinned in `docs/package-lock.json`.

## Rebuild the complete documentation site

Run the repository-level recipe from the project root:

```sh
just docs
```

The full recipe installs dependencies from the npm lockfile, checks the Astro
configuration and content, and builds the static site.

The explicit rebuild alias runs the same complete workflow:

```sh
just docs-rebuild
```

## Individual workflows

```sh
just docs-install
just docs-check
just docs-build
just docs-dev
just docs-preview
```

If `just` is not installed, invoke the equivalent npm commands directly:

```sh
npm ci --prefix docs --no-audit --no-fund
npm run --prefix docs check
npm run --prefix docs build
```

The existing `docs/indexed-change-tracking.md` file remains in its original
location because the Rust implementation embeds it with `include_str!` to
extract the normative SQLite schema. The site keeps an
[embedded schema source](/reference/indexed-change-tracking/) note instead of
rendering that internal source as current integration guidance.
