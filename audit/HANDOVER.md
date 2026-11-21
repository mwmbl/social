# Audit fix handover (2026-11-21)

Where the audit fix work stands, for picking it up in a new session. The plan is
`audit/FIX_PLAN.md`; the findings are `audit/AUDIT.md`. Work goes one PR at a time, one
subagent per PR, in the table's order, with each PR on `main` unless its Base column names
another PR.

## Done so far

Plan rows 1–17 are all open as PRs, plus two PRs that aren't in the plan. Nothing is merged.

| Row | IDs | PR | Branch | Base |
|---|---|---|---|---|
| 1 | A1 | mwmbl/social#3 | `fix/a1-key-owner` | main |
| 2 | A2 | mwmbl/social#4 | `fix/a2-key-owner-lists-key` | #3 |
| 3 | A3, E4 | mwmbl/social#6 | `fix/a3-e4-delete-author` | #5 |
| 4 | A4 | mwmbl/social#7 | `fix/a4-block-remove-data` | main |
| 5 | D1 | mwmbl/social#5 | `fix/d1-queue-generation` | main |
| 6 | D2, D6 | mwmbl/social#8 | `fix/d2-d6-fetch-start-time` | #5 |
| 7 | D3 | mwmbl/social#9 | `fix/d3-author-refusal-origin` | main |
| 8 | D4 | mwmbl/social#11 | `fix/d4-undo-follow-removes-account` | main |
| 9 | D5 | mwmbl/social#12 | `fix/d5-failed-revalidation-limit` | #5 |
| 10 | D7, D8 | mwmbl/social#13 | `fix/d7-d8-tombstone-on-miss` | main |
| 11 | E9 | mwmbl/social#14 | `fix/e9-writer-watchdog` | main |
| 12 | B1, F7 | mwmbl/social#15 | `fix/b1-f7-no-proxy` | main |
| 13 | B2 | mwmbl/social#16 | `fix/b2-trailing-dot-host` | main |
| 14 | B5 | mwmbl/social#17 | `fix/b5-fasp-no-redirects` | main |
| 15 | C1, C2 | mwmbl/social#18 | `fix/c1-c2-announced-uris` | main |
| 16 | E1, E3 | mwmbl/social#19 | `fix/e1-e3-query-caps` | main |
| 17 | E2 | mwmbl/social#21 | `fix/e2-username-caps` | main |
| — | flaky test log capture | mwmbl/social#10 | `test/discovery-trace` | main |
| — | SQLite lock bug (the flake's cause) | mwmbl/social#20 | `fix/sqlite-immediate-tx` | main |

Row 3 was done after row 5 because its base is PR 5.

**Next:** row 18 (C3, E5), based on PR 5 (`fix/d1-queue-generation`). After that, row 19
(based on PR 13, `fix/b2-trailing-dot-host`), then 20 onwards in order.

## Read before merging

- **Merge mwmbl/social#20 first.** Every multi-statement store transaction was
  `BEGIN DEFERRED`. Under contention, SQLite returned "database is locked" at once instead
  of waiting out the 5 s busy timeout. That dropped an announced delete, and it is why
  `discovery::trending_announcements_feed_trends_and_follow_recommendations` fails now and
  then on CI. #18's latest CI failed only on that test, in both runs. Rebase #18 onto
  `main` after #20 merges and re-run CI. Other PRs that saw it pass on a re-run.
- **Migration numbers collide.** #5 adds migration 8. #8 and #12 both stack on #5 and both
  add a migration 9. Whichever merges second must renumber its migration to 10.
- **Index rebuild.** #21 changes account index keys, so it bumps the key scheme id from
  `social-v1` to `social-v2`. Existing indexes are refused on open and must be rebuilt.
  Nothing is deployed yet, so this should be fine, but check.
- **`audit/FIX_PLAN.md` conflicts.** Every PR ticks its own row in the table. The rows are
  next to each other, so expect small merge conflicts in that file when merging one PR after
  another. #18 also rewrites decision 3 and the text of rows 15, 18 and 20.
- **Stacked PRs.** Retarget each stacked PR to `main` when its base merges: #4 after #3; #6,
  #8 and #12 after #5.
- Each PR body has a "For a human to check" list. The points most worth a look:
  - **#18:** decision 3 changed. The FASP data sharing spec says servers "MUST share not only
    local but also remote content", so announced URIs on other hosts are kept, counted and
    logged rather than dropped. The amplification C1 was about is left to rows 18 and 20.
  - **#6:** a Delete of a URI we don't store now queues a fetch on the sender's origin, per
    decision 1.
  - **#7:** a `removeData` Block from anyone but a followed Group is dropped, including
    Lemmy instance bans, which are signed by the admin's own account.
  - **#17:** a server whose nodeinfo redirects (for example apex to `www`) can no longer
    register without being given its real URL.
  - **#19:** E3's test compares timings (it must be under 24x; the old code was about 76x),
    so it may flake on a noisy runner.
  - **#20:** `enqueue_all` still logs and drops a URI when the store errors. That now needs
    a lock held for over 5 s.

## Running the checks here

The container is short on memory. Use:

```sh
cargo fmt --all --check
CARGO_BUILD_JOBS=2 cargo clippy --workspace --all-targets -- -D warnings
CARGO_BUILD_JOBS=2 cargo test --workspace -j 2
```

Clippy and the full test run each take several minutes. Run them in the background or with
a long timeout.

## The subagent prompt

Each PR was made by one subagent given:
- CLAUDE.md, the "How each PR is made" section of `audit/FIX_PLAN.md`, the row, any decision
  it depends on, and only the finding's own section of `AUDIT.md`;
- the base branch, with the API a stacked base added;
- known migration numbers on open PRs;
- the check commands above, and the note about the flaky test;
- the commit and PR attribution lines. PRs are opened with `gh api .../pulls`, since only
  the REST API works here.

It reported back in under 150 words. After each one, the main session checked that the
working tree was clean, the PR's base was right, there was one non-WIP commit, and CI was
green.
