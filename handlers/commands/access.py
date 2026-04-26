"""Owner-only Telegram access-control commands."""

from dataclasses import dataclass
import logging

from telegram import Update
from telegram.error import TelegramError
from telegram.ext import Application, ChatMemberHandler, CommandHandler, ContextTypes

from services.access_control import AccessControl, AccessControlError, AccessEntry
from utils.telegram_errors import bot_absent_from_chat
from utils.telegram_log import chat_label, chat_state_label, chat_username, user_label, user_state_label, user_username

from .menu import OwnerMenuStatus, clear_owner_group_menu, set_owner_group_menu, set_owner_group_menu_if_owner_present

logger = logging.getLogger(__name__)

GROUP_CHAT_TYPES = {"group", "supergroup"}
ACTIVE_MEMBER_STATUSES = {"member", "administrator", "creator"}


@dataclass(frozen=True)
class AccessTarget:
    kind: str
    value: int
    label: str | None = None
    username: str | None = None


def load_access_commands(app: Application, access_control: AccessControl) -> None:
    """Register access-control commands and membership updates."""
    app.add_handler(CommandHandler("allow", allow_entity(access_control)))
    app.add_handler(CommandHandler("deny", deny_entity(access_control)))
    app.add_handler(CommandHandler("reset", reset_entity(access_control)))
    app.add_handler(CommandHandler("status", access_status(access_control)))
    app.add_handler(ChatMemberHandler(my_chat_member(access_control), ChatMemberHandler.MY_CHAT_MEMBER))


def allow_entity(access_control: AccessControl):
    async def callback(update: Update, context: ContextTypes.DEFAULT_TYPE) -> None:
        _log_command(update, "allow")
        _remember_update(access_control, update)
        if not await _owner_required(update, context, access_control):
            return
        target = _target(update, context)
        if target is None:
            await _reply(update, _usage("allow"))
            return

        if target.kind == "user":
            access_control.remember_user(target.value, target.label, target.username)
            changed = access_control.allow_user(target.value)
            message = (
                f"Allowed user {_format_user(access_control, target.value)}."
                if changed
                else _unchanged_user_message(access_control, target.value)
            )
            await _reply(update, message)
            return

        try:
            access_control.remember_chat(target.value, target.label, target.username)
            changed = access_control.allow_chat(target.value)
        except AccessControlError as e:
            await _reply(update, str(e))
            return
        if changed:
            await set_owner_group_menu_if_owner_present(context.bot, target.value, access_control.owner_id)
        group = _format_chat(access_control, target.value)
        message = f"Allowed group {group}." if changed else f"Group {group} is already allowed."
        await _reply(update, message)

    return callback


def deny_entity(access_control: AccessControl):
    async def callback(update: Update, context: ContextTypes.DEFAULT_TYPE) -> None:
        _log_command(update, "deny")
        _remember_update(access_control, update)
        if not await _owner_required(update, context, access_control):
            return
        target = _target(update, context)
        if target is None:
            await _reply(update, _usage("deny"))
            return

        if target.kind == "user":
            if target.value == access_control.owner_id:
                await _reply(update, "Owner cannot be denied.")
                return
            access_control.remember_user(target.value, target.label, target.username)
            changed = access_control.deny_user(target.value)
            user = _format_user(access_control, target.value)
            message = f"Denied user {user}." if changed else f"User {user} is already denied."
            await _reply(update, message)
            return

        await _remove_group_access(update, context, access_control, target.value, verb="Denied")

    return callback


def reset_entity(access_control: AccessControl):
    async def callback(update: Update, context: ContextTypes.DEFAULT_TYPE) -> None:
        _log_command(update, "reset")
        _remember_update(access_control, update)
        if not await _owner_required(update, context, access_control):
            return
        target = _target(update, context)
        if target is None:
            await _reply(update, _usage("reset"))
            return

        if target.kind == "user":
            if target.value == access_control.owner_id:
                await _reply(update, "Owner cannot be reset.")
                return
            access_control.remember_user(target.value, target.label, target.username)
            changed = access_control.reset_user(target.value)
            message = (
                f"Reset user {_format_user(access_control, target.value)} to neutral."
                if changed
                else f"User {_format_user(access_control, target.value)} is already neutral."
            )
            await _reply(update, message)
            return

        await _remove_group_access(update, context, access_control, target.value, verb="Reset")

    return callback


def access_status(access_control: AccessControl):
    async def callback(update: Update, context: ContextTypes.DEFAULT_TYPE) -> None:
        _log_command(update, "status")
        _remember_update(access_control, update)
        if not await _owner_required(update, context, access_control):
            return
        if not _private_chat(update):
            await _reply(update, "Use /status in private chat.")
            return
        chat = update.effective_chat
        user = update.effective_user
        user_id = user.id if user else None
        chat_id = chat.id if chat else None
        lines = [
            "Access status",
            "",
            f"Owner: {access_control.owner_id}",
            "",
            "Current",
            f"  User {_format_current_user(update, user_id)}: {_user_status(access_control, user_id)}",
            f"  Chat {_format_current_chat(update, chat_id)}: {_chat_status(access_control, chat_id)}",
            "",
            "Users",
            f"  Allowed: {_format_users(access_control, access_control.allowed_user_ids)}",
            f"  Denied: {_format_users(access_control, access_control.denied_user_ids)}",
            "",
            "Groups",
            f"  Allowed: {_format_chats(access_control, access_control.allowed_chat_ids)}",
        ]
        await _reply(update, "\n".join(lines))

    return callback


def my_chat_member(access_control: AccessControl):
    async def callback(update: Update, context: ContextTypes.DEFAULT_TYPE) -> None:
        member_update = update.my_chat_member
        if not member_update or member_update.chat.type not in GROUP_CHAT_TYPES:
            return

        access_control.remember_chat(
            member_update.chat.id,
            chat_state_label(member_update.chat),
            chat_username(member_update.chat),
        )
        access_control.remember_user(
            member_update.from_user.id if member_update.from_user else None,
            user_state_label(member_update.from_user),
            user_username(member_update.from_user),
        )
        old_status = member_update.old_chat_member.status
        new_status = member_update.new_chat_member.status
        if old_status in ACTIVE_MEMBER_STATUSES and new_status not in ACTIVE_MEMBER_STATUSES:
            chat_id = member_update.chat.id
            if access_control.deny_chat(chat_id):
                logger.info(
                    "Removed allowed group %s after bot membership changed from %s to %s.",
                    chat_label(member_update.chat),
                    old_status,
                    new_status,
                )
            return

        if old_status in ACTIVE_MEMBER_STATUSES or new_status not in ACTIVE_MEMBER_STATUSES:
            return

        chat_id = member_update.chat.id
        actor_id = member_update.from_user.id if member_update.from_user else None
        actor_allowed = access_control.is_user_allowed(actor_id)
        chat_allowed = access_control.is_chat_allowed(chat_id)
        if actor_id == access_control.owner_id:
            changed = access_control.allow_chat(chat_id)
            await set_owner_group_menu(context.bot, chat_id, access_control.owner_id)
            logger.info(
                "Bot added to %s by owner %s: staying; %s.",
                chat_label(member_update.chat),
                user_label(member_update.from_user),
                "approved group" if changed else "group was already approved",
            )
            await context.bot.send_message(chat_id, "Group approved.")
            return

        if actor_allowed and chat_allowed:
            menu_status = await set_owner_group_menu_if_owner_present(
                context.bot,
                chat_id,
                access_control.owner_id,
            )
            logger.info(
                "Bot added to %s by allowed user %s: staying; group is allowed; owner menu %s.",
                chat_label(member_update.chat),
                user_label(member_update.from_user),
                "ready" if menu_status == OwnerMenuStatus.READY else "pending",
            )
            return

        logger.info(
            "Bot added to %s by %s: leaving; %s.",
            chat_label(member_update.chat),
            user_label(member_update.from_user),
            _membership_leave_reason(actor_allowed=actor_allowed, chat_allowed=chat_allowed),
        )
        await _leave_chat_safely(context, chat_id)

    return callback


async def _remove_group_access(
    update: Update,
    context: ContextTypes.DEFAULT_TYPE,
    access_control: AccessControl,
    chat_id: int,
    *,
    verb: str,
) -> None:
    changed = access_control.deny_chat(chat_id)
    if changed:
        await clear_owner_group_menu(context.bot, chat_id, access_control.owner_id)
    group = _format_chat(access_control, chat_id)
    message = f"{verb} group {group}." if changed else f"Group {group} is already neutral."
    await _reply(update, message)
    if update.effective_chat and update.effective_chat.id == chat_id and update.effective_chat.type in GROUP_CHAT_TYPES:
        await _leave_chat_safely(context, chat_id)


async def _owner_required(update: Update, context: ContextTypes.DEFAULT_TYPE, access_control: AccessControl) -> bool:
    user = update.effective_user
    if user and user.id == access_control.owner_id:
        return True
    logger.info(
        "Owner command from %s in %s blocked; user is not owner.",
        user_label(user),
        chat_label(update.effective_chat),
    )
    return False


def _log_command(update: Update, command: str) -> None:
    logger.info(
        "Command /%s from %s in %s.",
        command,
        user_label(update.effective_user),
        chat_label(update.effective_chat),
    )


def _target(update: Update, context: ContextTypes.DEFAULT_TYPE) -> AccessTarget | None:
    if context.args:
        value = _parse_id(context.args[0])
        if value is None:
            return None
        return AccessTarget("group" if value < 0 else "user", value)

    message = update.message
    if message and message.reply_to_message and message.reply_to_message.from_user:
        user = message.reply_to_message.from_user
        return AccessTarget("user", user.id, user_state_label(user), user_username(user))

    return None


def _parse_id(value: str) -> int | None:
    try:
        return int(value)
    except ValueError:
        return None


def _usage(command: str) -> str:
    return f"Usage: /{command} <user_id|group_chat_id>, or reply with /{command}."


def _private_chat(update: Update) -> bool:
    chat = update.effective_chat
    return bool(chat and chat.type == "private")


async def _reply(update: Update, text: str) -> None:
    if update.message:
        await update.message.reply_text(text)


async def _leave_chat_safely(context: ContextTypes.DEFAULT_TYPE, chat_id: int) -> None:
    try:
        await context.bot.leave_chat(chat_id)
    except TelegramError as e:
        if bot_absent_from_chat(e):
            logger.info("Leave already complete for chat %s; bot is absent.", chat_id)
            return
        logger.warning("Unexpected Telegram error while leaving chat %s: %s.", chat_id, e)


def _membership_leave_reason(*, actor_allowed: bool, chat_allowed: bool) -> str:
    if actor_allowed:
        return "group is not allowed"
    if chat_allowed:
        return "actor is not allowed"
    return "actor and group are not allowed"


def _unchanged_user_message(access_control: AccessControl, user_id: int) -> str:
    if user_id == access_control.owner_id:
        return "Owner is always allowed."
    return f"User {_format_user(access_control, user_id)} is already allowed."


def _user_status(access_control: AccessControl, user_id: int | None) -> str:
    if user_id is None:
        return "unknown"
    if user_id == access_control.owner_id:
        return "owner"
    if access_control.is_user_denied(user_id):
        return "denied"
    if access_control.is_user_allowed(user_id):
        return "allowed"
    return "neutral"


def _chat_status(access_control: AccessControl, chat_id: int | None) -> str:
    if chat_id is None:
        return "unknown"
    if chat_id >= 0:
        return "private"
    if access_control.is_chat_allowed(chat_id):
        return "allowed"
    return "neutral"


def _remember_update(access_control: AccessControl, update: Update) -> None:
    user = update.effective_user
    chat = update.effective_chat
    access_control.remember_user(user.id if user else None, user_state_label(user), user_username(user))
    access_control.remember_chat(chat.id if chat else None, chat_state_label(chat), chat_username(chat))


def _format_current_user(update: Update, user_id: int | None) -> str:
    if user_id is None:
        return "unknown"
    return _format_entry(user_id, user_state_label(update.effective_user), user_username(update.effective_user))


def _format_current_chat(update: Update, chat_id: int | None) -> str:
    if chat_id is None:
        return "unknown"
    return _format_entry(chat_id, chat_state_label(update.effective_chat), chat_username(update.effective_chat))


def _format_user(access_control: AccessControl, user_id: int) -> str:
    entry = access_control.user_entry(user_id)
    return _format_access_entry(entry)


def _format_chat(access_control: AccessControl, chat_id: int) -> str:
    entry = access_control.chat_entry(chat_id)
    return _format_access_entry(entry)


def _format_users(access_control: AccessControl, values: tuple[int, ...]) -> str:
    return ", ".join(_format_user(access_control, value) for value in values) or "none"


def _format_chats(access_control: AccessControl, values: tuple[int, ...]) -> str:
    return ", ".join(_format_chat(access_control, value) for value in values) or "none"


def _format_access_entry(entry: AccessEntry) -> str:
    return _format_entry(entry.id, entry.label, entry.username)


def _format_entry(item_id: int, label: str | None, username: str | None) -> str:
    details = []
    if label:
        details.append(label)
    username_label = f"@{username}" if username else None
    if username_label and username_label != label:
        details.append(username_label)
    if not details:
        return str(item_id)
    return f"{item_id} ({', '.join(details)})"
