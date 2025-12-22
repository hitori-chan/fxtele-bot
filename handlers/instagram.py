import re

RE_INSTAGRAM = re.compile(r"https?://(?:www\.)?instagram\.com")


async def handle_instagram(text: str) -> dict[str, str] | None:
    """Replace Instagram URLs with privacy-friendly alternatives."""
    fixed = text
    fixed = RE_INSTAGRAM.sub("https://zzinstagram.com", fixed)

    if fixed != text:
        return {"type": "text", "text": fixed}
    return None
