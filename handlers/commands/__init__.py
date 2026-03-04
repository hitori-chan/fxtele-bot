"""Bot command handlers."""

from telegram.ext import Application


def load_commands(app: Application) -> None:
    """
    Load all command handlers into the application.

    Args:
        app: The telegram Application instance
    """
    # No commands registered currently
    pass


__all__ = ["load_commands"]
