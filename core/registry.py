"""Handler registration and discovery."""

import importlib
import pkgutil
from typing import Type

from .types import MessageHandler

# Global registry
_handler_registry: dict[str, Type[MessageHandler]] = {}


def register_handler(name: str):
    """
    Decorator to register a handler class.

    Args:
        name: Unique name for the handler

    Example:
        @register_handler("x")
        class XFixer(LinkFixer):
            ...
    """

    def decorator(cls: Type[MessageHandler]) -> Type[MessageHandler]:
        _handler_registry[name] = cls
        cls.name = name  # Set name on class
        return cls

    return decorator


def get_handler(name: str) -> Type[MessageHandler] | None:
    """Get a handler class by name."""
    return _handler_registry.get(name)


def list_handlers() -> dict[str, Type[MessageHandler]]:
    """Get all registered handlers."""
    return _handler_registry.copy()


def discover_handlers(*package_paths: str) -> list[MessageHandler]:
    """
    Auto-discover and instantiate handlers from packages.

    Args:
        package_paths: Package paths to scan (e.g., "handlers.link_fixers")

    Returns:
        List of instantiated handlers
    """
    instances = []

    for package_path in package_paths:
        try:
            package = importlib.import_module(package_path)
        except ImportError:
            continue

        # Walk through all modules in package
        for _, module_name, _ in pkgutil.iter_modules(
            package.__path__, prefix=f"{package_path}."
        ):
            try:
                importlib.import_module(module_name)
            except ImportError:
                continue

    # Instantiate all registered handlers
    for name, cls in _handler_registry.items():
        try:
            instances.append(cls())
        except Exception as e:
            import logging

            logging.getLogger(__name__).warning(
                f"Failed to instantiate handler {name}: {e}"
            )

    return instances


def clear_registry():
    """Clear all registered handlers. Useful for testing."""
    _handler_registry.clear()
