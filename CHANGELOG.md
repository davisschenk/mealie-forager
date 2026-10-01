# Changelog

## Unreleased

### Changed

- A job fails instead of importing a recipe with no ingredients or no
  instructions. Recipes Mealie creates that way (web scrapes, AI and zip
  imports) are deleted again; a web page falls back to the post pipeline.
- A post with several separate recipes fails with their names instead of
  importing only one. A note naming the one you want imports just that one.
  The Clean stage also refuses recipes that mix several dishes.

### Added

- Re-import recipes from their original links, one at a time or the whole
  library (`POST /api/reimport`, `GET`/`POST /api/reimport/library`, and the
  "Mealie library" panel). The new recipe replaces the old one, which is only
  deleted once the import succeeds. A job's **Re-import** button now replaces
  its recipe too, instead of leaving a duplicate.
- Docker image (`ghcr.io/davisschenk/mealie-forager`, amd64 and arm64), a
  `docker-compose.yml`, and a commented `.env.example`.
- `AUTH_PASSWORD`: an optional built-in login for the web UI and API.
- MIT license.

## 0.1.0

- First version: imports from social posts, recipe websites, photos, text,
  Mealie exports and videos; a Clean stage that links foods and units, tidies
  steps and adds categories; cleaning of recipes already in Mealie; an API
  token for iOS Shortcuts; and a NixOS module.
