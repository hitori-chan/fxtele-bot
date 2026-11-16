# Facebook extraction notes

Where the data sits in Facebook's pages, and how `server/src/extract/facebook` reads it. It started from the old Python extractor (`handlers/media_extractors/facebook.py`, the `legacy-python` tag) and has grown from testing real pages since. Facebook documents none of this, so when extraction changes, update this file with it, and check the saved-page snapshots (`server/tests/fixtures/`).

## URL handling

- Accepted hosts: `facebook.com` and any `*.facebook.com`, over `http`/`https` only.
- Normalizing: keep only the query parameters `story_fbid, id, fbid, v, set` and drop the fragment.
- **Media URLs at full size:** the `dl` parameter is removed, and so is `ctp` whenever `cstp` is present. CDN image URLs carry `cstp=mx{w}x{h}` (the original's size) and `ctp=s{w}x{h}` (the size delivered). The signature (`oh`) covers `cstp` but not `ctp`, so without `ctp` the CDN serves the original: grid thumbnails (310×414) become 1440×1920 photos, profile pictures go from 40×40 to 1080×1080. Removing `cstp` too gives "URL signature mismatch". A photo's width and height then come from `cstp`. Not documented by Facebook; found by testing in November 2026, and covered by the saved-page snapshots, which show when it changes.
- **Video posters:** the creator's cover (`preferred_thumbnail`) when it has the video's shape, else the first frame (`first_frame_thumbnail`, on reel pages under `short_form_video_context.video`). Reel covers are cut to 9:16 whatever the video's shape, and a mismatched poster gets cropped or letterboxed.
- **Video quality:** the best file with sound is the progressive `browser_native_hd_url` (720p on the pages tested; the `efg` tag says `…_720p`). 1080p exists only as DASH, with video and audio in separate files, which Telegram can't play as one.
- Login wall: the final URL path contains `/login`, `/checkpoint` or `/two_step_verification`.
- Redirects are followed by hand, at most 10, and must stay on Facebook.
- Share links (`/share/p/...`, `/share/r/...`, `/share/v/...`): if a redirect lands on `story.php` or `permalink.php` with both `story_fbid` and `id`, jump straight to `https://www.facebook.com/{id}/posts/{story_fbid}` before Facebook puts it behind the login wall.
- Request headers: browser-like (`Accept: text/html...`, `Accept-Language: en-US`, `Sec-Fetch-*: navigate/document/none/?1`, `Upgrade-Insecure-Requests: 1`, `DNT: 1`, `Sec-GPC: 1`, `Priority: u=0, i`, Chrome user agent).

## Page types (decided by path)

| Type | Path | Expected ID |
|---|---|---|
| reel | `/reel/{id}` | `{id}` |
| video | `/{page}/videos/{id}`, `/{page}/videos/{slug}/{id}` | `{id}` |
| photo | `/photo`, `/photo/`, `*/photo.php` with `?fbid=` (`photo.php` is keyed as `/photo`); `/{page}/photos/{set}/{id}`, `/{page}/photos/{id}` | `fbid`, else the last path part |
| watch | `/watch`, `/watch/live`, `/video.php` with `?v=` | `v` |
| story card | `/stories/{bucket}/{card}` | `{card}` (third path part) |
| story | `/permalink.php` or `/story.php` with `?story_fbid=`, `/{page}/posts/{token}`, `/groups/{g}/posts/{id}`, `/groups/{g}/permalink/{id}`, `/share/...` | story token(s) |
| profile | `/profile.php?id=`, `/people/{name}/{id}`, or a single path part `/{name}` that isn't a section of the site | checked separately |
| album | `/media/set/` with `?set=` | the set token |
| other | everything else: events, groups, marketplace, page tabs... | none; meta tags only |

If a page type needs an ID or story token and none is found, nothing is extracted.

## Where the data is in the page

- **Media scripts:** `<script type="application/json" data-sjs>` whose text contains `__bbox` and at least one of `videoDeliveryLegacyFields`, `currMedia`, `node_v2`, `attachments`.
- **Route scripts:** the same kind of script containing `initialRouteInfo`.
- **Meta tags:** `og:title`, `og:description`, `og:image`, `al:ios:url`, `al:android:url`.

## Media by page type

Each lookup runs against every JSON node of the media scripts. Duplicates (same media URL) are dropped. Video uses the HD URL, then SD; photos use the image.

- **reel:** `video.{ id, creation_story.short_form_video_context.playback_video.{videoDeliveryLegacyFields.browser_native_hd_url | browser_native_sd_url, preferred_thumbnail.image.uri}, creation_story.message.text }`. `video.id` must equal the reel ID.
- **video:** `video.story.attachments[].media.{ id, videoDeliveryLegacyFields.browser_native_hd_url | _sd_url, preferred_thumbnail.image.uri, creation_story.message.text | creation_story.comet_sections.message.story.message.text }`. `media.id` must equal the ID.
- **photo:** `currMedia.{ id, image.uri, creation_story.message.text }`. `currMedia.id` must equal `fbid`.
- **story card:** for the node whose `id == card`: `attachments[].media.{ image.uri, playable_url_quality_hd | playable_url, previewImage.uri | image.uri, message.text }`.
- **watch, and the fallback for reel/video:** see "Video node fallback" below. If that finds nothing for watch, use DASH prefetch: `all_video_dash_prefetch_representations[]`, keeping only entries whose `video_id` (if present) equals `v`, then `representations[]` with `mime_type == "video/mp4"`, picking the highest `bandwidth` `base_url`. The thumbnail and caption come from the node whose `id == v`.
- **story:**
  - Only look at nodes that belong to the requested post. A node belongs if any of `post_id, story_fbid, story_token, url, permalink_url, www_url` (or the same fields under `node_v2.`) contains a target story token.
  - Within those nodes, run these lookups:
    - `node_v2.attachments[].styles.attachment.media`
    - `node_v2.comet_sections.content.story.attachments[].styles.attachment.media`
    - `all_subattachments.nodes[].media`
    - `node_v2.attachments[].styles.attachment.all_subattachments.nodes[].media`
    - `node_v2.comet_sections.content.story.attachments[].styles.attachment.all_subattachments.nodes[].media`
  - Read each media item as:
    - **photo:** `viewer_image.uri | photo_image.uri | image.uri | massive_image.uri`
    - **video HD:** `playable_url_quality_hd | videoDeliveryLegacyFields.browser_native_hd_url`
    - **video SD:** `playable_url | videoDeliveryLegacyFields.browser_native_sd_url`
    - **thumbnail:** `previewImage.uri | preferred_thumbnail.image.uri | photo_image.uri | image.uri`
    - **caption:** `creation_story.message.text | creation_story.comet_sections.message.story.message.text`
  - If nothing is found, collect the IDs of `attachments[].media` items whose `__typename == "Video"` from the matching nodes, then try the video node fallback for each one, and then DASH prefetch.
  - Don't search the whole page broadly: feed and suggested media leak in.
- **Story tokens:** from the URL (`story_fbid` parameter, or the path part after `permalink` or `posts`). For `/share/...` URLs only, also from route scripts: any node's `params.story_token` or `params.story_fbid`. Tokens from the URL come first.

## Video node fallback

For the node whose `id` equals the target:

1. `videoDeliveryLegacyFields.browser_native_hd_url | browser_native_sd_url`.
2. Otherwise, anywhere inside it, `progressive_url`, ranked by `metadata.quality` (HD over SD over others).
3. Otherwise, a `base_url` with `mime_type == "video/mp4"`, highest `bandwidth`.

- **Thumbnail:** `first_frame_thumbnail | preferred_thumbnail.image.uri | thumbnailImage.uri | previewImage.uri | image.uri`, on the node first, then on anything inside it.
- **Caption:** `creation_story.message.text`, then `creation_story.comet_sections.message.story.message.text`.

## Text, title and canonical URL

- **The post's own nodes:** for stories, the nodes naming a story token; for reels, videos and watch pages, the story the video belongs to (a node with `creation_time` whose `attachments[].media` is the video, plus every node sharing that story's `id`, since Facebook splits one story across several query results), then the nodes carrying the media's ID. Logged-in pages keep a reel's caption, time and author only on that story.
- **Caption,** first one found wins, and only from the post's own data, so the next reel in a feed can't lend its caption:
  1. the caption of any media found
  2. `message.text` on the post's own nodes (photos, a video's story)
  3. on those nodes, in this order (for stories, also inside them):
     - `node_v2.comet_sections.content.story.message.text`
     - `node_v2.comet_sections.content.story.comet_sections.message.story.message.text`
     - `node_v2.comet_sections.content.story.comet_sections.message_container.story.message.text`
     - `video.creation_story.message.text`
     - `currMedia.creation_story.message.text`
  4. the group post's message (below)
  5. `og:description`
- **Title:** the group post title, then `initialRouteInfo.route.meta.title` in route scripts, then `og:title`. Engagement counts are dropped ("44K views · 3.6K reactions | Someone on Reels" becomes "Someone on Reels"), and login-wall titles ("Log in or sign up to view", "Facebook") are ignored, text-only posts' and shares' included.
- **Post time:** the first `creation_time` on or inside the post's own nodes.
- **Author:** the first `actors[].name`, `owner.name` or `video_owner.name` there; for story cards, the bucket's `story_bucket_owner.name`.
- **Video size:** from the node carrying the video's ID (logged-in pages wrap it in `video.creation_story.attachments[].media`).
- **Group post behind a reel or video (types reel, video and watch):**
  - Find a node with a string `post_id` where `video.id == target` or any `attachments[].media` has `__typename == "Video"` and `id == target`.
  - Group ID: `to.id | feedback.associated_group.id`.
  - Group name: `to.name | feedback.associated_group.name | ...if_viewer_can_see_reel_group_attribution.name` (nested once or twice).
  - Canonical URL: `https://www.facebook.com/groups/{group_id}/permalink/{post_id}/`.
  - Title: `"{group name} | {seo_title or first line of the message}"`.
  - Caption: `message.text`.

## Text-only posts (story type only)

If no media is found, but a node belonging to the post has `attachments == []`, it's a text-only post. Its text comes from the caption lookups above, and its title from the route title or `og:title`. Requiring the explicit empty list means a login wall is never mistaken for a text post.

## Profiles

For profile-shaped URLs, if `al:ios:url` (or, without it, `al:android:url`) starts with `fb://profile/` and `og:image` exists:

- **Media:** `og:image`.
- **Title:** `og:title`.
- **Text:** `og:title` and `og:description`, with a leading `"{name}."` and follower counts ("773,616 followers · 8,248 talking about this.") removed from the description.
- **Not a profile after all** (a section of the site): previewed from meta tags, as below.

## Album pages and other pages

- **Album** (`/media/set/?set=a.…`): the page's `__typename == "Photo"` nodes, up to 30. The grid's thumbnails become full-size photos by the media-URL rule above. The owner comes from the album node (`id` = the set without `a.`) when its name is on the page. Public albums work logged out.
- **Other** pages, and album or profile pages without their own data: a preview from `og:title`, `og:description` and `og:image`, unless the title is a login wall's or the only image is a Facebook placeholder (`/rsrc.php/`).
- **Scripts kept:** those with `__bbox` and one of `videoDeliveryLegacyFields`, `currMedia`, `node_v2`, `attachments` or a `"__typename":"Photo"` node (album grids have none of the others).

## Logging in first

Story cards show logged-out visitors a login wall, so under the `fallback` policy they're fetched with cookies right away (when saved), saving a round trip.

## Album expansion (story type; with cookies whenever the policy allows them)

Expansion only gets the time left before the extraction deadline (less 300 ms); if it runs out, the post keeps the photos it embeds instead of failing.

If a node has a string `mediaset_token`, an `all_subattachments` object with `count` greater than `nodes.len()`, and a `url` containing a target story token:

1. Fetch `https://www.facebook.com/media/set/?set={token}&type=3`.
2. Collect `__typename == "Photo"` nodes (image from `viewer_image | photo_image | image | massive_image`), deduplicated by ID and URL, up to `count`.
3. Add them to the post's media, deduplicated by CDN filename (the last part of the URL path).
4. A login wall here means the session has expired.

## Cookies

- **Format:** a JSON list of cookie objects (Cookie-Editor's Export → JSON), or a `Cookie` header copied from the developer tools.
  - Accepted field aliases: `expirationDate → expires`, `host → domain`.
  - Default path: `/`.
- **Expired cookies** (`expires > 0` and in the past) are dropped.
- **Valid session:** requires both `c_user` and `xs`.
