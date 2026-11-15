# Saved Facebook pages

Logged-out copies of real public posts, saved with `server record <url>`. Each keeps only what
extraction reads (meta tags and the JSON blocks), minus comments and the `viewer` block, and
starts with a `<!-- url: ... -->` line naming the page. `tests/fixtures.rs` snapshots what
extraction makes of each.

- Add one: `just record <url> --out server/tests/fixtures/<page type>-<what>.html`, then
  `cargo insta test --review` (or `just snapshots`) to accept its snapshot.
- When Facebook changes a page type, re-record it and review the snapshot diff.
- Never add a page saved with `--cookies`: it holds the account's name and more.
- Check that only the posting page or person is named in it before committing.
