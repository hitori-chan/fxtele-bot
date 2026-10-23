"""Regression tests for how the anonyig signer subprocess is confined.

The signer evaluates JavaScript fetched from anonyig.com in jsdom, which can
reach the host process from inside the page, so Node's permission model and a
scrubbed environment are what keep a compromised bundle away from the bot.
"""

import json
import os
import subprocess
import tempfile
import unittest
from types import SimpleNamespace
from unittest.mock import AsyncMock, MagicMock, patch

from services import anonyig_signer
from services.anonyig_signer import _SIGNER_DIR, _SIGNER_SCRIPT, NODE_PERMISSION_ARGS, _signer_env

_ESCAPE_ATTEMPTS = """
const results = {};
const attempt = (name, fn) => { try { fn(); results[name] = "allowed"; } catch (e) { results[name] = e.code; } };
const fs = require("fs");
attempt("shell", () => require("child_process").execSync("id"));
attempt("read_elsewhere", () => fs.readFileSync("/etc/hostname"));
attempt("write", () => fs.writeFileSync(process.argv[1], "x"));
attempt("read_signer_dir", () => fs.readdirSync(%s));
results.token = process.env.TELEGRAM_BOT_TOKEN ?? null;
console.log(JSON.stringify(results));
"""


def _node_supports_the_flags() -> bool:
    try:
        result = subprocess.run(
            ["node", *NODE_PERMISSION_ARGS, "-e", "0"], capture_output=True, env=_signer_env(), timeout=30, check=False
        )
    except (OSError, subprocess.TimeoutExpired):
        return False
    return result.returncode == 0


class LaunchTests(unittest.IsolatedAsyncioTestCase):
    async def test_signer_starts_under_the_permission_model_without_the_bots_secrets(self):
        process = SimpleNamespace(
            stdin=MagicMock(),
            stdout=SimpleNamespace(readline=AsyncMock(return_value=b"")),
            stderr=SimpleNamespace(readline=AsyncMock(return_value=b"")),
            terminate=MagicMock(),
            wait=AsyncMock(),
        )
        spawn = AsyncMock(return_value=process)

        with (
            patch.object(anonyig_signer.asyncio, "create_subprocess_exec", spawn),
            patch.dict(os.environ, {"TELEGRAM_BOT_TOKEN": "123:SECRET", "HOME": "/home/bot"}),
        ):
            await anonyig_signer.init_anonyig_signer(None)
            await anonyig_signer.shutdown_anonyig_signer(None)

        self.assertEqual(spawn.await_args.args, ("node", *NODE_PERMISSION_ARGS, str(_SIGNER_SCRIPT)))
        env = spawn.await_args.kwargs["env"]
        self.assertEqual(set(env), {"PATH"})
        self.assertNotIn("SECRET", json.dumps(env))


class PolicyTests(unittest.TestCase):
    def test_only_the_signers_own_directory_is_readable_and_nothing_is_writable(self):
        self.assertIn("--permission", NODE_PERMISSION_ARGS)
        self.assertIn(f"--allow-fs-read={_SIGNER_DIR}", NODE_PERMISSION_ARGS)
        reads = [arg for arg in NODE_PERMISSION_ARGS if arg.startswith("--allow-fs-read")]
        self.assertEqual(len(reads), 1)
        for forbidden in ("child-process", "fs-write", "worker", "addons", "wasi", "ffi", "inspector"):
            self.assertFalse(
                any(forbidden in arg for arg in NODE_PERMISSION_ARGS), f"must not grant --allow-{forbidden}"
            )

    def test_environment_carries_only_path(self):
        with patch.dict(os.environ, {"TELEGRAM_BOT_TOKEN": "123:SECRET", "PATH": "/usr/bin"}):
            self.assertEqual(_signer_env(), {"PATH": "/usr/bin"})


@unittest.skipUnless(_node_supports_the_flags(), "node with --permission and --allow-net is not installed")
class ConfinementTests(unittest.TestCase):
    def test_script_running_in_the_signer_cannot_reach_the_host(self):
        with tempfile.TemporaryDirectory() as temp_dir:
            target = os.path.join(temp_dir, "pwned")
            code = _ESCAPE_ATTEMPTS % json.dumps(str(_SIGNER_DIR))
            with patch.dict(os.environ, {"TELEGRAM_BOT_TOKEN": "123:SECRET"}):
                env = _signer_env()
            result = subprocess.run(
                ["node", *NODE_PERMISSION_ARGS, "-e", code, target],
                capture_output=True,
                text=True,
                env=env,
                timeout=30,
                check=True,
            )
            self.assertFalse(os.path.exists(target))

        self.assertEqual(
            json.loads(result.stdout),
            {
                "shell": "ERR_ACCESS_DENIED",
                "read_elsewhere": "ERR_ACCESS_DENIED",
                "write": "ERR_ACCESS_DENIED",
                "read_signer_dir": "allowed",
                "token": None,
            },
        )


if __name__ == "__main__":
    unittest.main()
