# Reddit extraction

Reddit documents its API for registered apps, not the JSON its own site reads, which is what this
uses. Checked on 2026-11-17, from a home address.

## What answers what

| Source | Logged out | Logged in (`reddit_session`) |
|---|---|---|
| `www.reddit.com/{path}.json` | 403 "blocked by network security", whatever the user agent | the data, through the server's HTTP client (curl with the same cookie is refused) |
| HTML pages | a JavaScript challenge | the page |
| The page for link-preview crawlers (`facebookexternalhit/1.1`) | headline, counts and a picture of the post | – |
| `embed.reddit.com/{permalink}?embed=true` | the post's player, with files that have sound | – |
| Redirects (`/r/{sub}/s/{code}`, `v.redd.it/{id}`) | answered to a program's user agent | – |

So Reddit's cookie policy is `always` by default: the session gives the full post, and without
cookies (or when they're refused) the crawlers' card is the preview. Under `fallback` the
crawlers' card is tried first, and as it nearly always works, the session would almost never be
used. Only the `reddit_session` cookie is sent, with the client's one browser user agent.

The data's limit is 100 requests in 10 minutes per account (`x-ratelimit-*` headers, the window
ending on the clock's tens), so the budget is 6 fetches a minute, 2 at once. Pages are 600 per
window; logged out, about 200 per window per address.

## Links

Links reach the server on its own host (`REDDIT_HOST`), path and query unchanged. The bot swaps
whole hosts, so `redd.it`, `v.redd.it` and `i.redd.it` links arrive as a bare `/{something}`,
told apart by shape.

| Link | Cache key | How |
|---|---|---|
| `/r/{sub}/comments/{id}/...`, `/comments/{id}` (or `{id}.json`), `/user/{name}/comments/{id}`, `/gallery/{id}`, `redd.it/{id}` | `/comments/{id}/` | `/comments/{id}.json` |
| the same with a comment's ID after the slug, or `/comment/{cid}` | `/comments/{id}/comment/{cid}/` | `/comments/{id}/comment/{cid}.json` |
| `/r/{sub}/s/{code}`, `/u/{name}/s/{code}` | the link | redirects (up to three), logged out, to the post |
| `v.redd.it/{id}` (10 characters or more) | the link | two redirects, by way of `/video/{id}` |
| `i.redd.it/{name}.{jpg,png,gif,webp}`, `preview.redd.it/{title}-v0-{name}.{ext}` (signed, so its `i.redd.it` copy), `/media?url=` around either | the picture | nothing is fetched: a card of the picture |
| `/u/{name}`, `/user/{name}` | `/user/{name}/` | `/user/{name}/about.json` |
| `/r/{name}` | `/r/{name}/` | `/r/{name}/about.json` |
| anything else | the path | the crawlers' card |

IDs and names are lowercased; no query is kept (`share_id`, `utm_*`). JSON is always asked for
with `raw_json=1`, without which Reddit writes `&` as `&amp;` in every URL and text.

## A post

`/comments/{id}.json?raw_json=1&limit=1&depth=1` answers two listings: the post is
`[0].data.children[0].data` (kind `t3`), comments `[1]`. `limit` and `depth` keep the comments
short, though the IDs of the rest still come (200 KB for a big thread).

- **Title:** `title`, after `NSFW · ` or `Spoiler · ` when `over_18` or `spoiler`.
- **Byline:** `u/{author}` (none for `[deleted]`) and `subreddit_name_prefixed`; they go in the
  card's site line and under the title in Instant View.
- **Text:** `selftext`, Markdown, rendered into the few tags Instant View knows (paragraphs,
  headings, quotes, lists, code, bold, italic, strikethrough, links) and as plain text for the
  card. A spoiler (`>!…!<`) shows as `[spoiler]`. A link post adds its `url_overridden_by_dest`, unless it's Reddit's own.
- **Counts:** `score` (⬆️), `num_comments`, `num_crossposts`.
- **Date:** `created_utc` (a float).
- **Media**, first that applies:
  - a hosted video: `secure_media.reddit_video` (or `media.`): `fallback_url`, its `width`,
    `height`, `duration` and `has_audio`, and the poster from `preview.images[0].source`; else
    the clip Reddit made of a linked GIF, `preview.reddit_video_preview`, without sound
  - a gallery: `gallery_data.items[]` in order, each `media_metadata[media_id]` (`status` valid):
    an `Image`'s `s.u` (full size on `preview.redd.it`) with `s.x` × `s.y`, an `AnimatedImage`'s
    `s.mp4`
  - a GIF: `preview.images[0].variants.mp4.source`
  - else the picture Reddit made for it, `preview.images[0].source`: the photo itself, or a
    link's preview
  - a text post (`is_self`) takes nothing from `preview`: it's of a page its text links to
- **Crossposts** have no media of their own: `crosspost_parent_list[0]` is the whole shared post,
  shown after `↘️ Shared from u/{author} in r/{sub}`, with its headline when it differs.

## Sound

`fallback_url` has no sound. The post's embed page has files that do: its `<shreddit-player>`'s
`packaged-media-json` attribute holds `playbackMp4s.permutations[].source{url, dimensions,
bitrateBps}` on `packaged-media.redd.it`, signed for a few hours (`e=`, Unix time). The best
becomes the video, and the next best that fits the largest-video size its smaller file.

Without them, the DASH playlist (`dash_url`, used only on `https://v.redd.it/`) gives the streams, `CMAF_{height}.mp4` and
`CMAF_AUDIO_128.mp4` (relative to `https://v.redd.it/{id}/`; older videos may name them
`DASH_*`), fragmented MP4 like Facebook's, which the server joins when 1080p videos are on.
Otherwise the video stays silent, and that's logged.

## A comment

`[1].data.children[0].data` is the linked comment (kind `t1`, its `id` checked). The preview is
the comment (`body`, `author`, `score` unless `score_hidden`, `created_utc`, `permalink`), titled
`Re: {post's title}`, with the post after `↘️ Replying to u/{author} in r/{sub}`, and its media.

## Users and communities

- `/user/{name}/about.json` (kind `t2`), only for names of Reddit's letters (the name comes from
  the post's data and goes into a request with the session): `name`, `snoovatar_img` or `icon_img`,
  `subreddit.public_description`, `total_karma`.
- `/r/{name}/about.json` (kind `t5`): `display_name_prefixed`, `title` (when it isn't just the
  name), `public_description`, `subscribers`, `community_icon` or `icon_img`.

## Without cookies

The page for `facebookexternalhit/1.1` (after the redirect a link without its slug gets):

- `og:title` `From the {sub} community on Reddit: {title}`; a text post's title is only in
  `og:image:alt`.
- `description` `216K votes, 23K comments. {start of the text}`; a community's description
  ("17M subscribers in the … community.") isn't the post's text.
- `og:image` `share.redd.it/preview/post/{id}`, a picture Reddit draws of the post.
- For users and communities, `og:title` is the name and `og:description` theirs; the picture is
  Reddit's logo, left out.

## Errors

| Answer | Stage |
|---|---|
| 403 or 404 with JSON `reason`: `quarantined`, `private` | login wall (the account hasn't opted in) |
| the same with `banned` or anything else | not found |
| 403 HTML (the network-security page), or HTML where JSON was expected | login wall |
| a redirect to `/login`, or 401 | login wall; the cookies are marked rejected |
| 404 HTML | not found |
| 429 | rate limited |

Logged-in answers carry a `modhash` and nothing that names the account, but fixtures are recorded
logged out only, as the crawlers' tags.
