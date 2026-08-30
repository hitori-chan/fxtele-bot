"""Manages the Node.js subprocess that signs anonyig.com Instagram requests.

anonyig.com validates each convert request against a signature computed by its
own client-side JS, which is protected by heavy control-flow obfuscation that
makes re-deriving the algorithm impractical. Instead, services/anonyig_signer/
signer.js drives a jsdom copy of the real page and calls that JS directly.
This module owns the subprocess lifecycle and the JSON-lines request protocol.
"""

import asyncio
import itertools
import json
import logging
from pathlib import Path
from typing import Any

logger = logging.getLogger(__name__)

_SIGNER_SCRIPT = Path(__file__).resolve().parent / "anonyig_signer" / "signer.js"
_REQUEST_TIMEOUT = 30.0

_process: asyncio.subprocess.Process | None = None
_reader_task: asyncio.Task | None = None
_stderr_task: asyncio.Task | None = None
_pending: dict[int, asyncio.Future] = {}
_stdin_lock = asyncio.Lock()
_next_id = itertools.count(1)


async def init_anonyig_signer(app) -> None:
    """Start the Node.js signer subprocess on bot startup."""
    global _process, _reader_task, _stderr_task

    try:
        _process = await asyncio.create_subprocess_exec(
            "node",
            str(_SIGNER_SCRIPT),
            stdin=asyncio.subprocess.PIPE,
            stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.PIPE,
        )
    except (OSError, FileNotFoundError) as e:
        logger.warning("Could not start anonyig signer subprocess (Node.js unavailable?): %r.", e)
        _process = None
        return

    _reader_task = asyncio.create_task(_read_responses())
    _stderr_task = asyncio.create_task(_drain_stderr())
    logger.info("Started anonyig signer subprocess.")


async def shutdown_anonyig_signer(app) -> None:
    """Terminate the Node.js signer subprocess on bot shutdown."""
    global _process, _reader_task, _stderr_task

    for task in (_reader_task, _stderr_task):
        if task:
            task.cancel()
    _reader_task = None
    _stderr_task = None

    process = _process
    _process = None
    if process is None:
        return

    process.terminate()
    try:
        await asyncio.wait_for(process.wait(), timeout=5)
    except asyncio.TimeoutError:
        process.kill()
        await process.wait()
    logger.info("Stopped anonyig signer subprocess.")


async def sign_target_url(url: str) -> dict[str, Any] | None:
    """Return a freshly-signed anonyig convert payload for url, or None if unavailable."""
    process = _process
    if process is None or process.stdin is None:
        return None

    request_id = next(_next_id)
    future: asyncio.Future = asyncio.get_running_loop().create_future()
    _pending[request_id] = future

    request_line = json.dumps({"id": request_id, "target_url": url}) + "\n"
    try:
        async with _stdin_lock:
            process.stdin.write(request_line.encode())
            await process.stdin.drain()
        response = await asyncio.wait_for(future, timeout=_REQUEST_TIMEOUT)
    except Exception as e:
        logger.warning("anonyig signer request failed for %s: %r.", url, e)
        return None
    finally:
        _pending.pop(request_id, None)

    if not response.get("ok"):
        logger.warning("anonyig signer returned an error for %s: %s.", url, response.get("error"))
        return None
    return response.get("payload")


async def _read_responses() -> None:
    """Match signer stdout lines back to their pending request futures."""
    process = _process
    assert process and process.stdout
    try:
        while True:
            line = await process.stdout.readline()
            if not line:
                break
            try:
                response = json.loads(line)
            except json.JSONDecodeError:
                logger.warning("anonyig signer produced non-JSON output: %r.", line)
                continue

            future = _pending.get(response.get("id"))
            if future and not future.done():
                future.set_result(response)
    finally:
        for future in _pending.values():
            if not future.done():
                future.set_exception(RuntimeError("anonyig signer process exited"))


async def _drain_stderr() -> None:
    """Forward the signer's diagnostic stderr output into our own logs."""
    process = _process
    assert process and process.stderr
    while True:
        line = await process.stderr.readline()
        if not line:
            break
        logger.debug("anonyig signer: %s", line.decode(errors="replace").rstrip())
