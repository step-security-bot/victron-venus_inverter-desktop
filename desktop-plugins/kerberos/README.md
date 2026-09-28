# Kerberos desktop worker

`inverter-desktop.kerberos` version 0.3.0 requires desktop host API 1.9. It is independently installable and does not require Frigate or Home Assistant. Android and iOS builds reject this package at build time.

The worker subscribes to native Agent motion topics. The default filters are `kerberos/agent/+;kerberos/hub/+`; custom filters may replace the final identity with an exact ID. The broker host, port, TLS setting and write-only username/password belong to this plugin. No core MQTT or HA credentials are inherited.

Standalone Agent messages must contain `motion`. Hub messages must contain an unencrypted, non-hidden `payload.action=motion`, an exact device identity, and a positive timestamp within 60 seconds behind or 10 seconds ahead of the current clock. Conflicting outer device IDs, retained messages, oversized payloads and legacy clip URLs are ignored.

**Excluded legacy cameras** (`excluded_legacy_cameras`) is an optional JSON array
of exact, case-sensitive Agent/Hub camera IDs. It suppresses only their legacy
motion notifications and live previews. For a shared Hub topic, matching uses
the payload's validated `device_id`, not the Hub topic suffix or display name.
The separate MQTT URL preview path remains active even for the same camera ID.
Empty or absent settings preserve all legacy cameras; other cameras and broker
subscriptions are unchanged. The list holds up to 32 valid identities of 128
UTF-8 bytes each, within 8 KiB of JSON.

Agent and Hub messages share a 15-second silence boundary per camera. Repeated movement extends the same episode. A fresh episode receives a distinct notification ID; MQTT reconnects keep the current process's episode history.

`camera_labels` is an optional JSON object mapping exact camera identities to names. `camera_live_urls` is a private JSON object mapping those identities to HTTP(S) image or MJPEG stream URLs. The editor replaces the complete private mapping without revealing saved URLs. Queries and fragments are allowed; embedded URL credentials are rejected. Both maps hold at most 32 cameras, with identities up to 128 UTF-8 bytes. Labels are limited to 96 bytes, live URLs to 2,048 bytes, the public map to 8,192 bytes, and the private map to 16,384 bytes. The complete configuration must fit in 32 KiB.

If the exact camera identity has a private live destination, motion requests an automatic 15-second preview without a duplicate native notification covering it. No notification click is required. An unmapped camera produces only its ordinary motion notification. The host resolves the configured URL, owns the isolated preview window, and closes it when its time expires, the plugin stops, settings change, or authentication expires. The private URL is never part of MQTT, dashboard snapshots, or emitted worker frames.

The manifest explicitly declares `live_view.preview_duration_seconds: 15`. Automatic previews share the host's camera window slots, admission expiry and global media budget. Their per-camera cooldown uses the exact camera identity rather than its display label and preserves the 15-second silence boundary. They do not download or cache a clip before opening, steal focus, or require notification permission. Each motion episode produces either a preview or an ordinary notification.

Transport limits are 16 provider-specific filters, a 4,096-byte filter list, 16 KiB incoming payloads, 512 recent camera identities and 30 outgoing notifications per minute. Unsupported global wildcard filters are rejected before any connection. Slow host output cannot delay shutdown or block MQTT polling. Network errors expose only generic connection status.

Run `cargo test --locked --manifest-path desktop-plugins/kerberos/Cargo.toml` and `cargo clippy --locked --manifest-path desktop-plugins/kerberos/Cargo.toml --all-targets -- -D warnings`. Unit tests cover silence/timestamp boundaries, exact identity mapping, equal display labels and bounded parsing; subprocess tests verify preview-only frames and unmapped-camera notifications, private URL exclusion, retained-event suppression and reconnect deduplication using local fixture brokers. The installed-package fixture `plugins::camera_integration_tests::signed_kerberos_package_real_mqtt_lifecycle` additionally checks native preview admission, private URL ownership and closure on disable, logout and uninstall. No fixture contacts real cameras or sends an OS notification.

## Home Assistant MQTT URL previews

The optional **MQTT live camera endpoints** (`mqtt_live_endpoints`) setting is a
JSON object mapping exact MQTT camera IDs to token-free HTTPS HA endpoints:

```json
{ "front": "https://ha.example/api/camera_proxy_stream/camera.front" }
```

For each configured ID, the existing MQTT client also subscribes to
`homelab/cameras/live/<ID>` at QoS 0. No new connection, package, or changes to
`mqtt_topics` are needed. Empty configuration disables these subscriptions.
Endpoints must be canonical HTTPS URLs, with the exact HA camera proxy stream
path, and without credentials, a query or fragment. Up to 32 endpoints fit in
16 KiB. The combined subscription packet is bounded to 8 KiB.

Each event is just the UTF-8 endpoint followed by `?token=<current-camera-token>`.
Only that URL-safe camera access token may vary. Wrong camera/path/origin/port,
extra query parameters, encoded path tricks, retained messages, non-UTF-8 and
oversized payloads are ignored. Invalid events do not consume the camera cooldown.
The worker sends one `mqtt_live` frame through the private host pipe; the host
independently validates the ID and URL against the startup settings. Tokens are
never stored as settings, logged, or included in dashboard/manager snapshots.
They are delivered only to the owning temporary viewer.

A fresh admitted event opens a 15-second MJPEG preview with the plugin's
**Always on top** preference and no native motion notification. A 15-second
cooldown is keyed by camera ID, survives MQTT reconnects in the worker, and
cannot be bypassed by rotating the token. A duplicate never extends the current
window. Existing native Kerberos Agent/Hub handling is unchanged. No stream is
opened at startup, during reconnect, or just because an endpoint is configured.

The publisher must send QoS 0, non-retained events for actual motion transitions.
Plain URLs carry no event timestamp: the consumer can reject retained delivery
and expire its own queue, but cannot distinguish a freshly republished old URL
from a fresh motion event. The MJPEG viewer uses an exact-origin image CSP; it
is not an HTTP client with a strict redirect-denial policy. Configure endpoints
that serve MJPEG directly without redirects. No HA long-lived access token,
Authorization header, RTSP credentials, HLS or WebRTC are used.

The installed-package acceptance test
`plugins::camera_integration_tests::signed_kerberos_package_mqtt_url_preview_lifecycle`
uses a private real MQTT broker to verify subscriptions, token refresh, native
URL ownership, retained/reconnect suppression, actual timed expiry and disabling.
It never contacts household cameras or updates the installed desktop application.
