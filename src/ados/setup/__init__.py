"""Setup and onboarding status facade for ADOS Drone Agent."""

from ados.setup.models import SetupStatus
from ados.setup.service import build_setup_status

__all__ = ["SetupStatus", "build_setup_status"]
