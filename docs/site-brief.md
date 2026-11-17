# mwmblsocial.org: site brief

What the project site at mwmblsocial.org needs to say. It is a static site on GitHub Pages. The
design and the decisions behind it are in [`PLAN.md`](../PLAN.md), "Deployment and adoption".

## Purpose and audience

mwmblsocial.org is the home of the **software**, in the way joinmastodon.org is Mastodon's
home. It is not the site of any one instance. Mwmbl's hosted instance, social.mwmbl.org, is one
option it links to, not the main thing.

Readers, in priority order:

1. **Fediverse server admins** deciding whether to connect a server, either to Mwmbl's instance
   or to one they run themselves.
2. **Fediverse users** who saw Mwmbl Social mentioned and want to know whether their posts are
   indexed and how to stop it.
3. **Developers and other FASP implementers.**

The tone is plain and factual. The fediverse has rejected search projects that indexed without
asking, so consent comes first and the site never overstates what the software does.

## Pages

### Home

- One sentence: a search engine for the fediverse that indexes only what people have opted
  into, and that any server admin can run as a FASP (Fediverse Auxiliary Service Provider).
- Two calls to action:
  - **Use Mwmbl's instance:** links to the sign-up page on mwmbl.org. Say that Mwmbl, a
    non-profit open source search engine, runs it, and that its opted-in posts appear in Mwmbl
    search.
  - **Run your own:** links to the self-hosting guide.
- A short "how consent works" summary that links to the full page.
- Licence (AGPL-3.0) and a link to the source on GitHub (github.com/mwmbl/social).

### What connecting gives a server

Be honest about the current state:

- Today Mastodon calls a FASP for **account search** and **follow recommendations** only, and
  only with `EXPERIMENTAL_FEATURES=fasp` set (Mastodon 4.5 or later). The FASP specs have no
  post search yet.
- **Data sharing** lets the server send us its public, opted-in posts and accounts, so its
  users can be found outside their own server, for example in Mwmbl search when connected to
  Mwmbl's instance.
- **Trends** are implemented, but Mastodon doesn't query them yet.
- Lemmy doesn't speak FASP. Mwmbl Social follows communities whose instance admins agree to be
  indexed instead.

### How consent works

Summarise the rules from `CLAUDE.md` ("Consent comes before anything else") for non-developers:

- Only fully public posts are indexed. Unlisted, quiet-public, followers-only and direct posts
  never are.
- An account must have **discoverable** turned on, and posts also need **indexable** (Mastodon:
  Preferences > Public profile > Privacy and reach). Explain where to find these settings.
- Turning these settings off, deleting a post or making it non-public removes it. Say how long
  that takes: deletions announced over FASP are near-immediate, and the revalidation sweep
  catches everything else within about a week.
- Lemmy: missing settings count as yes, but an explicit no is respected. robots.txt is
  respected.
- Following the instance's server actor opts in for settings an account doesn't carry, and
  unfollowing withdraws it. An explicit "no" always wins.
- How to ask an instance's operator to remove content (`forget`). For Mwmbl's instance, link to
  its contact and privacy pages on mwmbl.org.

### Self-hosting guide

Condensed from the repo's README, which stays the source of truth and should be linked to:

- Requirements: a public HTTPS URL served at the root of its host, a reverse proxy, about 256
  MiB of disk, very little CPU and memory.
- Install with Docker (`ghcr.io/mwmbl/social`) or from source.
- Connect a server: `register`, check the fingerprint, approve in Mastodon's admin panel,
  enable capabilities.
- Point out the settings an operator should set: `name`, `contact_email`,
  `privacy_policy_url`, `sign_in_url`, `root_redirect`.
- Warn that the base URL can't change after the first registration, and that the `keys/`
  directory must be backed up.

### FAQ

- Is this Mastodon's search? No: it's a separate service a server connects to.
- Does it store my private posts? No, never, not even in logs.
- Who runs social.mwmbl.org? Mwmbl. Link to mwmbl.org.
- How is this different from a crawler? Mostly it is told about posts by servers whose admins
  connected it, or by communities it follows that accepted. The one exception is Lemmy: a
  web crawler, such as Mwmbl's, can submit Lemmy post URIs, which are fetched only where
  robots.txt allows and indexed under the same consent rules.
- What about other servers than Mastodon? Any server implementing the FASP specs may work.
  Only Mastodon has been tested.

## Not on this site

- No sign-up form, accounts or search box. Sign-up for Mwmbl's instance and social search live
  on mwmbl.org, and a self-hosted instance has no UI.
- No analytics or third-party trackers.

## Facts to keep in sync

These change, so the site should state them once and link to the repo for detail:

- Supported FASP capabilities and their versions (README "It currently provides").
- Minimum Mastodon version, and whether FASP is still behind `EXPERIMENTAL_FEATURES`.
- The Docker image name and the configuration fields.
