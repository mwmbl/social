# mwmblsocial.org: copy

Draft copy for the site described in [`site-brief.md`](site-brief.md). Edit here first; the site
gets built from this. Notes for the editor are in `> [!NOTE]` blocks and are not part of the copy.
URLs marked `TODO` don't exist yet.

---

## Home

### Mwmbl Social

**A search engine for the fediverse that indexes only what people have opted into, and that any
server admin can run as a FASP (Fediverse Auxiliary Service Provider).**

Mwmbl Social indexes fully public posts and accounts whose authors have chosen to be found. When
someone changes their mind, deletes a post or makes it non-public, it comes out of the index.
It has no search box of its own: your fediverse server connects to it and shows the results.

[**Use Mwmbl's instance**](https://mwmbl.org/TODO-social-sign-up)
Mwmbl, a non-profit, open source search engine, runs an instance at social.mwmbl.org. Connect
your server to it and your users' opted-in posts become findable in Mwmbl search.

[**Run your own**](/self-hosting)
It's a single small service. If you can run a reverse proxy, you can run Mwmbl Social for your
own server.

### How consent works

Mwmbl Social indexes a post only if all of these are true:

- The post is fully public. Unlisted, quiet public, followers-only and direct posts are never
  indexed.
- The author has turned on **discoverable**, and for posts, **indexable**.
- The author hasn't asked for it to be removed.

When in doubt, it doesn't index. [Read the full rules](/consent), including how to check and
change your settings.

### Open source

Mwmbl Social is free software under the [AGPL-3.0](https://github.com/mwmbl/social/blob/main/LICENSE).
The source is at [github.com/mwmbl/social](https://github.com/mwmbl/social).

> [!NOTE]
> Status line? The README says "early: tested end to end against Mastodon 4.7.5, but not yet in
> a real deployment". Worth saying on the home page until social.mwmbl.org is live, e.g.
> "Mwmbl Social is new. It has been tested with Mastodon but is not yet widely deployed."

---

## What connecting gives a server

Mwmbl Social speaks FASP, the protocol Mastodon uses to connect to outside services. What your
server gets depends on what your server software supports today, and that is still limited.
This page says what works now.

### Account search and follow suggestions

Mastodon can ask Mwmbl Social for **account search** results and **follow recommendations**.
Your users can find accounts on other servers, and get suggestions for who to follow based on
their own name and bio and on what is trending.

This needs Mastodon 4.5 or later, with `EXPERIMENTAL_FEATURES=fasp` set. FASP support in
Mastodon is still experimental.

### Your users can be found outside your server

With **data sharing** turned on, your server tells Mwmbl Social about its public, opted-in posts
and accounts. Mwmbl Social fetches each one from your server and indexes it, so your users can
be found outside your own server. If you connect to Mwmbl's instance, that means in
[Mwmbl search](https://mwmbl.org).

Your server also tells Mwmbl Social when a post is deleted or an account changes its settings,
so those changes reach the index quickly.

### What it doesn't do yet

- **Post search on your server.** The FASP specifications don't define post search yet, so
  Mastodon can't ask Mwmbl Social to search posts. Your users' own search box won't change.
- **Trends.** Mwmbl Social works out trending posts and hashtags, but Mastodon doesn't ask for
  them yet.

### Lemmy

Lemmy doesn't speak FASP. Instead, Mwmbl Social can follow Lemmy communities, but only where the
instance's admins have agreed to be indexed. Lemmy posts can also reach it through Mwmbl's web
crawler, which respects robots.txt. See [how consent works](/consent#lemmy).

### Other server software

Any server that implements the FASP specifications may work. Only Mastodon has been tested.

> [!NOTE]
> Keep in sync with the README's "It currently provides" list and its Requirements section.

---

## How consent works

Search on the fediverse only works if people can trust it. These are the rules Mwmbl Social
follows. They are built into the software: an operator can't turn them off.

### What can be indexed

Only **fully public** posts. Mwmbl Social never indexes unlisted, quiet public, followers-only or
direct posts, and never stores them anywhere, not even in its logs.

### Your settings

Two settings on your account decide whether you can be found:

- **Discoverable** lets your account appear in search and suggestions. Without it, neither your
  account nor your posts are indexed.
- **Indexable** lets your public posts appear in search. Posts need both settings.

On Mastodon, both are under **Preferences > Public profile > Privacy and reach**:

- "Feature profile and posts in discovery algorithms" is discoverable.
- "Include public posts in search results" is indexable.

> [!NOTE]
> Check the exact Mastodon labels against a current Mastodon before publishing. Other software
> (Misskey, Pleroma and so on) names these differently, or may not have them; we could add a
> short table later.

### Changing your mind

Turn either setting off, delete a post, or change it from public, and it comes out of the index:

- **Deleting a post**, on a server connected to Mwmbl Social, removes it within moments.
- **Everything else** is caught by a regular re-check of everything in the index. Each item is
  checked again within about a week.

The re-check matters because your server doesn't announce every change. For example, if you make
a post non-public, your server simply stops talking about it.

> [!NOTE]
> The sweep period defaults to 6 days, and the spec requires weekly revalidation, so "within
> about a week" is safe. An account `update` (e.g. turning off discoverable) is also announced
> over FASP and handled at once, but I've kept the copy to deletes vs. everything else, as the
> brief does. Say more if you want.

### Following opts you in

Some fediverse software doesn't have the discoverable and indexable settings. If you're on one of
those, you can opt in by following the instance's server actor (for Mwmbl's instance,
`@index@social.mwmbl.org`). Following it means "index my public posts".

- Following only fills in settings your account doesn't have. If you have turned discoverable or
  indexable **off**, that still wins.
- Unfollowing withdraws your consent and removes what was indexed.

### Lemmy

Lemmy doesn't have these settings. For an author on a Lemmy server, a missing setting counts as
yes, but if the setting is there and set to no, it is respected. The rule follows the author's
own server, not the community they posted in, so a Mastodon user replying in a Lemmy thread still
needs discoverable and indexable turned on.

Mwmbl Social fetches Lemmy pages only where the server's robots.txt allows it, and respects its
crawl delay.

### Asking for removal

Every Mwmbl Social instance is run by an operator, who can remove content for good: a single
post, an account and its posts, or a whole domain. Removed content stays out, even if it is
announced again.

To ask for removal, contact the operator of the instance that indexed it. For Mwmbl's instance,
use [Mwmbl's contact page](https://mwmbl.org/TODO-contact). Its
[privacy policy](https://mwmbl.org/TODO-privacy) explains what Mwmbl keeps and why.

> [!NOTE]
> Is there a way for a user to find out which instances have indexed them? Not really: any
> server their server is connected to. Could say "ask your own server's admin which providers
> it is connected to" (Mastodon admins see this under Administration > Fediverse Auxiliary
> Service Providers).

---

## Self-hosting

Mwmbl Social is a single small service with no user interface. Run it for your own server, or
offer it to others. The [README](https://github.com/mwmbl/social#readme) has the full and
current instructions; this page is a summary.

### Requirements

- A public HTTPS URL for Mwmbl Social, served at the root of its host (for example
  `https://search.example.org`, not `https://example.org/search`).
- A reverse proxy in front of it, such as Caddy or nginx.
- Your fediverse server must be able to reach it, and it must be able to reach the fediverse.
- About 256 MiB of disk with the default index sizes. It needs very little CPU and memory.
- For Mastodon: 4.5 or later, with `EXPERIMENTAL_FEATURES=fasp` set.

> [!WARNING]
> **Choose your URL carefully.** The base URL can't change after you register your first server:
> it is part of every registration and of the service's fediverse identity.

### Install

With Docker:

```sh
docker run -d --name mwmbl-social -p 127.0.0.1:8080:8080 -v mwmbl-social:/data \
  -e MWMBL_SOCIAL_BASE_URL=https://search.example.org \
  ghcr.io/mwmbl/social:main
```

Or build from source with Rust:

```sh
cargo build --release -p mwmbl_social
MWMBL_SOCIAL_BASE_URL=https://search.example.org ./target/release/mwmbl-social serve
```

Then point your reverse proxy at `127.0.0.1:8080`.

### Connect your server

1. Register your server:

   ```sh
   docker exec mwmbl-social mwmbl-social register https://your.server
   ```

   This prints a key fingerprint and an approval link.
2. Open the link. In Mastodon it goes to **Administration > Fediverse Auxiliary Service
   Providers**. Check that the fingerprint matches the one printed, then approve.
3. On the same page, enable the capabilities you want: **data sharing** to send your public,
   opted-in content, and **account search** and **follow recommendations** to use them.

### Settings to set

Admins see these when they connect, so set them before you register anyone:

| Setting | What it's for |
|---|---|
| `name` | The name admins see in their list of providers |
| `contact_email` | How admins and users reach you |
| `privacy_policy_url` | Your privacy policy |
| `sign_in_url` | Where admins can manage their connection with you |
| `root_redirect` | Where visitors to your base URL are sent |

Every setting can go in a TOML config file or an environment variable, such as
`MWMBL_SOCIAL_CONTACT_EMAIL`. See the [README](https://github.com/mwmbl/social#configuration) for
the full list.

### Back up your keys

> [!WARNING]
> **Back up the `keys/` directory** inside the data directory. It holds the keys your registrations
> depend on. If you lose it, every server will have to register again.

> [!NOTE]
> Check the last sentence: is re-registering the actual recovery, given the base URL can't
> change and Mastodon refuses a second registration for the same URL until the old one is
> deleted? Maybe: "every connected server's admin will have to delete and re-approve you."

### Removing content

As an operator, you can remove content for good:

- `mwmbl-social forget <post or account URI>` removes one item.
- `mwmbl-social forget user@host` removes an account and its posts.
- `mwmbl-social forget example.com` blocks a whole domain.

Honour removal requests promptly. Your users and theirs are trusting you to.

---

## FAQ

### Is this Mastodon's search?

No. Mwmbl Social is a separate service that a Mastodon server (or other fediverse server) can
connect to. Each server's admin decides whether to connect, and to which instance.

### Does it store my private posts?

No, never, not even in its logs. Only fully public posts are indexed, and only if you have opted
in. See [how consent works](/consent).

### How do I stop my posts being indexed?

Turn off **indexable** (for your posts) or **discoverable** (for your account and posts) in your
account settings. On Mastodon, they're under Preferences > Public profile > Privacy and reach.
Your posts come out of the index within about a week. To have something removed for good, ask the
instance's operator. See [changing your mind](/consent#changing-your-mind).

### Who runs social.mwmbl.org?

[Mwmbl](https://mwmbl.org), a non-profit, open source search engine. Posts indexed there can
appear in Mwmbl search. They are never copied into Mwmbl's own index, so removing them from
Mwmbl Social removes them from Mwmbl too.

### How is this different from a crawler?

A crawler goes out and finds pages on its own. Mwmbl Social mostly doesn't: it is told about
posts by servers whose admins connected it, and by communities that agreed to be followed.

The one exception is Lemmy. A web crawler, such as Mwmbl's, can tell Mwmbl Social about Lemmy
posts it has found. Mwmbl Social fetches them only where the server's robots.txt allows it, and
indexes them under the same consent rules as everything else.

### Does it work with servers other than Mastodon?

Any server that implements the FASP specifications may work. Only Mastodon has been tested so
far. Lemmy communities can be followed instead; see
[what connecting gives a server](/connecting#lemmy).

### Can I search with it?

Not here. Mwmbl Social has no search box of its own: your fediverse server shows its results. To
search what Mwmbl's instance has indexed, use [Mwmbl](https://mwmbl.org).

> [!NOTE]
> Last question isn't in the brief, but "where's the search box?" seems the obvious first
> question for a user landing here. Drop it if you disagree. Also, the social search page on
> mwmbl.org doesn't exist yet; link to it once it does.

---

## Footer (every page)

Mwmbl Social is free software under the AGPL-3.0 · [Source on GitHub](https://github.com/mwmbl/social)
· A [Mwmbl](https://mwmbl.org) project · No analytics, no trackers.
