"""Driver-layer exception types.

Driver implementations raise these for predictable failure modes so the
peripheral manager can translate them into ``sensor.<id>.error`` events
and surface a meaningful reason in the GCS detail page.

The base ``DriverError`` chains under :class:`ados.plugins.errors.PluginError`
so a driver crash flows through the same supervisor error path as any
other plugin fault.
"""

from __future__ import annotations

from ados.plugins.errors import PluginError


class DriverError(PluginError):
    """Base class for driver-layer failures.

    Use this for predictable, recoverable conditions (device disconnected
    mid-stream, bus contention, calibration timeout). Unhandled exceptions
    are not expected to subclass this and will fall through to the
    plugin supervisor's circuit breaker.
    """


class DriverDeviceNotFound(DriverError):
    """Raised when a driver cannot locate the device a candidate referenced.

    Typical cause is the device was unplugged between :meth:`discover` and
    :meth:`open`, or between two ``open`` attempts during a hotplug race.
    """


class DriverPermissionDenied(DriverError):
    """Raised when a driver lacks the OS permission needed to claim a device.

    Examples include a missing udev rule for a USB vendor-id, an unreadable
    ``/dev/spidev*`` node, or a denied capability token at the IPC boundary.
    """
