"""Telegram bot command menu setup."""

import logging

from telegram import Bot, BotCommand, BotCommandScopeChat, BotCommandScopeChatMember
from telegram.error import TelegramError
from telegram.ext import Application

from services.access_control import AccessControl

logger = logging.getLogger(__name__)

OWNER_COMMANDS = (
    BotCommand("allowuser", "Allow a user"),
    BotCommand("denyuser", "Deny a user"),
    BotCommand("listusers", "List allowed users"),
    BotCommand("allowgroup", "Allow a group"),
    BotCommand("denygroup", "Deny a group"),
    BotCommand("listgroups", "List allowed groups"),
    BotCommand("access", "Show access status"),
)


async def setup_bot_menu(app: Application, access_control: AccessControl) -> None:
    """Install owner-only command menus and clear the public default menu."""
    await app.bot.delete_my_commands()
    await set_owner_private_menu(app.bot, access_control.owner_id)
    for chat_id in access_control.allowed_chat_ids:
        await set_owner_group_menu(app.bot, chat_id, access_control.owner_id)


async def set_owner_private_menu(bot: Bot, owner_id: int) -> None:
    """Show command menu only in the owner's private chat."""
    await bot.set_my_commands(OWNER_COMMANDS, scope=BotCommandScopeChat(owner_id))


async def set_owner_group_menu(bot: Bot, chat_id: int, owner_id: int) -> None:
    """Show command menu only to the owner in an allowed group."""
    try:
        await bot.set_my_commands(OWNER_COMMANDS, scope=BotCommandScopeChatMember(chat_id, owner_id))
    except TelegramError as e:
        logger.info("Could not set owner command menu for chat %s: %s", chat_id, e)


async def clear_owner_group_menu(bot: Bot, chat_id: int, owner_id: int) -> None:
    """Remove the owner's scoped menu from a denied group."""
    try:
        await bot.delete_my_commands(scope=BotCommandScopeChatMember(chat_id, owner_id))
    except TelegramError as e:
        logger.info("Could not clear owner command menu for chat %s: %s", chat_id, e)
