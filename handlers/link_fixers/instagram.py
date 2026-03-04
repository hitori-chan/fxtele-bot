"""Instagram link fixer."""

import re

from core.registry import register_handler
from .base import LinkFixer


@register_handler("instagram")
class InstagramFixer(LinkFixer):
    """Replace Instagram URLs with zzinstagram.com."""

    name = "instagram"
    pattern = re.compile(r"https?://(?:www\.)?instagram\.com")
    replacement = "https://zzinstagram.com"
