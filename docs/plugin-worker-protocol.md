# Desktop plugin worker protocol

This document describes the first worker contract for optional desktop features.
The current host API is **1.9.0**, independently of the application version. The
wire protocol and package manifest each start at schema version **1**. Android
and iOS do not compile the worker host or include plugin UI contributions.

The contract supplies bounded messages and declarative dashboard contributions.
It does not make a native worker a sandbox, establish publisher trust, verify a
package signature, or install a package. Native workers have the operating
system access of the account that starts them. Permission declarations are not
an operating system sandbox.

## Transport and limits

A worker is an executable launched directly by the host, without a shell. Its
standard input receives host messages and its standard output carries worker
messages. Each message is a UTF-8 JSON object on one line, terminated by LF;
CRLF input is accepted. JSON strings may contain escaped line breaks. Pretty
printed JSON and several messages in one frame are rejected. Standard error is
reserved for diagnostic output and is never parsed as protocol data.

Each complete frame, including its terminating newline, is at most **65,536
bytes**. The host must bound reads before accumulating a complete line, not only
validate a line after reading it. Empty, malformed, oversized, unknown message
types and unknown fields are invalid. Labels are plain text; the contract has
no HTML, JavaScript, component source, navigation URL, Tauri command, or core
MQTT publish contribution.

The additional limits are:

- At most 128 dashboard contributions in one replacement snapshot, with unique
  IDs within that worker.
- Identifiers use ASCII letters, digits, `_`, `.`, `:`, and `-`, start with an
  ASCII letter or digit, and occupy at most 128 bytes.
- Plugin IDs use dot-separated lowercase components, each starting with a
  letter; subsequent characters can include digits and hyphens. A component
  cannot end in a hyphen. A plugin ID occupies at most 128 bytes.
- Titles and action labels occupy at most 128 bytes. Units occupy at most 32
  bytes. These strings cannot be blank or contain control characters.
- Text and status values occupy at most 4,096 bytes. Text can contain line breaks
  and tabs; status values cannot contain control characters.
- Action parameters must be JSON objects occupying at most 4,096 serialized
  bytes. Action results and worker event data occupy at most 16,384 serialized
  bytes. JSON data has at most eight levels below its root and 512 values; object
  keys occupy at most 128 bytes and cannot contain control characters.
- Action deadlines are relative milliseconds in the range 1–60,000. The host
  forwards the remaining budget immediately before writing, subtracting time
  spent in its queues and dropping requests with less than one millisecond left.
  Its original absolute deadline and cancellation remain authoritative, including
  during pipe transmission and when a worker does not cooperate.
- Error codes follow the identifier rules. Error messages occupy at most 1,024
  bytes.

Limits measured in bytes refer to UTF-8 or compact serialized JSON, as
applicable. A set of individually valid contributions must also fit the frame
limit.

## Handshake

The host starts with the expected identity and the API version it selected:

```json
{
  "type": "hello",
  "protocol_version": 1,
  "host_api_version": "1.9.0",
  "plugin_id": "org.example.weather"
}
```

The worker must acknowledge that exact identity and both selected versions
before sending data:

```json
{
  "type": "ready",
  "protocol_version": 1,
  "host_api_version": "1.9.0",
  "plugin_id": "org.example.weather"
}
```

A missing or mismatched acknowledgment fails startup. The worker's complete
supported host API range belongs in its manifest; `ready.host_api_version`
acknowledges the negotiated version and is not a version range. Identity comes
from the host's selected worker, not from an arbitrary frame claiming to be a
different plugin.

## Startup configuration (host API 1.1)

After a valid `ready`, a verified installed package declaring
`plugin_configuration` receives exactly one configuration frame on stdin:

```json
{
  "type": "configuration",
  "configuration": {
    "revision": "abceb85b-59bd-44d8-8135-e7f2ef873122",
    "values": { "endpoint": "https://example.invalid", "enabled": true },
    "secrets": { "token": "example-placeholder" }
  }
}
```

The worker must acknowledge the exact revision before contributing dashboard data
or accepting actions or sending notifications:

```json
{
  "type": "configuration_ready",
  "revision": "abceb85b-59bd-44d8-8135-e7f2ef873122"
}
```

The worker remains `starting` until both acknowledgments succeed. The existing
startup deadline covers both steps. Early contributions, missing/mismatched or
duplicate acknowledgments fail that generation. Packages without configuration
permission receive no configuration frame and complete startup after `ready`.
Workers must acknowledge the host API selected in `hello`; hard-coded 1.0 replies
are incompatible with a 1.9 host. The wire protocol and manifest remain version 1.
A configured worker should declare an API requirement such as `^1.1`.

The complete configuration object is bounded to 32 KiB, in addition to the 64 KiB
frame limit. Values and secrets are scoped to the verified plugin ID; fields not
present in its current schema are retained on disk but withheld from the worker.
The native service validates required values before launching. Missing required
setup therefore requires installing disabled, saving settings, then enabling.
With host API 1.5, a schema may explicitly use `omitEmpty` for optional public
default-empty strings. Only those empty values are withheld from startup; the
editor retains their effective empty default and saves without redundant keys.
See the [schema contract](plugin-settings.md).

Configuration is sent through the owned worker's startup pipe only. It is absent
from command-line arguments, inherited environment, runtime snapshots, UI events,
and host debug formatting. The pipe writer checks the originating authentication
epoch, stop signal, and startup deadline before delivery; an interrupted partial
frame fails that worker generation. A trusted native worker necessarily receives
its own secrets and must not echo them into diagnostics, contributions, or action
results. This is not process sandboxing or a guarantee against a malicious worker.

Saving settings restarts an enabled worker with the new configuration; disabled
workers remain stopped. There is no hot-reload or request-time secret API in this
checkpoint. See [plugin settings](plugin-settings.md) for the schema, encryption,
retention, revision, and failure contracts.

## Displayed action authority (host API 1.4)

Native dashboard snapshots include `instance_id`, an opaque identifier for the
actual worker process, or null when no live instance is available. A new process
always gets a fresh identity, including reinstall when generation counters repeat.
The authenticated `plugin_action` application command requires that displayed
identity as `instanceId`, plus the action ID and exact advertised preset params.
The host rejects stale instances before enqueueing, and continues checking the
session, generation, advertisement and deadline before writing the worker pipe.

The frontend also rejects changed clicked descriptors instead of substituting new
parameters. Pending and uncertain-result feedback belongs to the actual instance
and operation; changing a label or temporarily withdrawing an action does not
unlock duplicate dispatch. Snapshot requests are coalesced so live updates do not
starve visible state. Authentication changes and instance removal revoke old UI
work. None of this identity data is a publisher key or permission to bypass auth.

This extends the native dashboard boundary, not the worker wire schema. Existing
worker Action/Cancel/ActionResult/ActionError messages stay at protocol version 1.
HA 0.2–0.5 packages with fixed service actions require `^1.4`. HA 0.6 and 0.7 require
`^1.5` for bounded numeric inputs as well. HA 0.7's optional read-only sensor
discovery adds no host API, wire schema or contribution kind. HA 0.8 requires
`^1.6` for explicit state/control grouping. HA 0.9 retains that range and projects
selected weather state into existing bounded text contributions without new
authority, wire messages or contribution kinds. HA 0.10 keeps `^1.6` and combines
explicitly assigned dishwasher state/runtime readings in the running entity's
existing text/status contribution. Its optional role settings grant reads only;
the projection does not change action references or authority. HA 0.11 adds
optional washer/dryer remaining-time profiles in existing text/status slots,
retaining `^1.6` and the same authority, wire schema and contribution kinds.
Existing workers with compatible
`^1.3`, `^1.4`, `^1.5` or `^1.6` ranges still negotiate the selected 1.9 version; Frigate
retains its existing API range and flat contributions.

## Larger selected installations (host API 1.7)

The host accepts up to 128 contributions while retaining the 64 KiB complete-frame
limit and existing action, configuration, queue, and rate limits. HA 0.12 requires
`^1.7` and supports 64 distinct selected reads, 16 binary targets, and 63 controls.
One connection status plus 64 entity states plus 63 controls fills the snapshot.
Counts do not override the serialized byte limit: the worker bounds its display
strings, including JSON escaping, before publishing a complete replacement.
This capacity does not itself migrate legacy settings or activate a second UI.

## Compact presentation and settings choices (host API 1.8)

A Contributions frame may include `presentation`, an optional array of at most
96 host-owned descriptors. The host validates items and presentation together
before replacing either snapshot; the complete frame remains limited to 64 KiB.
Omitting the array supplies no compact views. This is declarative data, never
frontend source or arbitrary component loading.

- `control`: `id`, `surface` (`header` or `home`), `order`, `title`, `icon`,
  `state` (`on`, `off`, `unavailable`), and optional `action` contribution ID.
  The state controls appearance; only a valid current action reference enables
  interaction. This preserves actionable unknown button/scene states. Workers
  withdraw the reference when disconnected or when the operation is unavailable.
- `group`: `id`, `surface: "sidebar"`, `order`, `title`, `icon`, `collapsed`,
  and up to 64 `rows`. Each row supplies `id`, `title`, a read-only contribution
  reference in `value`, up to four `{id, label}` action references, and optional
  `input` referencing a numeric contribution.
- `summary`: sidebar identity/order/title/icon, `visible`, `active`, `text`, and
  up to four action references. Workers compute appliance visibility and status.
- `weather`: sidebar identity/order/title, `condition`, string `temperature`,
  `unit`, and up to five forecast entries with `datetime`, `condition`, string
  `temperature`, and optional string `templow`.
- `connection`: `id`, `title`, and explicit boolean `connected`.

Descriptor and row IDs are unique within their scope, bounded to 64 bytes with
letters, digits, periods, underscores, and hyphens. Icons are `home`, `plug`,
`light`, `washer`, `dryer`, `dishwasher`, `thermometer`, `gauge`, `blinds`, `play`,
or `cloud`. All references must resolve to the expected contribution kind in the
same atomic frame. Repeating one authorized action under distinct labels is
allowed; a presentation reference is not a native `action_id` and grants nothing.
Dashboard order mixes core controls and explicit plugin positions without
removing hidden legacy records. The generic host renders these views in existing
compact sections, never an appended flat diagnostic panel.

Workers may publish a read-only `event` named `settings_choices` with data:

```json
{
  "source": "entities",
  "revision": "catalog-1",
  "offset": 0,
  "complete": true,
  "options": [{ "value": "light.kitchen", "label": "Kitchen" }]
}
```

Each page contains at most 64 choices; up to eight sources and 4,096 unique values
per source are retained. Values and labels occupy at most 128 bytes. Sequential
pages must retain the revision and exact offset. Publication replaces a source
only on its complete page; partial pages are not exposed. Worker/session removal
revokes the catalog. The authenticated settings window reads it through
`get_plugin_settings_choices({pluginId})`, receiving `{revision, sources}`.
Catalog entries are suggestions, never observed state, permission, or action
advertisements. Manifest editor metadata and bounded structured JSON strings are
described in [plugin settings](plugin-settings.md).

## Dashboard contributions

A `contributions` message replaces that worker's entire current snapshot. An
empty `items` array removes all its contributions. The host associates the
snapshot with the selected worker process; items cannot choose another
plugin's namespace.

```json
{
  "type": "contributions",
  "items": [
    { "kind": "text", "id": "description", "title": "Conditions", "text": "Clear skies" },
    { "kind": "metric", "id": "temperature", "title": "Outside", "value": 21.5, "unit": "°C" },
    {
      "kind": "status",
      "id": "connection",
      "title": "Connection",
      "value": "Connected",
      "tone": "success"
    },
    {
      "kind": "action",
      "id": "refresh-card",
      "title": "Weather",
      "action_id": "refresh",
      "label": "Refresh",
      "params": { "force": true }
    }
  ]
}
```

Five contribution kinds are supported:

- `text`: `id`, `title`, `text`.
- `metric`: `id`, `title`, finite numeric `value`, and optional `unit` (omitted or
  `null` when unused).
- `status`: `id`, `title`, `value`, and `tone`. Tone is one of `neutral`,
  `success`, `warning`, or `error`.
- `action`: `id`, `title`, `action_id`, `label`, and object-valued `params`.
- `number_input` (host API 1.5): `id`, `title`, `action_id`, `label`,
  `input_revision`, integer `value_scaled`, `min_scaled`, `max_scaled`,
  `step_scaled`, integer `decimal_places`, and optional `unit` (omitted or null).

The UI renders text using text bindings. A string such as `<b>example</b>` is
literal text, not markup. Metric formatting and visual appearance belong to
the host. A worker supplies data, not application code.

## Grouped state and controls (host API 1.6)

An `action` or `number_input` may include `state_id`, an optional contribution
ID referring to a `text`, `metric` or `status` item in the same replacement
snapshot. Omitted or null means a standalone control. Read-only items cannot
declare this field. The host validates the reference even when its target follows
the control in the array; blank, malformed, dangling, self and control-to-control
references fail the snapshot. A reference cannot name another worker's item.

For example, a cover status with `id: "shade-state"` and explicitly authorized
Open, Close and position controls with `state_id: "shade-state"` appear together
under the status title. The renderer retains state-card order and the original
order of controls within each card. Friendly names and ID prefixes never establish
ownership. Plain text, metric and status updates can replace the anchor's kind
without changing its identity. Unreferenced controls retain their flat layout.

References are presentation only. Each control stays in the flat snapshot and
consumes its existing contribution slot; the 128-item and 64-KiB limits still apply.
There are no nested groups, executable views or additional permissions. Referencing
a read-only card never grants that card an action, and selecting or discovering
a state never creates controls. Actions retain exact parameters and instance
binding; numeric inputs retain their revision and constraints. Changing a group's
title or layout cannot reset pending/uncertain operation feedback or widen a grant.
An explicit move between state cards may reset an unsent numeric draft to the
current observed value; it never submits that draft. A stable anchor preserves
the editor through ordinary state/title updates.

Workers emitting `state_id` must require host API `^1.6` or a compatible range
excluding older hosts. Older workers can omit the field and retain their existing
flat contributions while acknowledging the exact selected host version.

## Bounded numeric input authority (host API 1.5)

A numeric input advertises a decimal grid using integer coefficients, avoiding
binary floating-point rounding across JSON, Rust and JavaScript:

```json
{
  "kind": "number_input",
  "id": "temperature-input",
  "title": "Temperature",
  "action_id": "set-temperature",
  "label": "Set temperature",
  "input_revision": "limits-7",
  "value_scaled": -3,
  "min_scaled": -5,
  "max_scaled": 5,
  "step_scaled": 1,
  "decimal_places": 1,
  "unit": "°C"
}
```

Here `-3` represents `-0.3`, and the step is `0.1`. Precision is 0–6 decimal
places; each coefficient's absolute value is at most 10^15. The minimum cannot
exceed the maximum, the step must be positive, and the observed value must lie
within the bounds and on the grid anchored at the minimum. Titles, labels, units
and revision identifiers follow the existing plain-text and token limits.
Numeric action IDs must be unique across all actionable contributions; existing
fixed-action aliases remain supported. Numeric declarations contain no `params`.

Submission uses the existing instance-bound action command with exactly
`{"input_revision":"limits-7","value_scaled":-2}`. Fractional coefficients,
extra keys, mismatched revisions, out-of-range values and off-grid values are
rejected. Fixed actions still require exact advertised preset parameters.

The host checks the complete numeric grant at enqueue, supervisor dispatch and
immediately before the first pipe byte, including after a pending write wakes.
Withdrawal, revision or constraint changes revoke queued grants; restoring the
same text revision cannot revive them. Observed-value, title and label updates
alone retain authority. Once transmission begins, existing frame-completion,
deadline, authentication and cancellation rules remain authoritative.

The desktop editor separates observed state from a draft and requires explicit
Apply. Live value updates preserve an edited draft; changes to bounds, step,
precision, unit or revision require review before further submission. Pending
and uncertain-result feedback is keyed by plugin, instance and action ID, so
changed values, revisions or temporary withdrawal cannot unlock a duplicate
request. These contributions add no mobile UI, permission or core command route.

## Native desktop notifications (host API 1.2)

A verified package declaring `desktop_notifications` may send a notification
only after its startup handshake and required configuration acknowledgment:

```json
{
  "type": "notification",
  "id": "motion-13d7bbf9",
  "title": "Frigate Garage camera motion detected",
  "body": "Motion started"
}
```

The ID follows the existing 128-byte identifier grammar. Title and body must be
nonblank plain text without control characters, bounded to 128 and 1,024 UTF-8
bytes respectively. Unknown fields, malformed content, missing permission, and
messages before readiness fail that worker generation. Host API 1.8 optionally
accepts `live_view_id`, an exact key in a startup-derived private destination map.
It is not a URL, Tauri command, or arbitrary notification option.
Ordinary notification packages require at least host API 1.2; packages using
`live_view_id` require host API 1.8.

Delivery is best effort. Each registered worker has at most 16 queued notifications,
30 accepted notifications per minute, and 512 recent IDs remembered for ten minutes.
The host accepts at most 120 per minute globally; eight registered workers bound
the aggregate queue to 128. Valid duplicate, rate-limited, or overflowing messages
are dropped without failing the worker. The existing total frame-rate limit still
applies. Pending notifications expire after 30 seconds. Deduplication survives
automatic worker restarts; process/session replacement cannot revive a queued item.

The native dispatcher checks the original session epoch, running process generation,
stop signal, reaped state, and delivery age while holding the authority guard. The
application keeps one dedicated dispatcher and one pending signal, preventing
overlapping delivery tasks. Media admission precedes notification dispatch, and
the dispatcher yields between submissions whenever media is pending. An already
started native submission still holds authority until it returns. Each started
dispatch checks live authentication before entering the authority guard. Actual
native submission happens inside the guard; the callback never defers unchecked
delivery to another application task. Linux notification-service waits are bounded
to two seconds; the macOS backend has its own two-second confirmation wait, whose
timeout can still return success. Both remain best-effort OS submission. Notification text is absent from manager
snapshots, webview events, and host logs. Freedesktop body markup is escaped so
worker text remains literal. Disable, uninstall, logout, and shutdown discard
pending work. Notifications already submitted to the operating system may remain
visible; OS permission, presentation timing, and notification-center retention are
outside the worker lifecycle contract. Native submission failures are not retried.

No plugin notification IPC or worker dependency is added to Android/iOS. Core
inverter notifications continue using their existing platform integration.

## Owned HTTP media (video from host API 1.3, snapshots from 1.7)

A package may declare `http_video` and `plugin_configuration`, with a matching
manifest declaration referencing one non-secret configuration field:

```json
{ "http_video": { "base_url_setting": "frigate_base_url" } }
```

The host derives an immutable grant from the same verified startup configuration
sent to that worker. An absent or blank base disables video requests while other
configured features can continue. A configured base must use HTTP(S), with no
userinfo, query, fragment, traversal, ambiguous encoded separators, or control
characters. Explicit ports and reverse-proxy prefixes are preserved. Invalid
candidate settings are rejected before an existing worker is stopped.

After configuration acknowledgment, the worker can submit:

```json
{
  "type": "http_video",
  "id": "frigate-clip-event-hash",
  "url": "http://frigate.example:5000/proxy/api/events/event-id/clip.mp4",
  "title": "Frigate Front camera motion detected"
}
```

IDs follow the bounded token grammar, titles use the notification plain-text
128-byte limit, and URLs are bounded to 2,048 bytes. The request must remain in
the configured origin and base path. The worker supplies no filesystem path,
window route, request headers, cookies, HA token, or core settings reference.
Downloads do not follow redirects or inherit core HA credentials. Host API 1.8
permits a manifest's `bearer_token_setting` to name a dedicated secret setting;
the host attaches that startup-bound token only within the granted origin/path.
Workers cannot supply headers or select a different credential per request.
Optional `allow_query` permits request query strings only when the manifest grants
it, and `cooldown_seconds` scopes the configured media policy. Request
`cooldown_id` separates equal camera titles without enlarging global limits.

Host API 1.7 also accepts `"media_kind": "jpeg"`, `"png"`, or `"webp"` on
this frame. The historical `http_video` message and permission names remain for
package compatibility. An omitted kind is video and retains its original wire
representation. Snapshot workers must require `^1.7`; no image is inferred from
an extension, response header, or remote title. The host checks the declared
format against file signature bytes, uses its corresponding image MIME type,
and rejects mismatches. SVG, HTML, and arbitrary content types are not supported.
Snapshots have an 8 MiB limit. The owned player closes a successfully displayed
image after 12 seconds, with the same generation revocation as video.

Host API 1.8 also supports automatic motion previews through `http_live`.
The verified `http_video` declaration must include
`live_preview: {query: "fps=2&height=360", max_duration_seconds: 15}`.
This grant cannot carry a bearer credential. A preview URL must stay within the
granted origin/path and match its exact query. The native adapter opens the local
compact viewer in an incognito `plugin-preview-<UUID>` window. Only that owning
viewer can obtain its active private image URL, close itself, or drag its window.
The image policy allows the configured camera origin; remote document navigation,
new windows, and general application IPC are denied. The native host closes the
window at the grant's lifetime or generation removal. Automatic previews do not
synthesize desktop notifications, so a motion toast cannot cover the video.

Mapped cameras instead emit `live_view` with `id`, `title`, and `live_view_id`,
without a URL. The verified `live_view` declaration must explicitly include
`preview_duration_seconds` (1–30; Kerberos uses 15). Native code resolves only the
exact camera ID from the generation's encrypted `urls_setting` map. Unmapped IDs
are omitted; declarations without a duration retain notification-click capability
without gaining automatic preview authority. The derived preview grant permits
only that exact configured URL and cannot authorize a download. Admission is
independent of desktop notification permission, uses a 15-second per-camera
cooldown, and retains the common queue, rate, episode-ID, and generation limits.
Kerberos emits a preview for a mapped camera, or an ordinary motion notification
when that camera has no configured preview. It never emits both for one event.

Previews share the eight-window limit but reserve no download transfer or disk
bytes. Waiting for capacity expires after three seconds, preventing delayed motion
replay. Timed expiry revokes display authority before requesting close, and the
window slot remains owned until native destruction is acknowledged. Downloaded
clips and twelve-second snapshots keep their existing behavior. Frigate 0.3 emits
fresh motion previews instead of delayed end-event clips; earlier clip workers
remain compatible with the host download path.

Automatic previews also retain a camera identity through admission and native
window ownership. Mapped previews use the exact `live_view_id`; HTTP previews use
the validated stream origin and path. A download carrying `cooldown_id` uses that
explicit identity. Keys are scoped to the plugin, independently of worker
generation and display title. A repeated camera request is dropped while its
previous request is queued, loading, visible, or awaiting native destruction;
it is never replayed after the original window closes. Different cameras remain
independent, including cameras with identical display titles. Legacy downloads
without `cooldown_id` retain their existing event/title cooldown because their
event-specific URLs do not identify a camera. Separate providers require an
explicit shared mapping to identify the same physical camera; equal titles do
not establish that identity.

Each worker has four pending requests, thirty admissions per minute, a ten-minute
512-ID history, and a default 45-second title cooldown (mapped previews use the camera policy above). Pending requests expire after
thirty seconds. The service independently bounds its queue to eight, transfers
to two, and active/download-reserved windows to eight. It reserves up to 256 MiB
per transfer within a 512 MiB media budget; completed smaller files release the
unused reservation. Valid excess traffic is dropped without failing the worker.
With `desktop_notifications`, admission also queues the existing bounded native
clip-available notification. HTTP or display failure can still follow admission.

The direct download policy retains eight attempts with 1/2/3/4/5/5/5-second retry
delays, a 15-second connect timeout, 60-second idle-read timeout, ten-minute overall
deadline, and 256 MiB per-file limit. Retryable responses include 400, 404, 408,
425, 429, and 5xx; empty or interrupted bodies can retry. Oversized responses fail
immediately. Playback begins only after the complete download succeeds.
Already-started disk operations are awaited before file cleanup, including after
cancellation; the network/retry deadline is not a forced interruption of filesystem I/O.

Each launch receives a fresh native instance identity and cancellation lease;
plugin ID, session epoch, and displayed generation alone are insufficient because
generation counters can repeat after registration. Disable, replacement, settings
restart, crash/restart, logout/expiry, uninstall, and shutdown revoke the original
lease. Long HTTP/disk operations never hold host authority locks. Cancellation
immediately denies media access and schedules owned-window destruction and file
cleanup. Native visibility is asynchronous, so already submitted display work can
briefly outlive its check; it cannot restore the revoked media authority.

Files live in a private sibling `desktop-plugin-media` directory under the package
manager's lifetime lease. The host creates hidden 330x186 windows, verifies their
ownership again before showing them, and waits for actual destruction acknowledgments
before releasing window slots. Failed native cleanup retains ownership and makes
shutdown fail visibly; a subsequent quit can retry. Closing one window does not
retire sibling media or reconnect core telemetry.

Camera windows remain hidden during authentication and media startup. The local
viewer uses its zero-argument owned reveal command only after an image is
decodable or video playback has started with current frame data. This also
supports the first complete part of an ongoing MJPEG response. A terminal media
error is rendered before revealing the window. Media startup has a ten-second
timeout; an unresponsive hidden native bootstrap is closed after thirty seconds.
These limits do not extend the preview grant's original lifetime. Positioning is
resolved when the window becomes visible, and reveal does not take keyboard
focus or move an already-visible window.
On macOS 14 and newer, camera webviews disable WebKit's inactive suspension so
authentication, decoding, and the startup timeout can finish while hidden. The
native lifetime limits remain in force; this policy is scoped to camera windows.

The player receives an opaque UUID through the `plugin-media` scheme. Requests are
bound to the exact requesting webview label and original running instance, with
live authentication checks before and after disk reads. No global temporary asset
scope is added. GET/HEAD support standard single byte ranges, at most 1 MiB per
range and four owned response buffers. Full GET for larger videos is rejected;
images allow a bounded full GET up to their 8 MiB limit. No image request bypasses
the original window/instance ownership checks. For video,
native playback acceptance must establish range behavior on each supported webview.
Core window-targeting permissions are absent: close and drag use commands that
operate only on their native invoking window. Android/iOS include none of these
commands, routes, windows, workers, or media services.

The [native media smoke harness](native-plugin-media-smoke.md) exercises actual
desktop webview playback separately from the signed-worker/MQTT and HTTP-service
fixtures. It is an explicit feature-only example, not a production startup mode.

## Actions, cancellation, and results

Actions travel to the worker that contributed the selected action. They do not
invoke a Tauri command or publish a core MQTT message. The host assigns a
correlation ID and bounds pending requests and execution time:

```json
{
  "type": "action",
  "request_id": "request-42",
  "action_id": "refresh",
  "params": { "force": true },
  "deadline_ms": 5000
}
```

The worker answers with exactly one result or error using that ID:

```json
{ "type": "action_result", "request_id": "request-42", "value": { "updated": true } }
```

```json
{
  "type": "action_error",
  "request_id": "request-42",
  "code": "unavailable",
  "message": "The weather service did not respond."
}
```

The wire validator checks syntax, size, and identifiers. The host supervisor
owns correlation, pending request limits, deadlines, cancellation, and process
lifecycle. The host sends cancellation when it stops waiting:

```json
{ "type": "cancel", "request_id": "request-42" }
```

Cancellation is cooperative. It does not by itself prove that external work
was undone. Late results cannot complete a different request or resurrect an
expired request. Request state is isolated between worker process generations.

## Worker events and shutdown

A worker can send bounded data scoped to its own identity:

```json
{ "type": "event", "name": "refresh-complete", "data": { "items": 3 } }
```

`name` is not a Tauri event name. A host must not forward this string as an
arbitrary application event, interpret its data as a host command, or grant
access to core control transports. This protocol reserves the data envelope;
it does not introduce a general host capability broker.

The host requests cooperative shutdown with:

```json
{ "type": "shutdown" }
```

Workers should release their resources and exit. The host enforces its shutdown
budget and reaps the process when cooperation fails. Startup, cancellation, and
shutdown timeout values are supervisor policy, not worker-selected authority.

## Manifest metadata

The manifest contract defines package identity and compatibility:

```json
{
  "schema_version": 1,
  "plugin_id": "org.example.weather",
  "version": "0.1.0",
  "host_api": ">=1.0.0, <2.0.0",
  "target": "aarch64-apple-darwin",
  "entrypoint": "bin/weather-worker",
  "config_schema": {
    "type": "object",
    "properties": {
      "location": { "type": "string" }
    }
  },
  "permissions": ["dashboard_contributions", "network_http"],
  "inventory": [
    {
      "path": "bin/weather-worker",
      "size": 1024,
      "sha256": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
    }
  ],
  "signature": null
}
```

The example digest and file size are illustrative metadata, not a usable
package. A native development launcher can select a fixture executable through
trusted `WorkerSpec`. Application package selection instead goes through the
[verified preview and installation pipeline](plugin-packages.md); only that native
pipeline derives a worker executable from a verified installed package. Manifest
parsing by itself neither loads a package nor enforces its permission declarations.

Manifest parsing rejects unknown fields and permissions. The version
must be semantic version syntax; the host API requirement must match the
host's current API. Supported targets are:

- `aarch64-apple-darwin`, `x86_64-apple-darwin`;
- `aarch64-unknown-linux-gnu`, `x86_64-unknown-linux-gnu`;
- `aarch64-pc-windows-msvc`, `x86_64-pc-windows-msvc`.

`validate_for_target` additionally requires an exact match to the selected
host target. Mobile targets are invalid. A package identifies one target;
multi-target publishing uses separate package artifacts.

The manifest occupies at most 65,536 bytes. Its inventory contains 1–128 files
with at most 512 MiB of declared total content. Paths are portable relative
paths using `/`; absolute paths, traversal, empty components, backslashes,
drive prefixes, hidden components, reserved Windows names, and
case-insensitive collisions are rejected. The entrypoint must name a nonempty
inventory file. SHA-256 metadata is exactly 64 lowercase hexadecimal digits.
These checks do not read the listed files, reject symlinks in an installed
filesystem, or verify their digests. Those checks belong to package handling.

`config_schema` is bounded object-schema metadata, using the same depth and
node limits as message data and a 16,384-byte limit. Its root must declare
`"type": "object"`; schema references (`$ref`, `$dynamicRef`, `$recursiveRef`)
are unsupported. The package settings service further compiles the bounded
[declarative subset](plugin-settings.md) for its editor and startup configuration.
It does not fetch external schemas or migrate legacy feature configuration.

The permission vocabulary is `dashboard_contributions`,
`plugin_configuration`, `desktop_notifications`, `http_video`, `network_http`, and `network_mqtt`. Duplicate and
unknown permissions are rejected. These names declare feature requirements;
they do not grant Tauri commands, access to core MQTT controls, or operating
system isolation. Access to a network MQTT service from a native worker must
not be confused with a host API for publishing arbitrary core control messages.

A non-null `signature` object contains `algorithm: "ed25519"`, a bounded
`key_id`, and a `signature` string of exactly 128 lowercase hexadecimal digits.
The protocol parser validates its encoding only. The separate
[package pipeline](plugin-packages.md) defines canonical signed bytes, publisher
trust, cryptographic verification, and file verification. An unsigned or
syntactically signed manifest must never be described as a verified package based
on this parser alone.

## Compatibility and implementation

The Rust source of truth is `src-tauri/src/plugins/protocol.rs`. Worker and host
messages reject unknown variants and fields so unsupported capabilities fail
explicitly. Adding a wire field requires a coordinated protocol change; the
host API range alone does not bypass wire compatibility.

Unit tests cover the version and identity handshake, unknown fields and
operations, malformed and oversized frames, bounded action data and results,
contribution limits, duplicate IDs, finite metrics, incompatible host APIs,
mobile target rejection, unsafe paths, invalid inventory metadata, schema
references, and signature encoding without claiming signature verification.

## Application lifecycle and authority

Tauri owns one desktop `PluginHost` and application package service, shared by all
windows. A clean profile starts with no package workers. After authentication,
explicit `desktop_plugins` declarations can download and install exact HTTPS
archive pins; cached enabled packages restore without another download. The
empty embedded publisher policy only disables the separate manual signed-file
flow. Archives and installed payloads are verified against their approved pin or
publisher before starting fresh workers. Configured disabled packages stay
stopped; legacy HA/camera settings never imply installation. Configuration edits
revoke obsolete work before publishing the replacement declaration set. There is no
executable-path or start-worker IPC. `WorkerSpec` is a trusted native API, exercised
by a separately compiled fixture executable. The package manager constructs it only after package
verification; a manually constructed spec itself is not a package trust decision.

Only authenticated `main` and `config` windows can call `get_plugin_snapshot` or
`plugin_action`. The latter requires the displayed worker instance, its plugin
and advertised action IDs, plus exact fixed-preset parameters or a value matching
the current numeric grant; it cannot select a core Tauri or MQTT command.
The host rechecks the process generation, current session epoch, and deadline
before dispatch. A logout or authentication-policy change synchronously revokes
the old epoch, clears contributions, and stops its workers. Logging in again
can restore enabled packages in a new epoch, but cannot revive old processes or
authorize their delayed responses. Background session expiry is checked at
one-second intervals while workers or package operations are active; each IPC
call independently requires a current session.

The fixed `plugin-host-update` event contains no worker data. The app coalesces
updates to at most 20 refresh signals per second, and windows retrieve an
authorized snapshot. Compact presentation descriptors render in the existing
header/Home/sidebar sections. The old generic flat contribution renderer is not mounted. The Plugins
settings card shows only explicit connection health for a fresh running worker,
without duplicating entities or household controls. Stale snapshots revoke UI
authority and pending gestures remain bound to their original worker instance.

The desktop settings tab manages packages through authenticated IPC, including
native selection, single-use verified previews, revision-bound settings, and
read-only worker choices. Generic camera group commands read and change native
package state, including declared enabled preferences. A
`plugin-configuration-changed` event refreshes only those declarations in an open
core settings draft. Group operations do not dispatch to a stopped worker or
reconnect the core transport.

Using notification live actions requires `live_view`, `plugin_configuration`,
`desktop_notifications`, and a manifest `live_view: {urls_setting: "camera_live_urls"}`. The named field
must be secret: it holds a JSON map of at most 32 exact IDs to HTTP(S) destinations,
bounded to 16 KiB total and 2,048 bytes per URL. Credentials in URL userinfo are
rejected; query/fragment data remains private. Only a notification interaction
can open a resolved destination in a generation-owned isolated window. The page
has no core/plugin management IPC authority. Revocation closes owned windows and
invalidates pending actions. The current native action backend is macOS; other
platforms retain ordinary notifications, with graphical action parity tracked in
TODO. This is separate from opaque host-downloaded media playback.

Standalone HA, Frigate, Kerberos, and Ring workers own their network connections.
Network permission metadata is neither an OS firewall nor a generic core network
proxy. Discovery, settings catalogs, and compact household semantics remain
worker-owned; the host validates their bounded contracts. All worker crates,
binaries, and manifests are excluded from core frontend and mobile artifacts.

Normal application exit waits for package initialization and transactions, stops
and reaps workers, drains owned media/windows, and releases the package-store lease
before permitting exit.
Repeated quit requests continue waiting for the same cleanup. Shutdown also
prevents subsequent registration and actions. Runtime policy caps registered
workers, in-flight actions, pipe queues, startup time, message/update rates,
and restart attempts. These are protocol and process-management bounds, not
operating-system CPU or memory quotas. Forced termination of the host or an
operating-system failure is outside the normal shutdown contract.

The implementation uses cancellable asynchronous pipe I/O and direct process
management; see [Tokio process lifecycle](https://docs.rs/tokio/latest/tokio/process/index.html)
and [Tauri run events](https://docs.rs/tauri/latest/tauri/enum.RunEvent.html).

## MQTT camera URL previews (host API 1.9)

A package using `mqtt_live` must require `^1.9` and declare `live_view` permission,
`preview_duration_seconds` from 1 to 15, and `live_view.mqtt_urls_setting` naming
a nonsecret string setting. The setting is a JSON camera-ID-to-endpoint map;
it grants only canonical HTTPS `/api/camera_proxy_stream/camera.<entity>`
destinations with no query, fragment or credentials. The optional manifest field
is omitted when absent so existing signed manifests retain their exact bytes.
The existing private `urls_setting` remains independently scoped.

```json
{
  "type": "mqtt_live",
  "id": "motion-unique-id",
  "title": "HA Front camera motion detected",
  "camera_id": "front",
  "url": "https://ha.example/api/camera_proxy_stream/camera.front?token=fixture"
}
```

The host validates both the camera ID and exact URL, allowing only one `token`
query with 1–512 ASCII alphanumeric, `_` or `-` characters. It derives a grant
for that single current URL and at most 15 seconds, with no download or
notification-click authority. Unknown cameras or substituted destinations fail
the worker generation with a generic reason. Diagnostics, routes and public
snapshots omit URLs. Delivery uses the existing private owner-window IPC,
incognito MJPEG viewer, focus preservation, per-plugin Always on top preference,
camera cooldown and active-window deduplication, queue expiry and generation
revocation. No native notification accompanies a `mqtt_live` frame.

The Kerberos 0.2 worker adds exact QoS-0 subscriptions only for configured cameras
on `homelab/cameras/live/<ID>`. It discards retained events and preserves its
camera cooldown across reconnects using clean MQTT sessions. The payload has no
timestamp: freshly republishing an old URL cannot be detected from this format.
An image origin CSP does not provide strict HTTP redirect denial; configured HA
endpoints must directly serve MJPEG without redirects.
