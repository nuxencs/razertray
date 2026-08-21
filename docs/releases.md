# Releases

## Ownership

`release-on-tag.yml` is the only workflow that builds assets and creates or
updates a GitHub release. It validates that the tag matches the Cargo package
version, runs tests, builds the MSVC binary, and uploads the executable and
SHA-256 checksum.

`release-device-map-merge.yml` creates a version tag after the automated
device-map pull request merges, then calls the same release workflow directly.
The direct call is required because tags pushed with `GITHUB_TOKEN` do not start
another workflow run.

## Generated notes

GitHub releases use `.github/release.yml`.

- `Features`: `feat`, `enhancement`
- `Fixes`: `fix`, `bug`
- `Docs`: `docs`, `documentation`
- `Maintenance`: `chore`, `ci`, `refactor`, `build`, `test`

Pull requests labeled `skip-changelog` are excluded.

The `label-release-notes` workflow maps Conventional Commit title prefixes to
the matching labels. A release-preparation title such as `chore: release
v1.2.3` also receives `skip-changelog`.
