# Shared camera worker transport

This desktop-only Rust library supports the independently packaged Kerberos and Ring workers. It contains bounded MQTT subscription/reconnect/TLS handling, rate limits and pure validation helpers. It is not itself an installable plugin and has no provider dispatch, core application dependency, HTTP client or device commands.

The network loop sends the configured provider's connection status only after a successful correlated SUBACK for every requested filter. Provider implementations own event parsing and deduplication, and persist that state across broker reconnects. Notification and media output have independent 30-per-minute budgets. Best-effort event output uses a bounded nonblocking queue; status frames still wait for their writes to complete.

Automatic `live_view` and `mqtt_live` previews have a three-second deadline from MQTT receipt in the local output queue. The writer discards an expired preview before writing any of its bytes, without changing the worker protocol or expiring notifications, clips, or stills. This bounds queued traffic; it cannot interrupt a blocking pipe write that already started. Shutdown remains independent of the output writer.

The `tests/support` module provides private loopback MQTT fixtures for the worker subprocess suites. Unit tests run with `cargo test --locked --manifest-path desktop-plugins/camera-common/Cargo.toml`; worker suites additionally exercise authentication packets, topic filters, failed SUBACKs, TLS failure and cancellation while stdout is blocked.
