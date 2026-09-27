# Native plugin media smoke test

`plugin-media-smoke` is an explicit desktop acceptance executable. It exercises
the production media service, opaque media route, Vue player, and native window
adapter with a private fixture grant. It is absent from normal builds and is
never selected by the production application, even when its feature is enabled.
Android and iOS cannot build this executable.

The harness creates temporary application state and uses a nonpersistent browser
profile. It does not initialize normal authentication, saved configuration,
keychain access, core MQTT, camera workers, or installed plugins. It initializes
an empty temporary package manager for the real bridge lifecycle. Its publisher
policy is empty; encrypted fixture preferences use a fresh disposable key. No production
publisher key, package installation, or new application-signing step is required.

## Run explicitly

Use a desktop graphical session and an actual decodable MP4, larger than 1 MiB,
lasting 8–40 seconds. Its duration must be at least the requested hold time plus
six seconds. The hold time is optional and defaults to zero; it accepts whole
seconds from zero through 30. The harness checks focus at each native reveal
boundary and must first focus its own anchor window. If
the desktop prevents that, the run reports an explicit focus-precondition failure.
Rerun with a new evidence file and bring the Native Media Smoke anchor to the
foreground during initialization, then let the automated checks finish. A later
user switch to another application is recorded as an unfocused anchor; it is not
reported as the preview stealing focus. Reveals must never focus the preview,
and a focused anchor must remain focused immediately across the show operation.

Serve only a disposable fixture directory on a loopback IP. For example, in a
separate terminal, with the clip at `/absolute/fixture-directory/prefix/clip.mp4`:

```bash
python3 -m http.server 8765 --bind 127.0.0.1 --directory /absolute/fixture-directory
```

The fixture URL must use plain HTTP and an explicit loopback IP. A `localhost`
hostname, credentials, query string, or fragment is rejected. The evidence path
must be an absolute path to a new file; its parent directory must already exist.

Build the desktop frontend from the repository root before running the example:

```bash
pnpm build
CARGO_BUILD_JOBS=2 cargo run --locked --manifest-path src-tauri/Cargo.toml \
  --example plugin-media-smoke --features native-media-smoke -- \
  --fixture-url http://127.0.0.1:8765/prefix/clip.mp4 \
  --evidence /absolute/new-evidence.json
```

To read the command's own usage without launching a window:

```bash
cargo run --locked --manifest-path src-tauri/Cargo.toml \
  --example plugin-media-smoke --features native-media-smoke -- --help
```

The explicit feature embeds the desktop assets, so rebuild them after player
changes. A stale desktop build is not evidence for changed frontend source.
Stop the fixture server after the test.

Add `--always-on-top true` to test enabled video window pinning. With the option
omitted or set to `false`, the preference remains absent from the isolated store
and must default off. Both clip and live-preview modes check the actual native
window level before and after reveal, alongside the existing focus checks.
The independent live-preview peer belongs to another plugin and must remain
unpinned even when the primary plugin's preference is enabled.

## What a pass establishes

The harness opens three players in sequence, including two simultaneously. It
requires decoded nonzero video dimensions, metadata, and playback time advancing
to at least 1.2 seconds. Its isolated `auth_status` holds bootstrap for 750 ms and
requires the native window to remain hidden before and after that wait. The
harness then observes the real media element while the native window is still
hidden, immediately before forwarding the production reveal command. Video must
have current decoded data, nonzero dimensions, a usable native canvas pattern,
and active playback; the observer
does not start playback or change the source. It checks native geometry, nonoverlapping windows within
the monitor work area, and preservation of the focused anchor window. It also exercises
bounded 1 MiB range responses, HEAD, and rejection of a different requesting
window.

The first player is closed through the production Vue Close button and its owned
IPC command. The second is destroyed after revoking its fixture lease. The last
must reach the actual video `ended` event and close automatically. Owned media
files must be removed and the bridge must exit successfully before the JSON can
report `passed: true`. Visible windows or a successful decode alone are not a
complete pass; retain the JSON and process result with the tested commit and
platform.

The signed-package, real Frigate worker, and Mosquitto tests are separate backend
acceptance. This executable does not prove package trust, MQTT behavior, OS
notification display, or playback on other operating systems. CI compiles the
opt-in example and runs `--help`; those checks do not launch its graphical test.
Current results and remaining acceptance work are recorded in [TODO.md](../TODO.md).

## Live preview mode

The same isolated harness supports the optional automatic preview path:

```text
plugin-media-smoke --live-fixture-url 'http://127.0.0.1:PORT/prefix/api/front?fps=2&height=360' --evidence /absolute/new-live-evidence.json
```

Serve a loopback MJPEG response with alternating visibly different frames and
`Access-Control-Allow-Origin: *` for the harness-only pixel probe. The isolated
observer sets `crossorigin=anonymous` when Vue creates the image, before its
production source is assigned. This permits reading the actual image's pixels
without opening a separate probe stream or changing the source or playback.
Ordinary camera viewing does not require this CORS response header. The
harness derives an explicit preview grant with a fifteen-second lifetime and no
bearer credential. It inspects decoded image dimensions and changing pixels by
native evaluation, checks window placement/focus, then verifies both timed expiry
and generation revocation. The local compact viewer receives only its owned source-read, close and drag IPC
and session-status bootstrap. Its image CSP permits only the configured origin;
remote document navigation and new windows are denied. The private URL is absent
from the route and public snapshots.
Preview tests are separate from downloaded H.264/image tests and use no media
cache file. Run this mode explicitly to establish current graphical acceptance;
merely building it or passing the pure media tests is not playback evidence.

### Delayed first-frame and failure fixtures

`scripts/native-media-fixture.py` serves only on `127.0.0.1` and requires `ffmpeg`
to generate disposable red/blue JPEG frames. It sends multipart headers
and JPEG metadata immediately, withholds the first frame's compressed body for
two seconds, and keeps the response open.
This distinguishes a decoded first frame from early JPEG metadata or the end of
the streaming HTTP response.
The generated URLs and chosen port are written to the requested new JSON file:

```bash
python3 scripts/native-media-fixture.py --ready-file /absolute/new-fixture.json
```

Pass its `delayed` URL to the harness with `--minimum-loading-ms 1500`. A pass
requires the window to remain hidden during bootstrap and the delayed source,
then prove nonzero decoded image dimensions and an opaque pixel from the actual
image before native reveal. The receipt
records `imageComplete` and `imageLoadEvents` before showing the window, followed
by the existing changing-pixel, focus, Close, expiry and revocation checks.
It also submits a second event for the same camera while the first window is
still loading and requires exactly one native media window through reveal. A
different camera still opens its own peer window; the original camera can open
again after its previous window expires.

The `static` URL deliberately splits an ordinary JPEG in the same way. Use it
with `--expected-live-outcome still --minimum-loading-ms 1500` to verify that
JPEG dimensions alone cannot reveal a window before its compressed image body.
This scenario checks the actual decoded pixel and lifetime without requiring
the stationary image to change colors.

The same server provides two bounded failure URLs. Pass `error` with
`--expected-live-outcome error`, or `stall` with
`--expected-live-outcome timeout --minimum-loading-ms 9000`. These runs require
the terminal error to appear only after hidden initialization, retain controls
without taking focus, and close under the original fifteen-second preview lease.
Pass `stall` with `--expected-live-outcome revoke-loading --minimum-loading-ms 1000`
to revoke a still-hidden, undecoded preview and require complete native cleanup.
Use a new evidence file for each invocation. `--clip /absolute/fixture.mp4` also
serves an existing disposable H.264 fixture for the ordinary three-player test.
Stop the fixture server with Ctrl-C afterward; it removes its temporary JPEGs.

Hidden playback is a native acceptance requirement: browser autoplay policies
may throttle invisible media. The harness requires actual WKWebView decoding and
does not make readiness depend on animation or video-frame callbacks that may
stop in hidden windows. A failed or timed-out hidden decode is a failed run, not
evidence that source compilation or browser unit tests establish native behavior.
Normal acceptance installs its observer at document initialization and does not
evaluate JavaScript in a hidden viewer before the viewer requests reveal.
`--diagnose-hidden true` adds hidden DOM probes for troubleshooting; those probes
may wake a suspended webview, so diagnostic runs are not readiness acceptance.
The receipt records both the video decoder and presentation counters when the
browser exposes them. A hidden video's presentation counter can still be zero
with a usable current frame; successful visible playback and progress are checked
separately after reveal.

## Recorded macOS acceptance

### Shared local viewer checkpoint

The shared frameless viewer passed both graphical modes using the same executable
(SHA-256 `db663cc4c75f4346adae7d91042e6702597fc276f3c8638c5966dddd00ff4657`). Evidence
files are `inverter-shared-live-preview-zub9nois-retry.json` and
`inverter-shared-live-h264-zub9nois.json`; both report `passed: true`, native exit
code 0, no failure and successful private-profile cleanup.

The local live viewer decoded changing 640x360 MJPEG frames and exposed the shared
toolbar and Close button. Two independent preview leases produced exactly stacked,
unfocused 660x372 physical-pixel windows within the monitor work area while the
anchor kept focus. The real Vue Close button destroyed the peer preview. The
first preview was absent after 16.926 seconds including admission and native
close processing under its fifteen-second grant; the other scenario closed on
generation revocation. Pure media-service tests separately verify grant lifetime
and retain the slot until native close acknowledgment.

The same executable also passed the three-player H.264 scenario with the
7,459,136-byte, 640x360, sixteen-second fixture: decoding and progress, actual
`ended`, bounded range and requesting-window ownership, Vue Close, revocation,
stacking/focus and owned-file cleanup. The disposable loopback server was stopped.
An initial MJPEG run lost anchor focus while the preview remained unfocused;
the unchanged binary and criteria passed on retry. That failed evidence remains
available as `inverter-shared-live-preview-zub9nois.json`.

These isolated macOS results do not establish signed-release installation,
real household camera or OS notification behavior, or Linux/Windows graphical
acceptance.

### Initial extraction checkpoint

The extraction checkpoint passed both graphical modes with the same isolated
executable (SHA-256
`55ea779afc86bd961f8ad56022accafc122c082b9a326981c6d209c4368f84d6`). Local evidence
files are `inverter-extraction-preview-w0ljgfb3-ready.json` and
`inverter-extraction-h264-w0ljgfb3-ready.json`; both report `passed: true`, no
failure, native exit code 0, and successful private-profile removal.

The MJPEG fixture produced distinct decoded 640x360 frames in visible, unfocused
windows without taking the anchor's focus. The first window expired after
15.055 seconds; generation revocation destroyed the second. The harness waits
through Wry's pre-navigation callback cancellation within its existing five-second
decode deadline; it still requires decoded dimensions and changing pixel values.

The H.264 fixture was 7,459,136 bytes, 640x360 pixels and 16 seconds long. All three
players decoded and advanced; the final player reached its actual `ended` event.
Two simultaneous 660x372 physical-pixel windows remained inside the monitor work
area, visible, unfocused and nonoverlapping. The focused anchor, bounded 1 MiB
`206` response, wrong-window rejection, real Vue Close button, revocation,
automatic end close and owned-file cleanup all passed. The disposable loopback
fixture server was stopped afterward.

This is source-checkpoint graphical evidence on macOS, separate from a signed
release installation or real camera/OS notification acceptance. Linux and Windows
graphical acceptance remains open; their build/help checks do not prove playback.

### Earlier clip checkpoint

The local `inverter-frigate-clips-native-smoke-3.json` reports `passed: true`, no
failure and native exit code 0. The actual H.264 fixture was 4,727,159 bytes,
640x360 pixels and 24 seconds long. All three players reported decoded dimensions
and playback progress; the final one reached its actual `ended` event at 24 seconds.

Two simultaneous native windows were 330x186, at y=30 and y=216, both visible,
unfocused and nonoverlapping. The anchor retained focus. The backend returned a
bounded 1 MiB `206` response and denied a different requesting window. The real
Vue Close control, lease revocation before native destruction, and automatic
video-end close all succeeded. Owned media files and the temporary profile were
removed before successful exit.

This acceptance exposed and verified fixes for two production defects: shared
camera placement now uses the monitor work area instead of the full display, and
macOS plugin windows suppress key-window eligibility only while being shown, then
restore normal user interaction. Linux and Windows graphical playback/window
acceptance remains pending. Their CI compile/help checks are not native playback
evidence, and this fixture does not establish OS notification display.
