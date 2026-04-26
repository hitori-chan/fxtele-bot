"""Persistent Telegram access-control state."""

from __future__ import annotations

from dataclasses import dataclass
import json
import os
from pathlib import Path
from tempfile import NamedTemporaryFile
from threading import RLock
from typing import Any


class AccessControlError(ValueError):
    """Raised when access-control state cannot be loaded safely."""


@dataclass(frozen=True)
class AccessSnapshot:
    """Serializable access-control state."""

    allowed_user_ids: tuple[int, ...]
    denied_user_ids: tuple[int, ...]
    allowed_chat_ids: tuple[int, ...]

    def to_json(self) -> dict[str, Any]:
        return {
            "allowed_user_ids": list(self.allowed_user_ids),
            "denied_user_ids": list(self.denied_user_ids),
            "allowed_chat_ids": list(self.allowed_chat_ids),
        }


class AccessControl:
    """Owner-controlled Telegram user and group allowlists."""

    def __init__(
        self,
        *,
        owner_id: int,
        path: Path,
        allowed_user_ids: set[int] | None = None,
        denied_user_ids: set[int] | None = None,
        allowed_chat_ids: set[int] | None = None,
    ) -> None:
        self.owner_id = owner_id
        self.path = path
        self._allowed_user_ids = set(allowed_user_ids or set())
        self._denied_user_ids = set(denied_user_ids or set())
        self._allowed_chat_ids = set(allowed_chat_ids or set())
        self._allowed_user_ids.discard(owner_id)
        self._denied_user_ids.discard(owner_id)
        self._allowed_user_ids.difference_update(self._denied_user_ids)
        self._lock = RLock()

    @classmethod
    def load(
        cls,
        *,
        owner_id: int,
        path: Path,
        seed_user_ids: set[int] | None = None,
        seed_chat_ids: set[int] | None = None,
    ) -> "AccessControl":
        """Load state, merge startup seeds, and persist the normalized result.

        Startup seeds are additive. They must never clear IDs that were already
        persisted by owner commands or prior startup seeds.
        """
        if owner_id <= 0:
            raise AccessControlError("telegram.owner_id must be a positive numeric user ID in config.toml")

        access = cls(owner_id=owner_id, path=path)
        changed = False
        if path.exists():
            snapshot = _read_snapshot(path)
            access = cls._from_snapshot(owner_id=owner_id, path=path, snapshot=snapshot)

        for user_id in seed_user_ids or set():
            if not access.is_user_denied(user_id):
                changed = access.allow_user(user_id, persist=False) or changed
        for chat_id in seed_chat_ids or set():
            changed = access.allow_chat(chat_id, persist=False) or changed

        if changed or not path.exists():
            access.save()
        return access

    @classmethod
    def _from_snapshot(cls, *, owner_id: int, path: Path, snapshot: AccessSnapshot) -> "AccessControl":
        return cls(
            owner_id=owner_id,
            path=path,
            allowed_user_ids=set(snapshot.allowed_user_ids),
            denied_user_ids=set(snapshot.denied_user_ids),
            allowed_chat_ids=set(snapshot.allowed_chat_ids),
        )

    @property
    def allowed_user_ids(self) -> tuple[int, ...]:
        with self._lock:
            return tuple(sorted(self._allowed_user_ids))

    @property
    def allowed_chat_ids(self) -> tuple[int, ...]:
        with self._lock:
            return tuple(sorted(self._allowed_chat_ids))

    @property
    def denied_user_ids(self) -> tuple[int, ...]:
        with self._lock:
            return tuple(sorted(self._denied_user_ids))

    def snapshot(self) -> AccessSnapshot:
        with self._lock:
            return AccessSnapshot(
                allowed_user_ids=tuple(sorted(self._allowed_user_ids)),
                denied_user_ids=tuple(sorted(self._denied_user_ids)),
                allowed_chat_ids=tuple(sorted(self._allowed_chat_ids)),
            )

    def save(self) -> None:
        """Persist state through an atomic replace."""
        with self._lock:
            self.path.parent.mkdir(parents=True, exist_ok=True)
            payload = json.dumps(self.snapshot().to_json(), indent=2, sort_keys=True)
            with NamedTemporaryFile("w", encoding="utf-8", dir=self.path.parent, delete=False) as tmp:
                tmp.write(payload)
                tmp.write("\n")
                tmp.flush()
                os.fsync(tmp.fileno())
                tmp_path = Path(tmp.name)
            os.replace(tmp_path, self.path)

    def is_user_allowed(self, user_id: int | None) -> bool:
        with self._lock:
            return user_id == self.owner_id or (
                user_id is not None and user_id in self._allowed_user_ids and user_id not in self._denied_user_ids
            )

    def is_user_denied(self, user_id: int | None) -> bool:
        with self._lock:
            return user_id is not None and user_id != self.owner_id and user_id in self._denied_user_ids

    def is_chat_allowed(self, chat_id: int | None) -> bool:
        with self._lock:
            return chat_id is not None and chat_id in self._allowed_chat_ids

    def allow_user(self, user_id: int, *, persist: bool = True) -> bool:
        with self._lock:
            if user_id == self.owner_id:
                return False
            changed = user_id not in self._allowed_user_ids or user_id in self._denied_user_ids
            if not changed:
                return False
            self._denied_user_ids.discard(user_id)
            self._allowed_user_ids.add(user_id)
            if persist:
                self.save()
            return True

    def deny_user(self, user_id: int, *, persist: bool = True) -> bool:
        with self._lock:
            if user_id == self.owner_id:
                return False
            changed = user_id in self._allowed_user_ids or user_id not in self._denied_user_ids
            if not changed:
                return False
            self._allowed_user_ids.discard(user_id)
            self._denied_user_ids.add(user_id)
            if persist:
                self.save()
            return True

    def reset_user(self, user_id: int, *, persist: bool = True) -> bool:
        with self._lock:
            if user_id == self.owner_id:
                return False
            changed = user_id in self._allowed_user_ids or user_id in self._denied_user_ids
            if not changed:
                return False
            self._allowed_user_ids.discard(user_id)
            self._denied_user_ids.discard(user_id)
            if persist:
                self.save()
            return True

    def allow_chat(self, chat_id: int, *, persist: bool = True) -> bool:
        if chat_id >= 0:
            raise AccessControlError(f"group chat_id must be negative: {chat_id}")
        with self._lock:
            if chat_id in self._allowed_chat_ids:
                return False
            self._allowed_chat_ids.add(chat_id)
            if persist:
                self.save()
            return True

    def deny_chat(self, chat_id: int, *, persist: bool = True) -> bool:
        with self._lock:
            if chat_id not in self._allowed_chat_ids:
                return False
            self._allowed_chat_ids.remove(chat_id)
            if persist:
                self.save()
            return True


def _read_snapshot(path: Path) -> AccessSnapshot:
    try:
        payload = json.loads(path.read_text(encoding="utf-8"))
    except json.JSONDecodeError as exc:
        raise AccessControlError(f"Invalid access-control JSON at {path}: {exc}") from exc
    except OSError as exc:
        raise AccessControlError(f"Cannot read access-control state at {path}: {exc}") from exc

    if not isinstance(payload, dict):
        raise AccessControlError(f"Invalid access-control state at {path}: root must be an object")

    allowed_chat_ids = tuple(_read_id_list(payload, "allowed_chat_ids", path))
    invalid_chat_ids = [chat_id for chat_id in allowed_chat_ids if chat_id >= 0]
    if invalid_chat_ids:
        raise AccessControlError(
            f"Invalid access-control state at {path}: allowed_chat_ids must be negative group chat IDs: {invalid_chat_ids!r}"
        )

    return AccessSnapshot(
        allowed_user_ids=tuple(_read_id_list(payload, "allowed_user_ids", path)),
        denied_user_ids=tuple(_read_id_list(payload, "denied_user_ids", path)),
        allowed_chat_ids=allowed_chat_ids,
    )


def _read_id_list(payload: dict[str, Any], key: str, path: Path) -> list[int]:
    value = payload.get(key)
    if not isinstance(value, list):
        raise AccessControlError(f"Invalid access-control state at {path}: {key} must be a list")
    ids: list[int] = []
    for item in value:
        if not isinstance(item, int) or isinstance(item, bool):
            raise AccessControlError(f"Invalid access-control state at {path}: {key} contains {item!r}")
        ids.append(item)
    return ids
