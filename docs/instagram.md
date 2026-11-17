# Instagram extraction

Instagram documents none of this. What follows was checked on 2026-11-16; the query IDs change now
and then, which shows up as test links failing at `fetch`. To find new ones, open a post logged
out in a browser and look for the `graphql/query` request (`doc_id`, `X-FB-Friendly-Name`).
[OGInstagram](https://github.com/seirenkr/OGInstagram) uses the same method and is worth
checking.

## Links

Links reach the server on its own host (`INSTAGRAM_HOST`), path and query unchanged.

| Link | Page type | Cache key |
|---|---|---|
| `/p/{code}`, `/reel/{code}`, `/reels/{code}`, `/tv/{code}`, with or without `/{user}` first | post or reel | `/p/{code}` |
| `/stories/{user}/{id}` | story | `/stories/{user}/{id}` |
| `/stories/{user}` | story | `/stories/{user}` |
| `/stories/highlights/{id}`, `/s/{base64 of "highlight:{id}"}` | story | `/stories/highlights/{id}` |
| `/share/...` | post or reel | followed once, to the post, with a plain `Mozilla/5.0` user agent: a browser's gets a page that redirects by script |
| `/{user}`, `/_u/{user}` | profile | `/{user}` |
| anything else | other | the path |

Usernames are lowercased (Instagram ignores case) and no query is kept: `igsh` tracks, and
`img_index` (from 1) only picks which carousel item the card shows, at render time.

A shortcode is the media ID in base 64 (`A`–`Z`, `a`–`z`, `0`–`9`, `-`, `_`). The ID's top bits
are milliseconds since 2011-08-24 21:07:01.721 UTC (`(id >> 23) + 1314220021721`), which dates a
post when nothing else does. Private accounts' links have longer codes that don't convert.

## Posts, reels and stories

1. **The post query, logged out.** `POST https://i.instagram.com/graphql/query`, form
   `variables={"shortcode":…,"__relay_internal__pv__PolarisAIGMMediaWebLabelEnabledrelayprovider":false}`,
   `doc_id=27128499623469141` and `lsd=<random>`, with headers `X-FB-Friendly-Name:
   PolarisPostRootQuery` and `X-FB-LSD` repeating the token. No page visit is needed first.
   The answer is `data.xdt_api__v1__media__shortcode__web_info.items[0]`.
2. **The embed page**, `/p/{code}/embed/captioned/` with the user agent `facebookexternalhit/1.1`,
   starts after 800 ms, or at once if the query fails; the first success wins. Its
   `"contextJSON"` string holds `gql_data.shortcode_media`, the older shape: no full name, no date.
3. **The logged-in API**, `GET /api/v1/media/{id}/info/` on www.instagram.com, with the cookies,
   `X-CSRFToken` (the `csrftoken` cookie) and `X-IG-App-ID: 936619743392459`. Same item shape as
   the query, plus play counts, and DASH up to 1080p (VP9; logged out stops at 720p H.264).
4. **Crawler tags.** `/p/{code}/` for `facebookexternalhit/1.1` keeps its tags even when the rest
   is gated: `og:image` (the cover), `og:title` `Name on Instagram: "caption"`, `og:description`
   `30K likes, 129 comments - user on April 17, 2023: "caption"`, `og:url`
   `/{user}/p/{code}/`, `twitter:title` `Name (@user) • Instagram photos and videos`. The result
   is a one-picture card with rounded counts.

Under the `fallback` cookie policy, the logged-in API is tried after a login wall or "not shown",
and for a day after that worked, a post goes to it first: the query would only say "not shown"
again. A rate limit isn't a reason to use the cookies, as it would put all the traffic on the
account. The crawler tags come last, after a login wall, "not shown" or a rate limit: the
crawlers' page still answers an address whose queries are rate-limited, while the embed page
holds no data for many posts (`"contextJSON":null`, single photos among them). A post that isn't
shown logged out gets the crawlers' error page (`PolarisErrorRoute`, as a code that never was
does), so while the queries are rate-limited it fails as rate limited even with cookies saved:
seen in November 2026 on a carousel the logged-in API still had.

A story with an ID goes through the post query too: its ID is a media ID. The answer is checked to
be a story (`product_type: story`) by the account the link names.

An account's live stories (`/stories/{user}`) and highlights can't be read logged out (the query
answers `1675002 Unauthorized logged out query`), so with cookies they're read first, from the
page itself: logged in, `/stories/{user}/` preloads `xdt_api__v1__feed__reels_media` and
`/stories/highlights/{id}/` preloads `xdt_api__v1__feed__reels_media__connection`, in a
`data-sjs` script. A page may preload both; the reel used is the one the link names (the
account or the highlight), and another's is refused. Without cookies, or when that fails, an account's stories show the account
(the profile query below), and a highlight its crawler tags: the cover, and `og:title`
`{title} = @{user}`.

## Reading an item

- **Author:** `user.username`, `user.full_name`, `user.profile_pic_url` (150 px, for cards), and
  `user.hd_profile_pic_url_info.url`, the picture as uploaded, for "About the author" in
  Instant View. The profile query has only the 150 px one. The card's title is `Name (@user)`.
- **Text:** `caption.text`.
- **Media:** `carousel_media[]`, else the item itself. Photos: the largest of
  `image_versions2.candidates`, sized by `original_width`/`original_height`. Videos
  (`media_type` 2): the largest `video_versions` entry, with the picture as poster, and
  `video_dash_manifest` for 1080p. A carousel item without a picture fails the post rather than
  shift every later `img_index`; so does one in the embed page's `edge_sidecar_to_children`.
- **Counts:** `like_count`, `comment_count`, `play_count`/`ig_play_count`/`view_count` (the first
  above 0), `media_repost_count`; likes and views are left out when
  `like_and_view_counts_disabled`.
- **Type:** `product_type` `clips` is a reel, `story` a story, anything else a post.
- **Date:** `taken_at`.

## Profiles

`POST https://www.instagram.com/api/graphql/`, `doc_id=27981003384861049`, friendly name
`PolarisLoggedOutDesktopWWWProfileRootContentQuery`, `variables={"username":…}`, logged out.
`data.xig_user_by_username` has the name, bio, picture, `is_private` and `latest_reel_media`
(when the newest story went up). The same query gives posts' authors their bio, once per six
hours per author, when "About the author" is on for Instagram (off by default: it costs requests).

## Errors

| Answer | Stage |
|---|---|
| GraphQL `1675004`, or "Please wait" | rate limited |
| GraphQL `1675030` (`field_exception`) | not found: private, age-restricted, limited to some countries, or gone; it can't tell which |
| HTTP 429 | rate limited |
| a redirect to `/accounts/login`, `/challenge` or `/checkpoint`, or a page where JSON was expected | login wall; with cookies, they're marked rejected |
| an empty `items` | not found |

Rate limits, timeouts and network errors are cached for a minute, not the failure time.

## Being careful with the account

A web session that suddenly shows up as another device gets logged out: one did, right after it
was used with the iPhone app's user agent against `i.instagram.com`'s REST API. So cookies only go
to www.instagram.com, always with the client's one browser user agent, and only for posts and
stories; profiles and other pages are always fetched logged out.

Instagram blocks an address after a few hundred requests an hour, so its budget is 20 fetches a
minute, 2 at once. A proxy with several exit addresses helps (OGInstagram rotates 16 IPv6
addresses from its server's /64).

## Fixtures

`server record <instagram link>` saves the answers as JSON (`query`, `embed`, `crawler`), keeping
only the fields extraction reads: `usertags`, music credits and likers name other people, and
`*_token` fields track. Only logged-out answers are recorded.
