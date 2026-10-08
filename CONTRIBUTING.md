# Contributing

Thanks for helping. touchcue is in early development, so the codebase and the plan change quickly.

## Before you start

Open an issue or a [discussion](https://github.com/theaifam5/touchcue/discussions) before working on a non-trivial change, so the approach can be agreed first.

## Development setup

```sh
mise install
hk install --mise
```

`mise install` installs the pinned toolchain from `mise.toml`. `hk install --mise` installs the git hooks. Cargo runs through mr-boxington (mbx) via mise; set `MBX_DISABLE=1` to disable it.

## Checks before a PR

```sh
mise run lint
mise run test
mise run deny        # when dependencies changed
mise run xwin-check  # when platform code changed
mise run render      # when the CLI changed
mise run docs:build
```

`mise run fix` applies the automatic fixers.

## Generated files

`touchcue.usage.kdl`, `completions/`, `man/` and `docs/reference/cli/` are generated from the usage-rs definitions in `crates/touchcue/src/cli.rs`. Do not edit them by hand; run `mise run render`.

## Documentation

`mise run docs:dev` serves the documentation site locally. Every PR checks README, the docs site, AGENTS.md and CONTRIBUTING.md in full for stale content, not only text about the change.

The [docs workflow](.github/workflows/docs.yml) publishes the site on a push to `main` that changes `docs/`, `mise.toml` or `touchcue.usage.kdl`, on a push of a `v*` tag, and when run manually. Every run builds the docs of the highest stable `vX.Y.Z` tag at `/` and the docs of `main` under `/next/`, with a version switch in the nav and a notice on the `main` docs; prerelease tags are skipped, and without a release tag both are built from `main`. A tag whose docs predate the version switch is built with the VitePress config, theme and npm packages from `main`.

Runs started by a tag deploy only if the `github-pages` environment allows `v*` tags in its deployment branches and tags rule. Tags pushed with `GITHUB_TOKEN`, as release-plz does without `RELEASE_PLZ_TOKEN`, start no workflow; run the docs workflow manually after such a release.

The build reads three environment variables:

- `DOCS_CHANNEL`: `latest` or `next` (default `next`).
- `DOCS_BASE`: the site path the build is served under, starting and ending with `/` (default `/`).
- `DOCS_LATEST_VERSION`: the latest release tag, `vX.Y.Z`, shown in the version switch.

Without them, as in `mise run docs:dev` and `mise run docs:build`, the build is the `main` docs at `/`, and the version switch links work only on the published site.

## Commits and pull requests

Commit format, history rules and PR expectations are in [AGENTS.md](AGENTS.md); they apply to everyone, not only to agents. Pull request text is filled in from the PR template.

## AI-assisted contributions

- AI tools are allowed.
- You must have read, understood and tested every line you submit, and be able to explain it.
- Disclose AI use in the PR template field.
- Write issue and PR text yourself, not raw model output.
- Unsupervised autonomous-agent contributions are not accepted.

## Security

Do not report vulnerabilities in public issues. See [SECURITY.md](SECURITY.md).
