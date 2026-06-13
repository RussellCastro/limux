# Fork Policy

This repository is a long-lived personal fork of `am-will/limux` focused on
cmux-style Linux workflows. The goal is a usable personal tool that can be
rebased onto upstream Limux periodically, not a strict upstream-first patch
queue.

## Branch Model

- `upstream/main`: canonical Limux source.
- `origin/main`: mirror of upstream unless there is a deliberate reason to
  move the fork default.
- `personal/cmux-parity`: long-lived personal branch for cmux parity work.

Keep personal changes on `personal/cmux-parity`. Rebase it onto upstream when
upstream has meaningful fixes or before starting a larger implementation pass.

## Rebase Workflow

```bash
git fetch upstream origin
git checkout personal/cmux-parity
git rebase upstream/main
./scripts/check-cmux-parity.sh
# run narrower Rust checks for touched crates; run ./scripts/check.sh before releases

git push --force-with-lease origin personal/cmux-parity
```

Use `--force-with-lease`, not plain `--force`, so the push refuses to overwrite
remote work you did not fetch.

## Upstream PR Policy

Do not open upstream PRs by default. Only PR changes that have traction or are
clearly useful to upstream Limux as small, self-contained improvements, such as
bug fixes, packaging fixes, isolated bridge improvements, or tests. Personal UX
policy, strict cmux parity tracking, and fork-specific roadmap docs can stay in
this fork.

## License Rule

cmux is GPL-3.0-or-later and Limux is MIT. Reimplement behavior from public docs
and observed behavior. Do not copy cmux implementation code into this fork unless
the license direction is intentionally changed.
