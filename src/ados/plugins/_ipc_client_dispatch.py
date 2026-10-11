"""Host-pushed delivery handling of :class:`ados.plugins.ipc_client.PluginIpcClient`."""

from __future__ import annotations

import asyncio
import inspect
from typing import Any

from ados.plugins._dispatch_generated import REQUIRED_CAP
from ados.plugins._ipc_client_common import (
    _TOOL_INVOKE_METHOD,
    _IpcClientState,
    log,
)
from ados.plugins.rpc import (
    CapabilityToken,
    Envelope,
    TokenError,
    encode_frame,
)


class _DeliveryMixin(_IpcClientState):
    """Token refresh, ``tool.invoke`` handling and callback dispatch."""

    def _adopt_refreshed_token(self, env: Envelope) -> None:
        """Replace the token this client presents with the host's fresh one.

        Called for a ``token.refresh`` event. The host re-mints from
        authoritative state, so the new token reflects the operator's CURRENT
        grant set — a capability that was just revoked is absent from it, and
        the plugin's next call using that capability is correctly denied. The
        granted set is stored too so a ``tool.invoke`` gate and any plugin-side
        capability introspection stay consistent with the wire.

        A malformed payload is ignored rather than clearing the token: keeping
        a working token beats dropping to one that cannot authenticate over a
        bad frame.
        """
        token = (env.args or {}).get("token")
        if not isinstance(token, str) or not token:
            log.warning(
                "plugin_token_refresh_malformed", plugin_id=self._plugin_id
            )
            return
        try:
            parsed = CapabilityToken.from_string(token)
        except TokenError as exc:
            log.warning(
                "plugin_token_refresh_unparseable",
                plugin_id=self._plugin_id,
                error=str(exc),
            )
            return
        lost = self._granted_caps - parsed.granted_caps
        self._token = token
        self._granted_caps = parsed.granted_caps
        log.info(
            "plugin_token_refreshed",
            plugin_id=self._plugin_id,
            expires_at=parsed.expires_at,
            # Named explicitly: a plugin losing a capability mid-flight is
            # something its author needs to see in the log, not something to
            # discover through a later capability_denied.
            revoked=sorted(lost),
        )

    async def _handle_tool_invoke(self, env: Envelope) -> None:
        """Run a declared tool for a host ``tool.invoke`` request and reply.

        Gated on the cap the generated table names for ``tool.invoke``
        (``mcp.expose``): a plugin whose token lacks it never runs a tool. An
        unknown tool or a handler exception is surfaced as an error response,
        never a silent drop, so the host's pending future always resolves.
        """
        required = REQUIRED_CAP.get(_TOOL_INVOKE_METHOD)
        if required is not None and required not in self._granted_caps:
            await self._reply(env.request_id, args={}, error=f"capability_denied: {required}")
            return
        tool = env.args.get("tool")
        if not isinstance(tool, str):
            await self._reply(env.request_id, args={}, error="tool.invoke: missing tool name")
            return
        handler = self._tool_handlers.get(tool)
        if handler is None:
            await self._reply(env.request_id, args={}, error=f"tool_not_found: {tool}")
            return
        raw_args = env.args.get("arguments")
        arguments = raw_args if isinstance(raw_args, dict) else {}
        try:
            result = handler(arguments)
            if inspect.isawaitable(result):
                result = await result
        except Exception as exc:  # noqa: BLE001
            await self._reply(env.request_id, args={}, error=f"tool_error: {exc}")
            return
        payload = result if isinstance(result, dict) else {"result": result}
        await self._reply(env.request_id, args=payload, error=None)

    async def _reply(
        self, request_id: str, *, args: dict, error: str | None
    ) -> None:
        """Write a response envelope back to the host for a tool.invoke."""
        if self._writer is None:
            return
        env = Envelope(
            type="response",
            method="response",
            capability="",
            args=args,
            request_id=request_id,
            token="",
            error=error,
        )
        try:
            self._writer.write(encode_frame(env))
            await self._writer.drain()
        except (ConnectionError, BrokenPipeError):
            pass

    async def _dispatch_event(self, env: Envelope) -> None:
        topic: Any = env.args.get("topic")
        payload: Any = env.args.get("payload")
        if not isinstance(topic, str):
            return
        for pattern, callbacks in list(self._event_callbacks.items()):
            if pattern == topic or _matches(pattern, topic):
                for cb in callbacks:
                    try:
                        result = cb(payload if isinstance(payload, dict) else {})
                        if asyncio.iscoroutine(result):
                            await result
                    except Exception as exc:  # noqa: BLE001
                        log.error(
                            "plugin_event_callback_error",
                            plugin_id=self._plugin_id,
                            topic=topic,
                            error=str(exc),
                        )

    async def _dispatch_button(self, env: Envelope) -> None:
        # The press rides as a decoded map, not a byte blob: it is four small
        # scalars, so the callback gets it directly with no second decode step.
        press = env.args.get("press") or {}
        for cb in list(self._button_callbacks):
            try:
                result = cb(press)
                if asyncio.iscoroutine(result):
                    await result
            except Exception:  # noqa: BLE001 - one bad callback must not stop the stream
                log.exception("button_callback_failed")

    async def _dispatch_msp(self, env: Envelope) -> None:
        # The whole args map ({bytes, timestamp_ms}) goes to the callback; the
        # plugin's codec parses the raw MSP bytes.
        for cb in list(self._msp_callbacks):
            try:
                result = cb(env.args)
                if asyncio.iscoroutine(result):
                    await result
            except Exception:  # noqa: BLE001 - one bad callback must not stop the stream
                log.exception("msp_callback_failed")

    async def _dispatch_detection(self, env: Envelope) -> None:
        payload = {
            "batch": env.args.get("batch"),
            "timestamp_ms": env.args.get("timestamp_ms"),
        }
        for cb in list(self._detection_callbacks):
            try:
                result = cb(payload)
                if asyncio.iscoroutine(result):
                    await result
            except Exception as exc:  # noqa: BLE001
                log.error(
                    "plugin_detection_callback_error",
                    plugin_id=self._plugin_id,
                    error=str(exc),
                )

    async def _dispatch_mavlink(self, env: Envelope) -> None:
        msg_name: Any = env.args.get("msg_name")
        if not isinstance(msg_name, str):
            return
        frame: Any = env.args.get("frame")
        payload = {
            "msg_name": msg_name,
            "frame": frame if isinstance(frame, (bytes, bytearray)) else bytes(frame or b""),
            "timestamp_ms": env.args.get("timestamp_ms"),
        }
        for pattern, callbacks in list(self._mavlink_callbacks.items()):
            if pattern == msg_name or _matches(pattern, msg_name):
                for cb in callbacks:
                    try:
                        result = cb(payload)
                        if asyncio.iscoroutine(result):
                            await result
                    except Exception as exc:  # noqa: BLE001
                        log.error(
                            "plugin_mavlink_callback_error",
                            plugin_id=self._plugin_id,
                            msg_name=msg_name,
                            error=str(exc),
                        )


def _matches(pattern: str, topic: str) -> bool:
    import fnmatch as _fnm

    return _fnm.fnmatchcase(topic, pattern)
