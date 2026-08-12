# List the available documentation workflows.
default:
    @just --list

# Install the exact documentation dependencies recorded in docs/package-lock.json.
docs-install:
    npm ci --prefix docs --no-audit --no-fund

# Check the documentation site's Astro configuration, content, and types.
docs-check:
    npm run --prefix docs check

# Build the static Starlight site into docs/dist/.
docs-build:
    npm run --prefix docs build

# Rebuild the complete documentation site from a clean, locked installation.
docs-rebuild: docs-install docs-check docs-build

# Rebuild the complete documentation site.
docs: docs-rebuild

# Run the local documentation development server.
docs-dev:
    npm run --prefix docs dev

# Preview the built static documentation site locally.
docs-preview:
    npm run --prefix docs preview
