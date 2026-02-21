import logging
from telegram import Update
from telegram.ext import ContextTypes

logger = logging.getLogger(__name__)


async def handle_avatar(update: Update, context: ContextTypes.DEFAULT_TYPE) -> None:
    """Handle /avatar — replies with the target user's profile photo.

    Usage:
      • Reply to any message and send /avatar
      • /avatar <user_id>  (numeric Telegram user ID)
    """
    message = update.effective_message
    target_user = None
    label = None

    # replied-to message
    if message.reply_to_message:
        target_user = message.reply_to_message.from_user
        if target_user:
            label = f"@{target_user.username}" if target_user.username else target_user.full_name

    # numeric ID
    if target_user is None and context.args:
        arg = context.args[0]
        lookup = int(arg) if arg.lstrip("-").isdigit() else f"@{arg.lstrip('@')}"
        try:
            chat = await context.bot.get_chat(lookup)
            label = f"@{chat.username}" if chat.username else chat.full_name

            class _User:
                id = chat.id

            target_user = _User()
        except Exception as e:
            logger.warning("Failed to resolve %s: %s", lookup, e)
            await message.reply_text(
                f"Could not resolve {arg}.\n"
                "Telegram only allows username lookup for users the bot has previously seen.\n"
                "Try replying to their message and sending /avatar instead."
            )
            return

    if target_user is None:
        await message.reply_text(
            "Usage:\n"
            "  • Reply to someone's message and send /avatar\n"
            "  • /avatar @username\n"
            "  • /avatar <user_id>"
        )
        return

    try:
        photos = await context.bot.get_user_profile_photos(target_user.id, limit=1)
    except Exception as e:
        logger.warning("Failed to get profile photos for %s: %s", label, e)
        await message.reply_text(f"Could not retrieve photos for {label}.")
        return

    if not photos.photos:
        await message.reply_text(f"{label} has no profile photo.")
        return

    best = photos.photos[0][-1]
    await message.reply_photo(
        photo=best.file_id,
        caption=f"Avatar of {label}",
    )
