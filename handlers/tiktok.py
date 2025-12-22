import re

RE_TIKTOK = re.compile(r"https?://(?:www\.|vt\.)?tiktok\.com")


async def handle_tiktok(text: str) -> dict[str, str] | None:
    """Replace TikTok URLs with privacy-friendly alternatives."""
    fixed = text
    fixed = RE_TIKTOK.sub("https://www.tfxktok.com", fixed)

    if fixed != text:
        return {"type": "text", "text": fixed}
    return None
