"""Owner-only Telegram access-control commands."""

from dataclasses import dataclass
import logging

from telegram import Update
from telegram.error import TelegramError
from telegram.ext import Application, ChatMemberHandler, CommandHandler, ContextTypes

from services.access_control import AccessControl, AccessControlError
from utils.telegram_errors import bot_absent_from_chat
from utils.telegram_log import chat_label, user_label

from .menu import clear_owner_group_menu, set_owner_group_menu

logger = logging.getLogger(__name__)

GROUP_CHAT_TYPES = {"group", "supergroup"}
ACTIVE_MEMBER_STATUSES = {"member", "administrator", "creator"}


@dataclass(frozen=True)
class AccessTarget:
    kind: str
    value: int


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
        if not await _owner_required(update, context, access_control):
            return
        target = _target(update, context)
        if target is None:
            await _reply(update, _usage("allow"))
            return

        if target.kind == "user":
            changed = access_control.allow_user(target.value)
            message = (
                f"Allowed user {target.value}." if changed else _unchanged_user_message(access_control, target.value)
            )
            await _reply(update, message)
            return

        try:
            changed = access_control.allow_chat(target.value)
        except AccessControlError as e:
            await _reply(update, str(e))
            return
        if changed:
            await set_owner_group_menu(context.bot, target.value, access_control.owner_id)
        message = f"Allowed group {target.value}." if changed else f"Group {target.value} is already allowed."
        await _reply(update, message)

    return callback


def deny_entity(access_control: AccessControl):
    async def callback(update: Update, context: ContextTypes.DEFAULT_TYPE) -> None:
        _log_command(update, "deny")
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
            changed = access_control.deny_user(target.value)
            message = f"Denied user {target.value}." if changed else f"User {target.value} is already denied."
            await _reply(update, message)
            return

        await _remove_group_access(update, context, access_control, target.value, verb="Denied")

    return callback


def reset_entity(access_control: AccessControl):
    async def callback(update: Update, context: ContextTypes.DEFAULT_TYPE) -> None:
        _log_command(update, "reset")
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
            changed = access_control.reset_user(target.value)
            message = (
                f"Reset user {target.value} to neutral." if changed else f"User {target.value} is already neutral."
            )
            await _reply(update, message)
            return

        await _remove_group_access(update, context, access_control, target.value, verb="Reset")

    return callback


def access_status(access_control: AccessControl):
    async def callback(update: Update, context: ContextTypes.DEFAULT_TYPE) -> None:
        _log_command(update, "status")
        if not await _owner_required(update, context, access_control):
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
            f"  User {_display_id(user_id)}: {_user_status(access_control, user_id)}",
            f"  Chat {_display_id(chat_id)}: {_chat_status(access_control, chat_id)}",
            "",
            "Users",
            f"  Allowed: {_format_ids(access_control.allowed_user_ids)}",
            f"  Denied: {_format_ids(access_control.denied_user_ids)}",
            "",
            "Groups",
            f"  Allowed: {_format_ids(access_control.allowed_chat_ids)}",
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
            logger.info(
                "Bot added to %s by allowed user %s: staying; group is allowed.",
                chat_label(member_update.chat),
                user_label(member_update.from_user),
            )
            await set_owner_group_menu(context.bot, chat_id, access_control.owner_id)
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
    message = f"{verb} group {chat_id}." if changed else f"Group {chat_id} is already neutral."
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
        return AccessTarget("user", message.reply_to_message.from_user.id)

    chat = update.effective_chat
    if chat and chat.type in GROUP_CHAT_TYPES:
        return AccessTarget("group", chat.id)
    return None


def _parse_id(value: str) -> int | None:
    try:
        return int(value)
    except ValueError:
        return None


def _usage(command: str) -> str:
    return f"Usage: /{command} <user_id|group_chat_id>, reply with /{command}, or run /{command} in a group."


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
    return f"User {user_id} is already allowed."


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


def _display_id(value: int | None) -> str:
    return str(value) if value is not None else "unknown"


def _format_ids(values: tuple[int, ...]) -> str:
    return ", ".join(str(value) for value in values) or "none"
