# Bot

In an allowed chat, the bot replies to a message with its links, domains swapped. Nothing else.

## Messages

- New and forwarded messages, including channel posts auto-forwarded into a discussion group.
  Messages sent directly by other bots are skipped.
- Links come from the ones Telegram marks in text and captions, deduplicated, at most
  `max_links_per_message`.
- A link matches a `[domains]` key when its host is that domain or a subdomain, and the whole
  host is replaced (`m.facebook.com/...` → `fb.example.com/...`). Path, query and fragment
  stay as sent. The most specific key wins.
- A value with a path puts it in front of the link's (`fb.watch/x` →
  `fb.example.com/fb.watch/x`, which is how the server tells Facebook's short video links
  from profiles). An empty value leaves the key's links alone, such as Facebook's redirects to
  other sites (`l.facebook.com`).
- One reply per message, links one per line, previewing the first link with large media. No
  matching links, no reply. A link that would take the reply past Telegram's 4096 characters is
  left out.
- Over a reply limit (per sender, per chat), the reply is skipped and logged.
- Logs hold chat IDs, counts and decisions, never message text.

## Access

A chat is allowed if it's the owner's private chat, the private chat of someone in
`allowed_users`, a group in `allowed_chats`, or a group the owner approved:

- When the owner adds the bot to a group, it's approved. When someone else does, the bot stays
  silent there and asks the owner to **Approve** or **Leave**, once per group however often it's
  added, for 20 groups at most at a time. The bot forgets unanswered prompts when it restarts.
- When the bot is removed from an approved group, the approval goes too.
- When a group becomes a supergroup, its approval (or its waiting prompt) moves to the new ID.
  For `allowed_chats` entries the new ID is logged, so you can update the file.

## Owner menu

In the owner's private chat only, and never from a forwarded message:

| Command | |
|---|---|
| `/menu` (or `/start`) | Buttons: **Add person**, **Add group** (Telegram's chat picker), **List** |
| `/allow <id>` | Add a person (positive ID) or group (negative ID) |
| `/remove <id>` | Remove one; removing a group also leaves it |

The list shows names and has a **Remove** button per entry. Entries from `bot.toml` can only be
changed in the file.
