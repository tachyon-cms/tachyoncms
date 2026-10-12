# Contributing to TachyonCMS

Thanks for taking a look. Whether it's a typo fix or a new field type, this page gets you from clone to merged pull request.

## Before you start

- **Small fixes** (typos, docs, an obvious bug): open a pull request straight away.
- **Anything bigger** (a feature, a new dependency, a change to the API's shape): open an issue or a discussion first, so nobody spends a weekend on something that clashes with the roadmap.
- **Security issues**: don't open a public issue. Follow [SECURITY.md](SECURITY.md).

## Setting up

You'll need Rust (stable), Docker, and `sqlx-cli`:

```bash
cargo install sqlx-cli --locked
cp .env.example .env
docker compose up -d
set -a; source .env; set +a
sqlx migrate run --source crates/tachyon-db/migrations
cargo test --workspace
```

All tests should pass before you change anything. If they don't, that's a bug, so please tell us.

## Making a change

1. Fork the repo and create a branch from `main` (`fix/empty-slug`, `feat/number-step`).
2. Write a failing test first, then the change that makes it pass. Database tests use `#[sqlx::test]`, which gives each test its own fresh database, so don't mock Postgres.
3. If you add or change a SQL query, run `cargo sqlx prepare --workspace` and commit the updated `.sqlx/` folder. CI builds without a database and needs it.
4. Run the same checks CI runs:

   ```bash
   cargo fmt --all
   cargo clippy --workspace --all-targets -- -D warnings
   cargo test --workspace
   ```

5. Open a pull request and fill in the template.

## Commit messages

We use [Conventional Commits](https://www.conventionalcommits.org/): `feat:`, `fix:`, `docs:`, `refactor:`, `test:`, `chore:`, with an optional scope such as `feat(api):`. Keep the subject under 72 characters and say what changed for the user, not which file you touched. Pull requests are squash-merged, so the PR title becomes the commit on `main`. Please give it the same care.

## How review works

- Nothing lands on `main` without a pull request and green CI. That applies to maintainers too.
- We aim to give a first response within a few days. If a week passes with no reply, a polite nudge is welcome.
- Reviews are about the code, never the person. The [Code of Conduct](CODE_OF_CONDUCT.md) applies everywhere in the project.

## Good first issues

Look for the [`good first issue`](https://github.com/tachyon-cms/tachyoncms/labels/good%20first%20issue) label. If one looks interesting, comment that you're taking it so nobody doubles up.

## License

By contributing, you agree that your contributions are licensed under the [MIT License](LICENSE).
