import re

RE_YOUTUBE = re.compile(r"https?://(?:www\.|m\.)?(?:youtube\.com|youtu\.be)")


async def handle_youtube(text: str) -> dict[str, str] | None:
    """Replace YouTube URLs with koutube alternatives."""
    fixed = text
    # Replace youtube.com/watch?v=... and youtu.be/... with koutube.com equivalents
    fixed = RE_YOUTUBE.sub("https://koutube.com", fixed)

    if fixed != text:
        return {"type": "text", "text": fixed}
    return None
