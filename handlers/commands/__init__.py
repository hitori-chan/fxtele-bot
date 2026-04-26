"""Bot command registration."""

from telegram.ext import Application

from services.access_control import AccessControl

from .access import load_access_commands
from .menu import setup_bot_menu


def load_commands(app: Application, access_control: AccessControl) -> None:
    """Load command handlers into the application."""
    load_access_commands(app, access_control)


__all__ = ["load_commands", "setup_bot_menu"]
