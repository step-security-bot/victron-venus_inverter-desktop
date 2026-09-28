# Frigate worker

`inverter-frigate-worker` is the optional desktop package
`inverter-desktop.frigate`, version 0.4.0, requiring host API `^1.8`. It opens its
own MQTT connection, emits connection status, and requests a 15-second live
preview when a direct Frigate HTTP address is configured. Motion without a
preview sends an ordinary notification. It belongs to the generic **Cameras** plugin group, so the dashboard
group control changes only installed camera packages and their saved desired state.

It contains no Tauri, core MQTT client, HA client, inverter controls or frontend
code. Kerberos and Ring use separate optional packages. Configured archives use
exact saved SHA-256 pins; manual signed packages retain publisher policy.
Application/executable signing or notarization is not required by either path.
Android and iOS exclude these packages and the desktop plugin runtime.

**Excluded cameras** (`excluded_cameras`) is an optional JSON array of exact,
case-sensitive Frigate camera IDs. It suppresses motion notifications, live
previews and legacy completed clips for those cameras in this desktop plugin.
It does not change recording or detection on the Frigate server. Empty or absent
settings include every camera. The list supports up to 32 IDs of 128 UTF-8 bytes
each, within 8 KiB of JSON; wildcards and display-name matching are not applied.
Excluded messages do not consume event history or notification budgets.

## Configuration

The package uses the existing native settings editor and encrypted per-plugin
record. It receives configuration through its startup pipe, acknowledges the exact
revision, and only then opens a network connection. Saving settings restarts an
enabled worker; disabling or uninstalling closes its connection. Credentials are
absent from command-line arguments, inherited environment, dashboard status, and
worker diagnostics.

- `mqtt_host`: required hostname or IP address, at most 253 bytes. Do not include
  a scheme, path, username, or port. IPv6 addresses use their unbracketed form.
- `mqtt_port`: integer 1–65535; default 1883. Enabling TLS does not change this
  value automatically; select the actual TLS listener port explicitly.
- `mqtt_tls`: default false. When true, certificate and hostname verification use
  the platform trust store through rustls. There is no insecure certificate mode
  or plaintext fallback. Client-certificate authentication is not implemented.
- `mqtt_topic`: one exact topic, default `frigate/events`, at most 256 bytes. MQTT
  wildcards and the legacy semicolon-separated topic-list syntax are rejected.
- `mqtt_username` and `mqtt_password`: optional write-only secrets, at most 1,024
  bytes each, without control characters. A nonempty password requires a username.
- `frigate_base_url`: optional direct HTTP(S) address, at most 2,048 UTF-8 bytes.
  Include the actual port and any reverse-proxy path prefix, for example
  `http://frigate.local:5000` or `https://camera.example/frigate`. Missing, null,
  or blank values keep motion-only operation. User information, query strings,
  fragments, control characters, and ambiguous path normalization are rejected.
  There are no HTTP credentials or injected headers in this slice; the host does
  not reuse HA credentials to access the direct Frigate API.

The worker connects only to the configured broker and subscribes only to the exact
configured topic at QoS 0. It publishes no messages. The `network_mqtt` permission
is a declaration for this native executable, not an OS firewall or an arbitrary
core MQTT publishing API. HA is not required for this connection.

## Motion and recovery

The parser accepts Frigate `type: new` messages with nonempty `after.id` and
`after.camera`, each bounded to 128 UTF-8 bytes. It emits `Frigate <Camera> camera motion detected` with body
`Motion started`. Names are normalized to plain text and truncated at a UTF-8
boundary to fit the host's 128-byte title limit. Event identifiers are hashed into
the protocol's bounded ID grammar.

Event IDs are deduplicated for ten minutes, each camera has a 45-second cooldown,
and each state map has at most 512 entries. Incoming JSON is limited to 16 KiB.
The motion parser ignores retained deliveries, updates, completed events, and malformed
messages. If `after.start_time` is present, starts older than ten minutes or more
than one minute in the future are ignored. Legacy publishers without timestamps
remain supported. Reconnect preserves the in-memory motion history; a fresh worker
process starts with a new history.

With a configured base, a fresh motion start emits one `http_live` frame for
`/api/<camera>?fps=2&height=360` beneath its path prefix. The camera occupies one
encoded URL segment: spaces, Unicode, and query/fragment characters cannot change
the origin or URL structure. Percent signs, separators, controls, and exact
`.`/`..` identities cannot request a preview; motion notification remains available.
The package's explicit `http_video.live_preview` grant fixes this query and a
15-second maximum duration. Worker frames cannot override either value.

The native host owns the preview and does not overlay a duplicate motion notification. It opens the
low-rate MJPEG stream in a separate incognito window, permits navigation only to
the exact scoped URL, denies application IPC and new windows, and closes it after
15 seconds or earlier on worker revocation. Preview admission expires after three
seconds if shared media-window capacity remains full. Previews use no clip cache
or download slot. These pages cannot receive bearer credentials or inherited HA
tokens. The worker itself never performs HTTP requests or supplies frontend code.

Completed `end` events do not open another delayed recording after a live preview.
Older installed Frigate 0.2 packages can continue requesting completed clips from
the host until their saved package pins are explicitly replaced.

The worker limits combined motion/media event output to 30 per minute before the host applies
its independent permission, rate, queue, and generation checks. The existing
two-frame output queue drops excess events while the host is slow, keeping MQTT
polling responsive. Handshake, configuration acknowledgement, and status frames
still wait for their writes to complete. Event delivery is best effort.
See the [notification contract](../../docs/plugin-worker-protocol.md#native-desktop-notifications-host-api-12)
for host queue limits and delivery semantics. Native submission does not prove
that the OS displayed a notification or that the user granted display permission.

Connection status is `Connecting`, `Connected`, or `Disconnected`. Connected
requires a matching successful SUBACK. Failed connections and rejected subscriptions
retry with bounded exponential delays from one to 30 seconds; every retry creates
a clean MQTT session and subscribes again. Shutdown and stdin EOF cancel retry,
network work, and blocked output waits promptly. No endpoint, raw broker payload,
or credential is included in status or error messages.

## Build and stage

This crate is a separate Cargo workspace with a checked-in lockfile. It is built
only for macOS, Linux, or Windows and is not a dependency of the application.
Its build script rejects Android, iOS, and the mobile build profile.

```bash
CARGO_BUILD_JOBS=2 cargo build --release --locked --manifest-path desktop-plugins/frigate/Cargo.toml
```

For example, after a native Apple Silicon build, prepare a new staging directory:

```bash
python3 scripts/plugins/prepare-frigate-package.py \
  --worker desktop-plugins/frigate/target/release/inverter-frigate-worker \
  --target aarch64-apple-darwin \
  --output /private/tmp/frigate-package-stage
```

Use the actual compilation target and binary path for other builds; Windows uses
`inverter-frigate-worker.exe`. The output parent must already exist and the staging
directory must be new. The helper rejects symlinks/reparse points, oversized files,
target/CPU mismatches, non-executable formats, and mobile or universal Mach-O files.
Cargo hardlinked inputs are copied to an independent staged file. Each
archive contains one thin target executable. Header validation does not establish
compiler provenance or compatibility with every system library on that platform.
The helper retains the identities of the selected file and parent directories,
then compares the opened file before and after its bounded read. These checks
detect replacement and observable modification; they are not a filesystem
transaction. Source validation finishes before the output directory is created.

Staging produces `manifest.json` and `payload/`. Pass them to the existing
[native package encoder](../../docs/plugin-packages.md#producing-an-archive):
configured downloads use its unsigned mode and an exact archive SHA-256 pin;
manual signed-file installation uses an externally supplied publisher key.
The staging helper itself neither signs nor installs.
No worker binary or `.idplugin` archive belongs in an Android/iOS application.

## Validation

Run worker checks separately from the host build to keep local machine load bounded:

```bash
cargo fmt --manifest-path desktop-plugins/frigate/Cargo.toml -- --check
CARGO_BUILD_JOBS=2 cargo clippy --locked --manifest-path desktop-plugins/frigate/Cargo.toml --all-targets -- -D warnings
CARGO_BUILD_JOBS=2 cargo test --locked --manifest-path desktop-plugins/frigate/Cargo.toml --all-targets -- --test-threads=2
cargo audit --file desktop-plugins/frigate/Cargo.lock
```

The independent worker lockfile receives its own advisory audit without borrowing
host-only exceptions. Hosted CI builds and tests the worker on Linux/macOS/Windows.
Parser tests cover malformed data, bounded state, timestamp/retained suppression,
independent start/clip deduplication, cooldown expiry, URL authority, version
negotiation, and output budgets. Subprocess tests drive real
stdio and TCP connections, including explicit topic/credentials, reconnect,
shutdown, live-start frames and end-event suppression, motion-only operation, absence of
worker HTTP requests, and TLS ClientHello/no-plaintext-fallback behavior. The latter does not
establish a complete certificate-trust/hostname handshake fixture.

A separate pair of host tests uses the actual compiled executable and a private loopback
Mosquitto instance. Build the worker and the desktop frontend first, then provide
explicit absolute fixture paths (substitute the paths on your machine):

```bash
INVERTER_FRIGATE_WORKER=/absolute/path/inverter-frigate-worker \
MOSQUITTO_BIN=/absolute/path/mosquitto \
CARGO_BUILD_JOBS=2 cargo test --locked --manifest-path src-tauri/Cargo.toml --lib \
  plugins::frigate_integration_tests::signed_frigate_package_real_mqtt_lifecycle \
  -- --exact --ignored --test-threads=1
```

Run the same command with the exact test name
`plugins::frigate_integration_tests::signed_frigate_package_real_mqtt_live_lifecycle`
for live previews. That test uses simulated native windows and an inaccessible
loopback HTTP address, proving preview admission does not download a clip first.
It covers start/end separation, URL prefixes, settings restart, disable, logout,
uninstall, and an independent core MQTT probe. Duration, queue limits, and
download failure scenarios have separate service tests; they are not all broker
acceptance scenarios.

Ordinary host unit tests do not build another crate or start an external broker.
Both ignored acceptance tests are explicitly required by CI; missing fixture paths,
startup failures, or a zero-test selection cannot count as a pass. Its temporary
signed archive, publisher policy, settings, and broker are isolated from production.
The original motion test submits notifications to a test collector, so it does not verify actual OS
notification presentation. Current results and remaining feature/device boundaries
are tracked in [TODO.md](../../TODO.md).

The live-preview worker tests exercise real process/MQTT request production.
Native MJPEG display and desktop window behavior use the separate, opt-in
[native media smoke harness](../../docs/native-plugin-media-smoke.md). A successful
worker or broker test alone does not establish decoding, native focus/stacking,
or physical-device proof. Record each result separately in TODO.
