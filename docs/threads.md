# Threads extraction

Threads documents no way to read a post, so this reads what its own pages carry. Checked on
2026-11-17, from a home address.

## Where the data is

A post's page, logged out, is about a megabyte and holds the post in a
`<script type="application/json" data-sjs>` (under `…/result/data/media`), in Instagram's media
shape: `caption`, `image_versions2`, `video_versions`, `carousel_media`, `video_dash_manifest`,
`like_count`, and `text_post_app_info` with the rest. The page reader keeps only the scripts with
`__bbox` and `text_post_app_info` (or an account's `biography`), so the rest of the page is never
stored.

| What | Where |
|---|---|
| Text | `text_post_app_info.text_fragments`, whose `link` fragments have the whole address (the caption may shorten it); `caption.text` without them. Inline stickers are left out. |
| A long post's attached text | `snippet_attachment_info.text_fragments`, after the text |
| A poll | `caption_add_on.poll.tallies` (`text`, `count`); logged out the counts are `null` |
| A link's card | `link_preview_attachment` (`url` behind `l.threads.com/?u=`, `image_url`): the address goes after the text, the picture is the post's when it has none |
| Media types | 1 photo, 2 video, 8 carousel, 19 text only |
| Counts | `like_count` (hidden with `like_and_view_counts_disabled`); `direct_reply_count`; `repost_count` plus `quote_count`, which Threads shows together |
| A quote | `share_info.quoted_post`, a whole post; its media are the card's when the quote has none |
| A reply | `reply_to_post_id`; the thread above it is on the page (`containing_thread`), else only `reply_to_author` |
| The community | `tag_header.tag_cluster_name` ("Tech Threads"), shown after the author as Threads does |
| An account | its page's `…/data/user`: `full_name`, `biography`, `hd_profile_pic_versions` |

Videos are progressive MP4s with sound, and a DASH manifest like Instagram's for 1080p.

## Links

Threads moved from threads.net, which redirects to threads.com with the same path; both are
read. Links reach the server on its own host (`THREADS_HOST`), path and query unchanged.

| Link | Cache key | How |
|---|---|---|
| `/@{name}/post/{code}`, with `/media`, a slug of the text or `?xmt=` after it | `/@{name}/post/{code}` | the page; a wrong name is redirected to the right one |
| `/t/{code}` | the link | one redirect to the page |
| `/share/{code}` | the link | followed, logged out, to the post it names |
| `/@{name}`, with `/replies`, `/media` or `/reposts` | `/@{name}` | the account's page, always logged out |
| anything else (search, the composer behind share buttons) | the path | nothing: logged out, Threads shows them only after a login (the composer) or without preview tags (search). People are sent on to Threads; Telegram gets "not a post" |

## Failures

| Answer | Means |
|---|---|
| a redirect to `/?error=invalid_post` | no such post: deleted, or a wrong code |
| a redirect to `/login` | a private account, an account that doesn't exist, or a login wall |
| with the cookies: an answer that deletes `sessionid` (a redirect to `/`) | the saved session is dead: the cookies are marked rejected. Nothing else marks them |
| 429 | rate limited |
| a page without the post's script | the page changed shape |

The cookie policy is `fallback`: logged out first, then with the saved cookies after a login wall
or "no such post". The cookies go only to www.threads.com, with the client's one browser user
agent, as Instagram logs out a session that shows up as another device. The budget is 20 fetches
a minute, 2 at once, as Meta blocks busy addresses as it does on Instagram, and "About the
author" costs a whole profile page per author every six hours, so it's off by default.

Fixtures (`threads-*.json`) keep the post and the post it answers, cut down to the fields read:
mentions are dropped, so the text comes from the caption there.
