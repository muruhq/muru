"""A WebSocket over HTTP/2, by the extended CONNECT of RFC 8441, for the
tests of what the edge relays: no WebSocket client library speaks it, so
this one opens the stream with h2 and frames the messages itself.
"""

from __future__ import annotations

import os
import socket

import h2.config
import h2.connection
import h2.events
import h2.settings


def _masked_text(text: str) -> bytes:
    """One masked text frame, as a client sends it."""

    data = text.encode()
    assert len(data) < 126, "a short message keeps the framing simple"
    mask = os.urandom(4)
    masked = bytes(byte ^ mask[index % 4] for index, byte in enumerate(data))
    return bytes([0x81, 0x80 | len(data)]) + mask + masked


def echo_over_http2(
    port: int,
    path: str,
    message: str,
    authority: str,
    headers: dict[str, str] | None = None,
) -> tuple[int, str]:
    """Open a WebSocket at `path` over cleartext HTTP/2, send `message` and
    read one text message back.

    Returns the status the CONNECT was answered with and the message, which
    is empty when the status is not 200.
    """

    connection = h2.connection.H2Connection(
        h2.config.H2Configuration(client_side=True, header_encoding="utf-8")
    )
    sock = socket.create_connection(("127.0.0.1", port), timeout=10)

    def events() -> list[h2.events.Event]:
        data = sock.recv(65536)
        assert data, "the edge closed the connection"
        received = connection.receive_data(data)
        sock.sendall(connection.data_to_send())
        return received

    try:
        connection.initiate_connection()
        sock.sendall(connection.data_to_send())

        # The edge's settings must allow extended CONNECT before the
        # stream may open with :protocol.

        allowed = False

        while not allowed:
            for event in events():
                if isinstance(event, h2.events.RemoteSettingsChanged):
                    changed = event.changed_settings.get(
                        h2.settings.SettingCodes.ENABLE_CONNECT_PROTOCOL
                    )
                    allowed = changed is not None and changed.new_value == 1

        stream = connection.get_next_available_stream_id()
        request = [
            (":method", "CONNECT"),
            (":protocol", "websocket"),
            (":scheme", "http"),
            (":path", path),
            (":authority", authority),
            ("sec-websocket-version", "13"),
            *(headers or {}).items(),
        ]
        connection.send_headers(stream, [(name.lower(), value) for name, value in request])
        sock.sendall(connection.data_to_send())

        # The answer, then the echo of the message, which arrives as one
        # unmasked text frame.

        status = 0
        received = b""

        while True:
            for event in events():
                if isinstance(event, h2.events.ResponseReceived):
                    status = int(dict(event.headers)[":status"])

                    if status != 200:
                        return status, ""

                    connection.send_data(stream, _masked_text(message))
                    sock.sendall(connection.data_to_send())
                elif isinstance(event, h2.events.DataReceived):
                    connection.acknowledge_received_data(event.flow_controlled_length, stream)
                    sock.sendall(connection.data_to_send())
                    received += event.data
                elif isinstance(event, h2.events.StreamReset):
                    return status, ""

            if len(received) >= 2 and len(received) >= 2 + received[1]:
                assert received[0] == 0x81, received
                return status, received[2 : 2 + received[1]].decode()
    finally:
        sock.close()
