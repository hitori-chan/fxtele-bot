# Server

## A Telegram request

1. Telegram's fetcher asks for `https://fb.example.com/reel/123`.
2. The server checks it's Telegram: the user agent contains `TelegramBot` **and** the IP is in
   Telegram's ranges (`core.telegram.org/resources/cidr.txt`, refreshed daily, with a bundled
   copy as fallback). Anything else, a spoofed user agent included, is redirected to the original
   link and never causes a fetch.
3. The host picks the site: `INSTAGRAM_HOST` serves Instagram's links, any other host Facebook's.
   The path becomes a page type and a cache key ([Facebook](facebook.md), [Instagram](instagram.md)).
4. On a cache miss, extraction takes a slot from the site's budget (waiting up to 2 s) and must
   finish within the extraction timeout. Concurrent requests for one post share one fetch.
5. The answer is one page: preview tags in `<head>`, the Instant View article in `<body>`,
   compressed and marked `Cache-Control: no-store, no-transform`.

## Preview tags

- `og:title` names who posted: the author (`Name (@handle)` on Instagram), `Author ▸ Group` in a
  group (`Author ▸ Highlight` for a highlight), or the page's own name for profiles, albums and
  pages without a post. Never counts: Telegram keeps previews for a long time.
- `og:description` is the text, cut to the description length, line breaks kept. With Instant View
  on, Telegram drops plain newlines there, so they're written as `<br>`, as FxEmbed does.
- `og:site_name` is the site's name (a setting per site), plus which item the card shows when
  there are several: `Instagram · Photo 2 / 5`. The card shows the item `img_index` asks for
  (Instagram's carousel links), else the first video, else the first photo.
- Video: `og:type=video.other`, `og:video` (`video/mp4`) with its size; the poster in `og:image`.
  Over the largest-video size (20 MB, past which Telegram doesn't play videos in previews), the
  video uses Facebook's smaller file, as FxEmbed picks the best file under the limit. Sizes are
  estimated from the file's `bitrate=` and the duration.
- Photos: `og:image` is the first, with its size, or for several photos a mosaic (below). Text
  posts get the author's picture, which
  Telegram shows small (`twitter:card=summary`): the full-size one from their profile page (a
  setting), else Facebook's 40 px one.
- After a failure, Telegram gets what "When extraction fails" says: by default a card saying why
  (login wall, not public, slowed down, too slow) and that @WebpageBot refreshes it, as FxEmbed
  answers errors with a 200. Telegram keeps the first answer for a URL, so failures are cached for
  only 5 minutes, and rate limits, timeouts and network errors for 1, since the next try may work.
- @WebpageBot's refresh sends `PURGE` (FxEmbed honours it too); from Telegram's addresses, `PURGE`
  or `DELETE` on a preview path drops the cached post, so the refresh gets it fresh. Cloudflare's
  edge answers `PURGE` itself (400) and never passes it on, so behind Cloudflare a refresh gets the
  cached post until it expires (failures: 5 minutes); `DELETE` does get through.

## Instant View

Telegram applies its Medium template to pages that present themselves as Medium articles
(`al:android:app_name=Medium` plus `article:published_time`), as FxEmbed does. Telegram doesn't
document this, so it may stop working; it can be turned off per post type. The article is laid
out like FxEmbed's (`src/render/instantview.ts`):

1. a "View on Facebook" (or the post's other site) line and the title; Telegram writes the date under the
   title itself, from `article:published_time`, in the reader's time zone
2. every photo or video
3. the full text, with links, `#hashtags` (to the site's hashtag page) and, on Instagram, `@names`
4. the counts as of the fetch, `💬 12 🔁 79 👍 1.5K 👁️ 3.9K` (likes are ❤️ on Instagram), leaving out
   unknown and zero ones (a setting), then "View on Facebook"
5. after a divider, "About the author": their full-size picture, name, profile link (`@name` on
   Instagram and Threads) and bio, and "View on Facebook" once more; not on a profile's own
   page, which is already about them

Telegram shows each picture at its own size, no wider than the page and no taller than the page
is wide, so a tall photo sits narrower in the middle.

Links in the text go through `/_/l/{signature}?u=…`, which answers 403 to Telegram, so its
Instant View builder never visits other sites, where one failing would break the article (FxEmbed
does the same). Only links the server signed work. Facebook's own (hashtags) redirect; any other
site gets a page naming it, with a link on, since anyone can have a link signed by putting it
in a public post, and a plain redirect would lend the preview host to phishing.

The author's picture and bio come from their profile, fetched logged out, within the extraction
time, and reused for six hours per author: one more request to the site at most that often (a
setting per site; off for Instagram). Facebook's own 40 px picture is never stretched across the
article.

## Photo mosaics

A card has room for one picture, so for a post with several photos and no video (and no
`img_index` naming one) the card shows the first four as one, as FixupX does, and
`og:site_name` says how many there are: `Instagram · 5 photos`. `og:image` is
`/_/g/{id}.jpg`; the page gives its size, worked out from the photos' sizes.

- Layout (`server/src/mosaic.rs`): rows and columns (2 side by side or stacked; 3 or 4 in
  splits like 2×2, 1+3, 1+2) on black with 10 px gaps. Of the arrangements that show every
  photo at close to the same scale, the squarest wins, as in FxEmbed's mosaic. No photo is
  enlarged, the longest side is at most 2048 px, and each photo fills its tile, cut around the
  middle. JPEG at quality 85.
- Only requests from Telegram's addresses get one built, since it downloads the photos (through
  the site's proxy, at most two builds at a time); everyone else, and a build that fails or takes
  over 6 s, is sent to the first photo. Built ones are kept in memory for an hour (16 MB).
- Instant View still shows every photo. A switch on the Previews page turns mosaics off.

## Media links

Videos always point at the server, `/_/v/{id}/{n}.mp4`, which redirects Telegram to the 1080p
file or the site's. Telegram doesn't play a card's video linked straight to a CDN, only through a
redirect on the preview host (Instant View plays either); FxEmbed works around the same.

With the `/_/m/` setting, photo and poster URLs point at the server too, which redirects to a
fresh CDN URL, refetching the post if the old one expired (for Telegram only). Only media of
posts the server extracted itself are served, and only on Meta's CDNs (`fbcdn.net`,
`cdninstagram.com`): anything else is dropped as the post is extracted, so neither a redirect nor
a 1080p download can be pointed elsewhere, and downloads follow a redirect only to another file
there. A failure still cached stands for media links too: no refetch until it's out.

## 1080p videos

Off by default, and needs a bucket with S3's API (Cloudflare R2, Garage, S3...). Facebook's
720p file is the best one with sound; above it, video and audio are separate DASH files, and only
in AV1 or VP9, which older phones show as a black picture. With "Videos: up to 1080p" on:

- `/_/v/{id}/{n}.mp4` gets the joined file when it will fit under the size limit (20 MB,
  Telegram's limit for videos in previews), and `og:video` gets its size; only for the videos the
  page shows: the first, or all of them in Instant View
- the server downloads both files right away, joins them into one MP4 without re-encoding
  (`server/src/video/mp4.rs`), at most two at a time, and uploads it to the bucket. One that
  waits over 20 s for its turn gives up, since Telegram has taken the 720p file by then
- `/_/v/` redirects Telegram to a link signed just then, for an hour. The server never sends
  video, and the bucket stays private, so its files can't be listed or linked to for long
- only requests from Telegram's addresses get the bucket's file or start a download; everyone
  else, and anything that goes wrong, gets the site's 720p file. Media IDs are made with a random
  key kept in `server.db`, so they can't be worked out from a post's link
- failures are remembered for 10 minutes, so a broken video isn't downloaded over and over
- files are kept for the days set on the Videos page (3 by default, 30 at most), and the oldest
  go first while over the space limit. A cleanup runs after each new file and every hour, 1080p
  on or off, from the list of the bucket the server keeps (below), so it adds no listing
- there's no polling: the bucket is listed once, when first needed, and each upload shows whether
  it works. After a failure Telegram gets 720p for 10 minutes; then the next look at the bucket
  (a video, a preview, the dashboard, the hourly cleanup) lists it again
- files are named `<source>/<id>-<n>.mp4`, e.g. `facebook/0123456789abcdef-0.mp4`. Only those
  prefixes are listed, and the server only ever deletes files of exactly that shape: the flat
  `<id>-<n>.mp4` files of earlier versions are left alone, for you to delete. A new extractor adds
  a `Source` variant (`server/src/extract/mod.rs`), which gives its files their own prefix
- delete the files from the dashboard: one removed elsewhere still counts as there until the
  server restarts

An R2 lifecycle rule on the prefixes (e.g. delete `facebook/` after 7 days, longer than the
setting) is a good backstop for files the server can't clean up, such as when it's gone.

Cloudflare allows video served from R2. Its free plan doesn't allow video through its proxy, so
a self-hosted bucket such as Garage needs an S3 API hostname that Cloudflare doesn't proxy.

## Requests to the sites

- Facebook: only `facebook.com` hosts; redirects are followed by hand, at most 10, and must stay
  on Facebook. Instagram: no redirects are followed, except a share link's one, to a post.
- One warm HTTP/2 connection per site, direct or through that site's proxy setting, which 1080p
  downloads of its videos use too.
- Each site has its own cookies, policy and budget. Policy `fallback` (the default) sends cookies
  only after a login wall, keeping the account out of most fetches. What never shows logged out
  (Facebook's stories, Instagram's live stories and highlights) goes logged in right away.
  Profiles and other pages never get cookies, which would show the account's own view.
- Cookies live in `server.db` and are never logged. Cookies without the site's session (`c_user`
  and `xs` on Facebook, `ds_user_id` and `sessionid` on Instagram and Threads, `reddit_session` on
  Reddit) are refused.

## Limits and cache

| | Default | |
|---|---|---|
| Requests per IP | 120/min | Public listener; Telegram exempt; over it: `429` |
| Fetches | Facebook 60/min, 4 at once; Instagram 20/min, 2 at once | Cache misses from Telegram, the playground and test links |
| Cache | 64 MB | Posts 60 min (less if a media URL expires sooner), failures 5 min (rate limits and timeouts 1 min) |

When the cache is full, the least recently used posts go first, so a post just extracted is
always there for the media requests that follow its preview.

## Statistics

Every request is counted by outcome (redirect, hit, extracted, failed at a stage, a cached failure
answered again, busy, rate limited), site and page type, and so is every fetch from a site, whoever
it was for (Telegram, test links, the playground, expired media links): those give fetch times and
each site's busiest minute against its budget. All of it is kept with timings, and saved each
minute: per-minute for 7 days, hourly for 90 days.
Timings are kept as totals, the slowest, and counts in bands (under 100 ms, 250 ms, 500 ms, 1, 2, 4
and 8 s, and the rest), from which the dashboard estimates the median and 95th percentile. The
last 500 failures are kept with their URL and error, and the last 500 requests (with the post's
link) in memory, for the dashboard's request log. The media fetched after the previews (photos
and thumbnails, the sites' videos, 1080p videos from the bucket and those that fell back to 720p,
mosaics) is counted apart, by site, kept the same way. Photos linked straight to the sites' CDN
never reach the server, so they count when the preview that links them is served.

## Test links and alerts

Test links are checked 2 minutes after start, then every 30 minutes, within each site's budget.
Each check extracts the post and fetches the start of its first media file (and, with 1080p on,
of both streams), as Telegram would. A day of checks (48) is kept per link. A link added on the
dashboard is checked at once, and one can be checked on its own; cookie and budget alerts wait for
a check of all of them.

Alerts go to the alert chat, sent with `TELEGRAM_BOT_TOKEN`, when:

- a test link starts failing, or works again
- a site's last 5 extractions for Telegram all failed, and again when one works
- 1080p videos couldn't be made (at most hourly)
- the video bucket fails, and again when it works
- Telegram's requests arrive from the front end's address: `CLIENT_IP_FROM` is wrong (daily)
- a site rejects its cookies, or they expire within 7 days
- a site's budget ran out more than 10 times in half an hour

Every alert is also noted on the dashboard's Activity page, sent or not.
