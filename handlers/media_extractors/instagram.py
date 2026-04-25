"""Instagram media extractor."""

import logging
import re
from urllib.parse import urlparse

import httpx
from lxml import html

from config import USER_AGENT, HTTP_TIMEOUT
from core.registry import register_handler
from core.types import HandlerResult, HandlerType
from services.http import get_client
from .base import MediaExtractor

logger = logging.getLogger(__name__)

RE_INSTAGRAM = re.compile(r"(https?://(?:www\.)?instagram\.com/\S+)")


@register_handler("instagram")
class InstagramExtractor(MediaExtractor):
    """Extract media from Instagram via vxinstagram.com."""

    name = "instagram"
    url_pattern = RE_INSTAGRAM

    async def _extract_media(self, url: str) -> HandlerResult | None:
        """Extract media from vxinstagram version of the URL."""
        vx_url = url.replace("instagram.com", "vxinstagram.com")

        try:
            client = get_client()
            headers = {"User-Agent": USER_AGENT}

            if not client:
                async with httpx.AsyncClient(timeout=HTTP_TIMEOUT) as temp_client:
                    response = await temp_client.get(
                        vx_url, headers=headers, follow_redirects=True
                    )
            else:
                response = await client.get(
                    vx_url, headers=headers, follow_redirects=True
                )

            response.raise_for_status()

            tree = html.fromstring(response.text)
            
            # Find all links containing d.rapidcdn.app/thumb
            # These can be in src, href or other attributes
            xpath_query = (
                '//@src[contains(., "d.rapidcdn.app/thumb")] | '
                '//@href[contains(., "d.rapidcdn.app/thumb")]'
            )
            media_elements = tree.xpath(xpath_query)

            if media_elements:
                media_urls = [vx_url]
                for elem in media_elements:
                    m_url = str(elem)
                    # Handle relative URLs
                    if m_url.startswith("/"):
                        parsed_vx = urlparse(vx_url)
                        m_url = f"{parsed_vx.scheme}://{parsed_vx.netloc}{m_url}"
                    media_urls.append(m_url)

                # Remove duplicates while preserving order
                unique_urls = list(dict.fromkeys(media_urls))

                return HandlerResult(
                    type=HandlerType.MEDIA_EXTRACTOR,
                    content=unique_urls,
                    metadata={"original_url": vx_url},
                )

            logger.warning(f"No d.rapidcdn.app/thumb links found in {vx_url}")
            # If we couldn't extract direct media, we still return the fixed link
            # but as a LINK_FIXER to avoid broken media preview
            return HandlerResult(
                type=HandlerType.LINK_FIXER,
                content=vx_url,
            )

        except Exception as e:
            logger.error(f"Error extracting Instagram media from {vx_url}: {e}")
            # Fallback to simple link replacement on error
            return HandlerResult(
                type=HandlerType.LINK_FIXER,
                content=vx_url,
            )
