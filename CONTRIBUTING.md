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
mise run deny      # when dependencies changed
mise run xwin-check  # when platform code changed
```

`mise run fix` applies the automatic fixers.

## Generated files

`touchcue.usage.kdl`, `completions/`, `man/` and `docs/reference/cli/` are generated from the usage-rs definitions in `crates/touchcue/src/cli.rs`. Do not edit them by hand; run `mise run render`.

## Documentation

`mise run docs:dev` serves the documentation site locally. Update the docs when behaviour changes.

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
