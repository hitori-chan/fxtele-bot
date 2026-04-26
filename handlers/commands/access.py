"""Owner-only Telegram access-control commands."""

from telegram import Update
from telegram.error import TelegramError
from telegram.ext import Application, ChatMemberHandler, CommandHandler, ContextTypes

from services.access_control import AccessControl

from .menu import clear_owner_group_menu, set_owner_group_menu

GROUP_CHAT_TYPES = {"group", "supergroup"}
ACTIVE_MEMBER_STATUSES = {"member", "administrator", "creator"}


def load_access_commands(app: Application, access_control: AccessControl) -> None:
    """Register access-control commands and membership updates."""
    app.add_handler(CommandHandler("allowuser", allow_user(access_control)))
    app.add_handler(CommandHandler("denyuser", deny_user(access_control)))
    app.add_handler(CommandHandler("listusers", list_users(access_control)))
    app.add_handler(CommandHandler("allowgroup", allow_group(access_control)))
    app.add_handler(CommandHandler("denygroup", deny_group(access_control)))
    app.add_handler(CommandHandler("listgroups", list_groups(access_control)))
    app.add_handler(CommandHandler("access", access_status(access_control)))
    app.add_handler(ChatMemberHandler(my_chat_member(access_control), ChatMemberHandler.MY_CHAT_MEMBER))


def allow_user(access_control: AccessControl):
    async def callback(update: Update, context: ContextTypes.DEFAULT_TYPE) -> None:
        if not await _owner_required(update, context, access_control):
            return
        user_id = _target_user_id(update, context)
        if user_id is None:
            await _reply(update, "Usage: /allowuser <user_id> or reply with /allowuser")
            return
        changed = access_control.allow_user(user_id)
        message = f"Allowed user {user_id}." if changed else f"User {user_id} is already allowed."
        await _reply(update, message)

    return callback


def deny_user(access_control: AccessControl):
    async def callback(update: Update, context: ContextTypes.DEFAULT_TYPE) -> None:
        if not await _owner_required(update, context, access_control):
            return
        user_id = _target_user_id(update, context)
        if user_id is None:
            await _reply(update, "Usage: /denyuser <user_id> or reply with /denyuser")
            return
        if user_id == access_control.owner_id:
            await _reply(update, "Owner access is implicit and cannot be revoked.")
            return
        changed = access_control.deny_user(user_id)
        message = f"Denied user {user_id}." if changed else f"User {user_id} was not allowed."
        await _reply(update, message)

    return callback


def list_users(access_control: AccessControl):
    async def callback(update: Update, context: ContextTypes.DEFAULT_TYPE) -> None:
        if not await _owner_required(update, context, access_control):
            return
        allowed = ", ".join(str(user_id) for user_id in access_control.allowed_user_ids) or "none"
        await _reply(update, f"Owner: {access_control.owner_id}\nAllowed users: {allowed}")

    return callback


def allow_group(access_control: AccessControl):
    async def callback(update: Update, context: ContextTypes.DEFAULT_TYPE) -> None:
        if not await _owner_required(update, context, access_control):
            return
        chat_id = _target_chat_id(update, context)
        if chat_id is None:
            await _reply(update, "Usage: /allowgroup <chat_id> or run /allowgroup in a group.")
            return
        if chat_id >= 0:
            await _reply(update, "Group chat_id must be negative.")
            return
        changed = access_control.allow_chat(chat_id)
        if changed:
            await set_owner_group_menu(context.bot, chat_id, access_control.owner_id)
        message = f"Allowed group {chat_id}." if changed else f"Group {chat_id} is already allowed."
        await _reply(update, message)

    return callback


def deny_group(access_control: AccessControl):
    async def callback(update: Update, context: ContextTypes.DEFAULT_TYPE) -> None:
        if not await _owner_required(update, context, access_control):
            return
        chat_id = _target_chat_id(update, context)
        if chat_id is None:
            await _reply(update, "Usage: /denygroup <chat_id> or run /denygroup in a group.")
            return
        changed = access_control.deny_chat(chat_id)
        if changed:
            await clear_owner_group_menu(context.bot, chat_id, access_control.owner_id)
        message = f"Denied group {chat_id}." if changed else f"Group {chat_id} was not allowed."
        await _reply(update, message)
        if (
            update.effective_chat
            and update.effective_chat.id == chat_id
            and update.effective_chat.type in GROUP_CHAT_TYPES
        ):
            await _leave_chat_safely(context, chat_id)

    return callback


def list_groups(access_control: AccessControl):
    async def callback(update: Update, context: ContextTypes.DEFAULT_TYPE) -> None:
        if not await _owner_required(update, context, access_control):
            return
        allowed = ", ".join(str(chat_id) for chat_id in access_control.allowed_chat_ids) or "none"
        await _reply(update, f"Allowed groups: {allowed}")

    return callback


def access_status(access_control: AccessControl):
    async def callback(update: Update, context: ContextTypes.DEFAULT_TYPE) -> None:
        if not await _owner_required(update, context, access_control):
            return
        chat = update.effective_chat
        user = update.effective_user
        lines = [
            "Access status",
            "",
            "Owner",
            f"  ID: {access_control.owner_id}",
            "",
            "Current context",
            f"  User: {_display_id(user.id if user else None)} ({_allowed_label(access_control.is_user_allowed(user.id if user else None))})",
            f"  Chat: {_display_id(chat.id if chat else None)} ({chat.type if chat else 'unknown'}, {_allowed_label(access_control.is_chat_allowed(chat.id if chat else None))})",
            "",
            "Allowlist",
            f"  Users: {len(access_control.allowed_user_ids)}",
            f"  Groups: {len(access_control.allowed_chat_ids)}",
        ]
        await _reply(update, "\n".join(lines))

    return callback


def my_chat_member(access_control: AccessControl):
    async def callback(update: Update, context: ContextTypes.DEFAULT_TYPE) -> None:
        member_update = update.my_chat_member
        if not member_update or member_update.chat.type not in GROUP_CHAT_TYPES:
            return

        old_status = member_update.old_chat_member.status
        new_status = member_update.new_chat_member.status
        if old_status in ACTIVE_MEMBER_STATUSES or new_status not in ACTIVE_MEMBER_STATUSES:
            return

        chat_id = member_update.chat.id
        actor_id = member_update.from_user.id if member_update.from_user else None
        if actor_id == access_control.owner_id:
            if access_control.allow_chat(chat_id):
                await set_owner_group_menu(context.bot, chat_id, access_control.owner_id)
            await context.bot.send_message(chat_id, "Group approved.")
            return

        await _leave_chat_safely(context, chat_id)

    return callback


async def _owner_required(update: Update, context: ContextTypes.DEFAULT_TYPE, access_control: AccessControl) -> bool:
    user = update.effective_user
    if user and user.id == access_control.owner_id:
        return True
    chat = update.effective_chat
    if chat and chat.type in GROUP_CHAT_TYPES and not access_control.is_chat_allowed(chat.id):
        await _leave_chat_safely(context, chat.id)
    return False


def _target_user_id(update: Update, context: ContextTypes.DEFAULT_TYPE) -> int | None:
    if context.args:
        return _parse_id(context.args[0])
    message = update.message
    if message and message.reply_to_message and message.reply_to_message.from_user:
        return message.reply_to_message.from_user.id
    return None


def _target_chat_id(update: Update, context: ContextTypes.DEFAULT_TYPE) -> int | None:
    if context.args:
        return _parse_id(context.args[0])
    chat = update.effective_chat
    if chat and chat.type in GROUP_CHAT_TYPES:
        return chat.id
    return None


def _parse_id(value: str) -> int | None:
    try:
        return int(value)
    except ValueError:
        return None


async def _reply(update: Update, text: str) -> None:
    if update.message:
        await update.message.reply_text(text)


async def _leave_chat_safely(context: ContextTypes.DEFAULT_TYPE, chat_id: int) -> None:
    try:
        await context.bot.leave_chat(chat_id)
    except TelegramError:
        return


def _allowed_label(value: bool) -> str:
    return "allowed" if value else "not allowed"


def _display_id(value: int | None) -> str:
    return str(value) if value is not None else "unknown"
