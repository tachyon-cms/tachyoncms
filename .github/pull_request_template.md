## What does this change?

<!-- What a user or contributor can now do, or what was broken and is now fixed. Link the issue: "Closes #123". -->

## How did you test it?

<!-- The tests you added, and anything you checked by hand. -->

## Checklist

- [ ] Tests cover the change, and I watched the new ones fail before the fix.
- [ ] `cargo fmt --all`, `cargo clippy --workspace --all-targets -- -D warnings` and `cargo test --workspace` pass locally.
- [ ] If I changed SQL, I ran `cargo sqlx prepare --workspace` and committed `.sqlx/`.
- [ ] The PR title follows [Conventional Commits](https://www.conventionalcommits.org/) (it becomes the commit message).
