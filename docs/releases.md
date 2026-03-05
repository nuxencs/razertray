# Releases

## Generated release notes

GitHub releases use generated notes via `.github/release.yml`.

Categories:

- `Features`: `feat`, `enhancement`
- `Fixes`: `fix`, `bug`
- `Docs`: `docs`, `documentation`
- `Maintenance`: `chore`, `ci`, `refactor`, `build`, `test`

PRs labeled `skip-changelog` are excluded from release notes.

## PR title -> label mapping

The `label-release-notes` workflow auto-labels PRs from conventional prefixes:

- `feat:` -> `feat`
- `fix:` -> `fix`
- `docs:` -> `docs`
- `chore:` -> `chore`
- `ci:` -> `ci`
- `refactor:` -> `refactor`
- `build:` -> `build`
- `test:` -> `test`

Release-prep PRs matching `chore: release vX.Y.Z` also get `skip-changelog`.

## Release workflows

- Tag releases use `gh release create --generate-notes`
- Device-map merge releases use `gh release create --generate-notes`
- When a previous tag exists, workflows pass `--notes-start-tag <previous-tag>`
