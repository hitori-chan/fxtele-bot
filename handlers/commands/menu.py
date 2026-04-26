"""Telegram bot command menu setup."""

from enum import Enum
import logging

from telegram import Bot, BotCommand, BotCommandScopeChat, BotCommandScopeChatMember, ChatMember
from telegram.error import TelegramError
from telegram.ext import Application

from services.access_control import AccessControl
from utils.telegram_errors import bot_absent_from_chat

logger = logging.getLogger(__name__)

OWNER_GROUP_COMMANDS = (
    BotCommand("allow", "Allow a user or group"),
    BotCommand("deny", "Deny a user or group"),
    BotCommand("reset", "Reset a user or group"),
)
OWNER_PRIVATE_COMMANDS = (
    *OWNER_GROUP_COMMANDS,
    BotCommand("status", "Show access status"),
)


class OwnerMenuStatus(Enum):
    READY = "ready"
    BOT_ABSENT = "bot absent"
    OWNER_ABSENT = "owner absent"
    ERROR = "error"


async def setup_bot_menu(app: Application, access_control: AccessControl) -> None:
    """Install owner-only command menus and clear the public default menu."""
    await app.bot.delete_my_commands()
    await set_owner_private_menu(app.bot, access_control.owner_id)
    for chat_id in access_control.allowed_chat_ids:
        status = await set_owner_group_menu_if_owner_present(
            app.bot,
            chat_id,
            access_control.owner_id,
            log_bot_absent=False,
        )
        if status == OwnerMenuStatus.BOT_ABSENT and access_control.deny_chat(chat_id):
            logger.info(
                "Removed stale allowed group %s; bot is not in the chat.",
                chat_id,
            )


async def set_owner_private_menu(bot: Bot, owner_id: int) -> None:
    """Show command menu only in the owner's private chat."""
    await bot.set_my_commands(OWNER_PRIVATE_COMMANDS, scope=BotCommandScopeChat(owner_id))


async def set_owner_group_menu(
    bot: Bot,
    chat_id: int,
    owner_id: int,
    *,
    log_bot_absent: bool = True,
) -> OwnerMenuStatus:
    """Show command menu only to the owner in an allowed group."""
    try:
        await bot.set_my_commands(OWNER_GROUP_COMMANDS, scope=BotCommandScopeChatMember(chat_id, owner_id))
        return OwnerMenuStatus.READY
    except TelegramError as e:
        if bot_absent_from_chat(e):
            if log_bot_absent:
                logger.info(
                    "Owner command menu for chat %s pending; bot is not in the chat.",
                    chat_id,
                )
            return OwnerMenuStatus.BOT_ABSENT
        logger.warning("Unexpected Telegram error while setting owner command menu for chat %s: %s.", chat_id, e)
        return OwnerMenuStatus.ERROR


async def set_owner_group_menu_if_owner_present(
    bot: Bot,
    chat_id: int,
    owner_id: int,
    *,
    log_bot_absent: bool = True,
) -> OwnerMenuStatus:
    """Show owner menu in a group only when Telegram can see the owner there."""
    owner_status = await _owner_in_chat(bot, chat_id, owner_id, log_bot_absent=log_bot_absent)
    if owner_status != OwnerMenuStatus.READY:
        return owner_status
    return await set_owner_group_menu(bot, chat_id, owner_id, log_bot_absent=log_bot_absent)


async def clear_owner_group_menu(bot: Bot, chat_id: int, owner_id: int) -> None:
    """Remove the owner's scoped menu from a denied group."""
    try:
        await bot.delete_my_commands(scope=BotCommandScopeChatMember(chat_id, owner_id))
    except TelegramError as e:
        if bot_absent_from_chat(e):
            logger.info(
                "Owner command menu for chat %s already removed; bot is not in the chat.",
                chat_id,
            )
            return
        logger.warning("Unexpected Telegram error while clearing owner command menu for chat %s: %s.", chat_id, e)


async def _owner_in_chat(
    bot: Bot,
    chat_id: int,
    owner_id: int,
    *,
    log_bot_absent: bool = True,
) -> OwnerMenuStatus:
    try:
        member = await bot.get_chat_member(chat_id, owner_id)
    except TelegramError as e:
        if bot_absent_from_chat(e):
            if log_bot_absent:
                logger.info(
                    "Owner command menu for chat %s pending; bot is not in the chat.",
                    chat_id,
                )
            return OwnerMenuStatus.BOT_ABSENT
        logger.warning("Unexpected Telegram error while checking owner membership in chat %s: %s.", chat_id, e)
        return OwnerMenuStatus.ERROR

    if _active_member(member):
        return OwnerMenuStatus.READY

    logger.info(
        "Owner command menu for chat %s pending; owner is not in the chat.",
        chat_id,
    )
    return OwnerMenuStatus.OWNER_ABSENT


def _active_member(member: ChatMember) -> bool:
    if member.status in {ChatMember.MEMBER, ChatMember.ADMINISTRATOR, ChatMember.OWNER}:
        return True
    return member.status == ChatMember.RESTRICTED and getattr(member, "is_member", False)
