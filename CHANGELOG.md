# Changelog

## Unreleased

### Added

- Docker image (`ghcr.io/davisschenk/mealie-forager`, amd64 and arm64), a
  `docker-compose.yml`, and a commented `.env.example`.
- `AUTH_PASSWORD`: an optional built-in login for the web UI and API.
- MIT license.
- The Clean stage fills in the recipe's tools (air fryer, sheet pan, …), reusing
  existing Mealie tools and creating missing ones.
- Imported recipes are tagged with where they came from (`TikTok`, `Instagram`,
  `Website`, `Upload`, …). `SOURCE_TAGS=false` turns this off.

## 0.1.0

- First version: imports from social posts, recipe websites, photos, text,
  Mealie exports and videos; a Clean stage that links foods and units, tidies
  steps and adds categories; cleaning of recipes already in Mealie; an API
  token for iOS Shortcuts; and a NixOS module.
