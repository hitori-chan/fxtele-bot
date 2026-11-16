# Server

## A Telegram request

1. Telegram's fetcher asks for `https://fb.example.com/reel/123`.
2. The server checks it's Telegram: the user agent contains `TelegramBot` **and** the IP is in
   Telegram's ranges (`core.telegram.org/resources/cidr.txt`, refreshed daily, with a bundled
   copy as fallback). Anything else, a spoofed user agent included, is redirected to facebook.com
   and never causes a fetch.
3. The path becomes a page type and a cache key ([Facebook extraction](facebook.md)).
4. On a cache miss, extraction takes a slot from the Facebook budget (waiting up to 2 s) and must
   finish within the extraction timeout. Concurrent requests for one post share one fetch.
5. The answer is one page: preview tags in `<head>`, the Instant View article in `<body>`,
   compressed and marked `Cache-Control: no-store, no-transform`.

## Preview tags

- `og:title` names who posted: the author, `Author ▸ Group` in a group, or the page's own name for
  profiles, albums and pages without a post. Never counts: Telegram keeps previews for a long time.
- `og:description` is the text, cut to the description length, line breaks kept. With Instant View
  on, Telegram drops plain newlines there, so they're written as `<br>`, as FxEmbed does.
- `og:site_name` is the site name, plus which item the card shows when there are several:
  `Facebook · Photo 1 / 5`.
- Video: `og:type=video.other`, `og:video` (`video/mp4`) with its size; the poster in `og:image`.
  Over the largest-video size (20 MB, past which Telegram doesn't play videos in previews), the
  video uses Facebook's smaller file, as FxEmbed picks the best file under the limit. Sizes are
  estimated from the file's `bitrate=` and the duration.
- Photos: `og:image` is the first, with its size. Text posts get the author's picture, which
  Telegram shows small (`twitter:card=summary`): the full-size one from their profile page (a
  setting), else Facebook's 40 px one.
- After a failure, Telegram gets what "When extraction fails" says: by default a card saying why
  (login wall, not public, too slow) and that @WebpageBot refreshes it, as FxEmbed answers errors
  with a 200. Telegram keeps the first answer for a URL, so failures are cached for only 5 minutes.
- @WebpageBot's refresh sends `PURGE` (FxEmbed honours it too); from Telegram's addresses, `PURGE`
  or `DELETE` on a preview path drops the cached post, so the refresh gets it fresh. Cloudflare's
  edge answers `PURGE` itself (400) and never passes it on, so behind Cloudflare a refresh gets the
  cached post until it expires (failures: 5 minutes); `DELETE` does get through.

## Instant View

Telegram applies its Medium template to pages that present themselves as Medium articles
(`al:android:app_name=Medium` plus `article:published_time`), as FxEmbed does. The article is laid
out like FxEmbed's:

1. a "View on Facebook" link, the title, and the date (with the author when the title doesn't
   name them)
2. every photo or video
3. the full text, with links and `#hashtags` (to Facebook's hashtag page)
4. the counts as of the fetch, `👍 1.5K · 💬 12 · 🔁 79 · 👁️ 3.9K`, leaving out unknown and zero ones
   (a setting)
5. "View on Facebook", then "About the author": their full-size picture, profile link and bio

Links in the text go through `/_/l/{signature}?u=…`, which sends people on and answers 403 to
Telegram, so its Instant View builder never visits other sites, where one failing would break the
article (FxEmbed does the same). It follows only links the server signed, so it's no open
redirect.

The author's picture and bio come from their profile page, fetched logged out, within the
extraction time, and reused for six hours per author: one more Facebook request at most that
often. Facebook's own 40 px picture is never stretched across the article. It can be turned off per post type. Telegram doesn't document this, so it may
stop working.

## Media links

Videos always point at the server, `/_/v/{id}/{n}.mp4`, which redirects Telegram to the 1080p
file or Facebook's. Telegram doesn't play a card's video linked straight to Facebook's CDN, only
through a redirect on the preview host (Instant View plays either); FxEmbed works around the same.

With the `/_/m/` setting, photo and poster URLs point at the server too, which redirects to a
fresh CDN URL, refetching the post if the old one expired (for Telegram only). Only media of
posts the server extracted itself are served.

## 1080p videos

Off by default, and needs a bucket with S3's API (Cloudflare R2, Garage, S3...). Facebook's
720p file is the best one with sound; above it, video and audio are separate DASH files, and only
in AV1 or VP9, which older phones show as a black picture. With "Videos: up to 1080p" on:

- `/_/v/{id}/{n}.mp4` gets the joined file when it will fit under the size limit (20 MB,
  Telegram's limit for videos in previews), and `og:video` gets its size; only for the videos the
  page shows: the first, or all of them in Instant View
- the server downloads both files right away, joins them into one MP4 without re-encoding
  (`server/src/video/mp4.rs`), at most two at a time, and uploads it to the bucket
- `/_/v/` redirects Telegram to a link signed just then, for an hour. The server never sends
  video, and the bucket stays private, so its files can't be listed or linked to for long
- only requests from Telegram's addresses get the bucket's file or start a download; everyone
  else, and anything that goes wrong, gets Facebook's 720p file. Media IDs are made with a random
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

## Facebook requests

- Only to `facebook.com` hosts; redirects are followed by hand, at most 10, and must stay on
  Facebook.
- One warm HTTP/2 connection, direct or through the proxy setting.
- Cookie policy `fallback` (the default) sends cookies only after a login wall, keeping your
  account out of most fetches. Story cards always hit the wall, so they go logged in right away.
- Cookies live in `server.db` and are never logged. A paste without `c_user` and `xs` is
  refused.

## Limits and cache

| | Default | |
|---|---|---|
| Requests per IP | 120/min | Public listener; Telegram exempt; over it: `429` |
| Facebook fetches | 60/min, 4 at once | Cache misses from Telegram, the playground and test links |
| Cache | 64 MB | Posts 60 min (less if a media URL expires sooner), failures 5 min |

## Statistics

Every request is counted by outcome (redirect, hit, extracted, failed at a stage, busy, rate limited)
and page type, with timings, and saved each minute: per-minute for 7 days, hourly for 90 days.
The last 500 failures are kept with their URL and error.

## Test links and alerts

Test links are checked 2 minutes after start, then every 30 minutes, within the Facebook budget.
Each check extracts the post and fetches the start of its first media file (and, with 1080p on,
of both streams), as Telegram would.

Alerts go to the alert chat, sent with `TELEGRAM_BOT_TOKEN`, when:

- a test link starts failing, or works again
- the last 5 extractions for Telegram all failed, and again when one works
- 1080p videos couldn't be made (at most hourly)
- the video bucket fails, and again when it works
- Telegram's requests arrive from the front end's address: `CLIENT_IP_FROM` is wrong (daily)
- Facebook rejects the cookies, or they expire within 7 days
- the Facebook budget ran out more than 10 times in half an hour
