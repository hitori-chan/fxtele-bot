# Telegram Access Control

This document describes the runtime access model for `fxtele-bot`.

## State

Access state is stored as JSON at `telegram.access_state_path`, normally `/app/data/access_control.json`.

```json
{
  "allowed_chats": [
    {
      "id": -1001234567890,
      "label": "Example Group",
      "username": null
    }
  ],
  "allowed_users": [
    {
      "id": 123456789,
      "label": "Example User",
      "username": "example"
    }
  ],
  "denied_users": []
}
```

Startup seeds from `config.toml` are additive. They never clear IDs already persisted by owner commands.

The access state file uses object arrays only. Legacy `allowed_chat_ids`, `allowed_user_ids`, and `denied_user_ids` keys are not supported in the state file.

Entry fields:

- `id`: authoritative Telegram user or chat ID.
- `label`: last-seen human display name.
- `username`: last-seen Telegram username, without `@`, or `null`.

## User Rules

- Owner: always allowed and cannot be denied or reset.
- Allowed user: can use private chat, inline mode, and allowed groups.
- Neutral user: can use allowed groups only.
- Denied user: blocked everywhere, including allowed groups.

## Group Rules

- Allowed group: any non-denied member can use the bot.
- Neutral group: the bot leaves when it receives an unapproved group interaction.
- Stale allowed group: removed automatically only when Telegram confirms the bot is absent.

## Commands

Only the owner can run access commands.

```text
/allow <user_id|negative_group_chat_id>
/deny <user_id|negative_group_chat_id>
/reset <user_id|negative_group_chat_id>
/status
```

Replying to a user with `/allow`, `/deny`, or `/reset` targets that user.

Bare `/allow`, `/deny`, and `/reset` do not target the current group. In private chat or group chat, they show usage help. This prevents accidental group denial and accidental bot leave.

`/status` is private-only. In groups, it returns a short private-only message and does not print access lists.

Group IDs must be passed explicitly:

```text
/allow -1001234567890
/deny -1001234567890
/reset -1001234567890
```

`/allow <group_id>` is owner intent. If the bot is not currently in that group, the group remains allowed and pending; the command does not clean it up.

For groups, `/deny <group_id>` and `/reset <group_id>` both remove approval. If the command is issued from inside that same group, the bot leaves after removing approval.

## Bot Admission

Telegram membership updates are handled through `ChatMemberHandler.MY_CHAT_MEMBER`.

When the bot is added to a group:

- Owner added the bot: the group is approved, the bot stays, and the owner command menu is set.
- Allowed user added the bot to an allowed group: the bot stays.
- Allowed user added the bot to a neutral group: the bot leaves.
- Neutral or denied user added the bot: the bot leaves.

Allowed users can add the bot back only to groups that are already allowed.

## Stale Group Cleanup

There is no Telegram API that lists every group a bot is currently in. Cleanup uses explicit evidence:

- Startup probe: for each stored allowed group, the bot checks Telegram while setting the owner menu. If Telegram says the bot is absent from that chat, the group is removed from `allowed_chats`.
- Membership event: if Telegram reports the bot changed from active membership to `left`, `kicked`, or another inactive state, the group is removed from `allowed_chats`.

`/allow <group_id>` never triggers stale cleanup. It preserves the owner intent so another admin can add the bot later.

## Owner Command Menu

The owner command menu is scoped with `BotCommandScopeChatMember(chat_id, owner_id)`.

The private owner menu includes `/status`. Group owner menus do not include `/status` because status output is private-only.

The menu is set only when Telegram can see both:

- the bot in the group
- the owner in the group

Menu status is tracked internally as:

- `ready`: owner menu was set
- `bot absent`: bot is not in the group
- `owner absent`: owner is not in the group
- `error`: unexpected Telegram API failure

At startup, `bot absent` also removes the stale allowed group. In command flows, `bot absent` is treated as pending intent unless Telegram sent a bot membership-removal event.

## Logging

Access logs are written as human-readable decisions:

- bot added and staying
- bot added and leaving, with the reason
- stale allowed group removed
- owner command menu ready or pending
- unexpected Telegram API errors

Expected races such as "bot already absent" are logged as completed or pending states, not as failed actions.

## Labels

Labels and usernames are maintained opportunistically from Telegram updates:

- command sender and current chat
- replied-to users for `/allow`, `/deny`, and `/reset`
- bot membership updates for group labels and actor labels

Labels are display hints only. Access decisions always use numeric IDs.
