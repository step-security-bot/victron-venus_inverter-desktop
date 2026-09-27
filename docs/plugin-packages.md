# Desktop plugin packages

The desktop application installs packages declared in application configuration
using exact version, platform, and archive SHA-256 pins. It also retains a separate
manual signed-package review flow. **Configuration → Plugins** shows download and
restoration results, installed workers, typed settings, and package lifecycle actions.

Installed plugins may contribute compact header/Home controls, accordion groups,
appliance summaries, weather, and connection indicators through a validated
presentation contract. They do not append generic flat entity panels. Background
workers, camera notifications, and owned media remain independent of dashboard
rendering. A package that is neither configured nor installed contributes no provider UI or connection.

Frigate, Kerberos, and Ring each own an independent MQTT client and publish its
subscription health. The status bar summarizes enabled camera workers in one
**HA MQTT** indicator, green only when every participant is connected. Its tooltip
lists each worker; failed, restarting, missing configured workers and stale host
or configuration reads keep it offline. Explicitly disabled workers are excluded.
Each worker becomes connected only after the broker accepts its topic subscriptions.
This also covers workers using the former HA MQTT broker settings. **HA** reports
Home Assistant's separate REST/WebSocket connection. To receive worker fixes, select the plugin
archive URL and checksum from the new release as well as updating the app.
`build-local.sh` builds all four `.idplugin` archives and SHA-256 files before
building and installing the host app. Run `python3 scripts/build-local-plugins.py`
to compile and package only the workers (Python 3.11+ and Rust required). Each
successful run creates a complete new artifact directory under
`target/local-plugins/<native-target>/`; its path is printed with the result.
Failed builds never replace a previous successful artifact set. For shell callers,
`--print-path` writes only that directory to stdout; compiler/packager diagnostics
go to stderr. The script does not accept an arbitrary output file path.

On macOS, add `--install` to build and install into the existing application, or
use `--install --artifacts /absolute/path/to/artifacts` to reuse a completed build.
`build-local.sh` installs the new host and then activates its matching workers.
Only previously installed plugins are updated; enabled/disabled intent, settings,
secrets and configured release pins are preserved. The app must include local-build
support and be unlocked. The script restarts it and verifies installed archive
hashes; unsupported/locked apps time out and the previous selection is restored.

The explicit same-account `local-plugin-overrides.json` selection and immutable
archives reside in the application's data directory, outside encrypted settings.
The native host verifies the exact ID/version/target/SHA-256 and uses its existing
transactional installer. This is a trusted native developer operation, not a
webview installation API. Configuration → Plugins labels local builds. Repeated
builds retain the original release baseline; a newly configured release hash takes
precedence over an older override. Run `python3 scripts/build-local-plugins.py
--restore-release` to remove the local selection and restore configured release
packages without compiling. For a published beta, install its app and update the
required URLs/checksums from `desktop-plugins-<target>.json` in your configuration
backup. Local archive sets remain available for inspection after restoration.

Automatic Frigate and Kerberos motion previews do not show a duplicate system
notification over the video. Cameras without a configured preview continue to
send ordinary motion notifications.

Configured downloads can use unsigned packages and need no publisher or app
signing keys. The embedded publisher policy is currently empty, so manual package
selection remains unavailable. HA and camera provider implementations are separate
worker packages; their former bundled frontend/native clients have been removed.
Local tests, hosted artifact checks, publication, and installed-household acceptance
remain distinct checkpoints in [TODO](../TODO.md).

Android and iOS compile neither the manager UI nor its native commands, package
store startup, publisher policy, cryptographic verifier, ZIP handling, or packaging
implementation. Mobile preserves declarations as dormant configuration data;
it does not interpret, fetch or execute them. Mobile artifact gates reject the
desktop implementations.

The extraction build requires compatible package pins for the familiar feature
views: Home Assistant **0.13 or later**, Frigate **0.3 or later** for the Cameras
group, and Kerberos/Ring **0.1 or later**. Existing pins remain authoritative and
are not automatically upgraded with the app. Publish and select the matching
archive URL/checksum for each installed desktop target before recording upgrade
acceptance; older workers do not gain compact presentation from an app update.

## Declaring plugins in application configuration

Add a `desktop_plugins` list to the application settings or merge the corresponding
fragment from a published release into your settings backup before importing it.
The fragment contains package declarations, not a complete application backup.
Keep only the plugins you want. To share a configuration between desktop
platforms, combine their artifact maps under one declaration for each plugin.
Each declaration supplies an exact plugin ID and version, an enabled preference,
and one or more desktop target artifacts. For example:

```json
{
  "desktop_plugins": [
    {
      "plugin_id": "example.monitor",
      "version": "1.2.3",
      "enabled": true,
      "artifacts": {
        "aarch64-apple-darwin": {
          "url": "https://packages.example.invalid/monitor-1.2.3.idplugin",
          "sha256": "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
        }
      }
    }
  ]
}
```

The example is illustrative; use the URL and checksum of the actual released
archive. Desktop accepts at most eight unique plugin declarations. Versions are
exact semantic versions, not ranges or `latest`. Artifact keys must be supported
desktop Rust target triples. Every configured URL must use HTTPS and contain no
credentials, query, or fragment; SHA-256 uses 64 lowercase hexadecimal characters.
Unknown declaration and artifact fields are rejected on desktop. Mobile preserves
future metadata without interpreting it. Omitted `enabled` means true; explicit
false permits installation but never launches that worker.

After authentication, startup, configuration import/save, and **Retry restoration**
reconcile the declarations with the private package store. Missing
packages are downloaded with bounded HTTPS requests, checked against their exact
identity/version/target/hash, and installed through the existing verified package
lifecycle. Healthy matching installations are reused without downloading or
restarting them. A missing artifact for this target or a failed download appears
in the manager. A failed package change preserves the previous installed version.
The full pin describes the desired content: editing or importing a declaration
can replace an archive with different bytes at the same worker version or select
an older version intentionally. Replacement is transactional and retains the
previous active archive for rollback. The app follows the saved pins; upgrading
the application does not silently select a newer release or rebuild. Change the
declaration's URL/hash and, when needed, version to select different content.

Declared plugins take their package version and enabled state from configuration.
The Plugins tab can enable, disable, or uninstall these packages directly. Native
enable/disable saves the desired preference before changing installed lifecycle
state; a lifecycle failure is reported with that saved intent retained.
Confirmed uninstall also removes the declaration so startup or retry cannot
download the removed package again. Settings are retained by default. Manual
archive replacement and rollback remain unavailable while an exact declaration
owns the version. Settings remain editable.

Video-capable plugins offer **Always on top** in their settings, off by default.
Enable it separately for each plugin to keep its clips, snapshots, live previews,
and opened live-view pages above other windows. Save settings to apply the choice
to the plugin's next windows. Automatic previews still open without taking focus.
The host stores this preference with encrypted plugin settings; existing plugin
packages need no update and never receive it in their worker configuration.

If a declaration has no installed package, its configured row offers **Remove
from configuration** with a confirmation. This removes only the saved declaration
and stops future automatic restoration; retained settings, secrets, and stored
data are kept. The confirmation is bound to the complete declaration, so a newer
version, same-version pin replacement, or enabled-state change requires a fresh
confirmation. The operation waits for any active restoration. If that restoration
installs the package first, removal is rejected and the refreshed installed card
offers the ordinary **Uninstall** action instead.

Individual and camera-group changes share the restoration operation lock and
notify configuration windows without overwriting dirty core fields. Ordinary
core settings saves preserve the current saved declarations, even when submitted
from an older draft. Removing only a declaration through an explicit configuration
edit releases version management and retains the installed package and its data.
Use Uninstall when the package should also be removed.

An app upgrade or reinstall that preserves app data preserves declarations and
encrypted plugin settings. If app data was deleted, import a saved application
backup to restore the declarations and fetch missing packages. The portable backup
includes current public plugin settings in versioned module namespaces, but no
secrets. A newly downloaded package needing credentials remains installed and stopped until valid settings are saved, then
activation is retried if enabled in the declaration. Missing local data cannot be
recovered without a backup. A missing package can be downloaded again, but damaged
installed content is not automatically overwritten or erased. To reinstall a
damaged package that remains manageable, uninstall it while retaining settings,
then restore the declaration and retry. An
unreadable or corrupt store inventory is reported without deleting it; this flow
does not bypass an inventory error or silently reset the store. Only an explicitly
installed/declared matching built-in package can receive a native legacy
configuration seed. Existing plugin settings remain authoritative;
legacy fields alone never download anything. Core MQTT/IGW is unchanged.

Hosted desktop releases generate `.idplugin` assets, `.sha256` files, and
`desktop-plugins-<target>.json` fragments. Their pinned URLs refer to that frozen
release tag and become usable only after publication. See the
[release output contract](release-workflow.md#desktop-plugin-release-assets).

## Selecting and reviewing a signed package

1. Open **Plugins** in desktop settings and choose a package through the native
   file dialog. Cancelling the dialog changes nothing. Selection is available only
   when the release policy contains approved publishers and the store is ready.
2. The native service verifies publisher scope, signature, target/API compatibility,
   archive structure, and the complete inventory. It retains the verified bytes
   and returns an opaque review token; the webview cannot supply an archive path,
   executable path, public key, or replacement archive contents.
3. Review the plugin ID, proposed and installed versions, approved publisher key,
   target, and declared capabilities. **Enable after installation** defaults to
   selected and can be cleared. Capability declarations describe features; workers
   still run with the user's OS account privileges.
4. Choose **Install** or **Update** to consume the token and commit those exact
   bytes. Replacing the original file after review cannot change the installation.
   Changes apply immediately without the configuration Save button or an app
   restart. Cancelling a review discards its token.

At most one selection/review is pending in the application. Its token belongs to
the originating settings window and authentication epoch and expires after five
minutes. Replacement, cancellation, closing that window, logout, or authentication
policy changes invalidate it. An install attempt consumes the review; an expired
or failed attempt requires selecting and reviewing the package again.

Manual signed-file updates require a newer version and reject different content
under an already installed version. Use the retained **Rollback** action to return
to its previous archive. Explicit configuration pins have the separate content
selection behavior described above, including same-version replacement and
intentional downgrade.

Installed cards show their version and running, starting, failed, installed, or
disabled state. A running worker may also provide one compact connection summary;
entity state cards and device actions are not repeated in this manager.
Rollback names the retained version. Uninstall requires an inline
confirmation identifying the plugin and version, then stops its worker and removes
owned package files. Settings are retained by default; the confirmation offers
explicit deletion of that plugin's settings and secrets. A configuration-capable
package has a **Settings** action with a native-validated declarative editor.
Password fields show only whether a secret exists; replacement and clearing are
explicit. Saving persists immediately and restarts an enabled worker. A failed
restart is reported separately from a failed save. See the [settings
contract](plugin-settings.md). Native failures are rendered as text in English/Russian UI; metadata never becomes HTML or
executable frontend code.

The on-demand stored-data view lists sizes and quota usage without decrypting
records. Data with no installed owner can be removed after explicit confirmation;
data belonging to any installed package remains protected. A removed package's
name cannot be reconstructed from its record hash. See [retained data inventory
and cleanup](plugin-settings.md#retained-data-inventory-and-cleanup).

## Package and signature contract

An `.idplugin` file uses a strict, uncompressed ZIP layout. `manifest.json` comes
first; the producer writes regular payload files in sorted portable-path order.
Fixed ZIP timestamps and permissions make identical metadata, payload bytes, and
signing keys produce identical archive bytes. Sorting and fixed timestamp values
are producer normalization, not additional verifier requirements. No host filesystem timestamp enters the
package identity. The producer pins ZIP creator metadata to Unix on every desktop
OS. The verifier also accepts DOS creator metadata with ordinary file attributes,
including the regular Unix mode written by Windows ZIP tooling, while rejecting
links, special files, and directory attributes. Compressed/encrypted entries,
directories, extra fields,
comments, ZIP64, data descriptors, duplicate paths, and unlisted payloads are not
part of this format. The entire archive is bounded to 64 MiB.

The manifest uses the [worker manifest schema](plugin-worker-protocol.md).
Manual file selection requires an Ed25519 signature from an approved publisher.
Configured downloads instead authorize the exact archive SHA-256 recorded in
application settings; their manifest may have `signature: null`. Both paths enforce
the same target/API, archive structure, and complete payload inventory checks.
The signing message is the ASCII domain prefix
`inverter-desktop:idplugin:manifest:v1` followed by a NUL byte and compact UTF-8
JSON serialization of the typed manifest with `signature` set to `null`. Object
keys within schema metadata are sorted recursively. Verification requires the
canonical manifest representation, so duplicate JSON keys and alternative
representations cannot create different interpretations of the signed content.

The signature authenticates plugin identity, version, host API requirement,
target, entrypoint, configuration schema, permission declarations, and every
payload path, byte length, and SHA-256 digest. The verifier checks the complete
archive structure and file inventory before returning an immutable
`VerifiedPackage`. Installation consumes those verified bytes, rather than
reopening a caller-controlled archive after verification.

The parser's larger declared-inventory limit does not relax the stricter archive
limit. A manifest with incompatible target/API, unsafe paths, unexpected
files, missing files, or incorrect lengths/digests is rejected.

## Frigate worker package

The separately built [Frigate worker](../desktop-plugins/frigate/README.md)
uses an independent MQTT connection and automatic fifteen-second previews for
fresh motion events, with native notifications for motion without a preview. Older clip workers remain
supported by the native download path. [Kerberos](../desktop-plugins/kerberos/README.md) and
[Ring](../desktop-plugins/ring/README.md) have separate package identities,
configuration, topic filters, cooldowns, and lifecycle. All camera packages join
the generic Cameras group. Optional proxy authentication and live destinations
are explicitly scoped to each plugin; no worker borrows the core HA token or
MQTT client. `camera_live_urls` migrate into a secret map and never reach core IPC
or portable backups.

The native host owns bounded media downloads, private files, opaque media routes,
window lifecycle, and optional notification live actions. Disable, restart,
logout, and uninstall revoke generation-bound media authority. See the
[worker protocol](plugin-worker-protocol.md) and
[native media smoke harness](native-plugin-media-smoke.md). Local broker/HTTP
fixtures do not establish real camera reachability or OS graphical behavior.

## Home Assistant worker package

The separate [Home Assistant worker](../desktop-plugins/home-assistant/README.md)
owns its REST/WebSocket connection and isolated token. Host API `^1.8` supports its
compact header/Home controls, grouped sensors/numbers/covers/media/scenes, weather
forecast strip, appliance summaries, household notifications, and read-only
entity-choice catalog. A bounded structured layout preserves legacy labels,
icons, order, section visibility, and explicitly mapped appliance controls.
The host renders generic views and dispatches only exact advertised references;
it does not infer entities or services from labels.

Explicit entity selections authorize fixed button/scene actions, media commands,
binary controls, covers, and bounded numeric writes. Compact toggles use a fresh
read before selecting an absolute service operation. State comes from subsequent
HA updates, never an optimistic service response. The 64-read/63-control capacity,
128-contribution count, and 64 KiB complete frame remain independently enforced.
Choice catalogs grant no service authority, and discovery does not silently add
write access. Missing credentials or an unsupported migration remain visible
setup errors instead of reviving the removed bundled client.

The native migration planner runs only for explicitly selected package IDs. It
preserves complete mappings or rejects the seed; it never silently trims a
legacy installation to worker capacity. Existing encrypted worker records remain
authoritative. Public portable settings can seed a clean reinstall, with
credentials entered separately. See [module configuration](module-configuration.md).

`prepare-plugin-package.py --plugin home-assistant`, `frigate`, `kerberos`, or
`ring` stages fixed built-in metadata and checks the target's executable header.
The shared stdio library and camera transport crate are independent worker-only
workspaces. Neither providers nor their manifests are linked into core frontend
or mobile artifacts. Package, broker, HTTP/WebSocket, action, and lifecycle
fixtures cover local acceptance; exact-head CI and installed-app verification
remain recorded separately in [TODO](../TODO.md).

## Producing an archive

Build the frontend once, then run the desktop packaging tool from the repository
root. For a package installed through an exact configuration pin, no key is needed:

```bash
pnpm build
cargo run --locked --manifest-path src-tauri/Cargo.toml --example plugin-package -- \
  --unsigned \
  --manifest /absolute/path/plugin-manifest.json \
  --root /absolute/path/payload \
  --output /absolute/path/plugin.idplugin
```

The unsigned mode removes any input signature, rebuilds the payload inventory,
and uses the same deterministic ZIP and source protections. Compute SHA-256 of
the resulting complete archive and place that exact digest in its declaration.
For manual publisher review, the existing signed mode takes an externally
supplied seed file:

```bash
pnpm build
cargo run --locked --manifest-path src-tauri/Cargo.toml --example plugin-package -- \
  --manifest /absolute/path/plugin-manifest.json \
  --root /absolute/path/payload \
  --key-id publisher-v1 \
  --key-file /secure/path/publisher.seed \
  --output /absolute/path/plugin.idplugin
```

The manifest input supplies all `PluginManifest` metadata fields. Set `inventory`
to `[]` and `signature` to `null`; the tool replaces both using the actual payload
files and, in signed mode, the supplied key. The target is the worker's Rust desktop target triple,
and the entrypoint is a portable path relative to the payload root.

The key file contains exactly 32 raw Ed25519 seed bytes, not hexadecimal, Base64,
or PEM. Both it and the manifest file must reside outside the payload directory.
The tool does not generate keys or change receiver trust. It rejects linked files,
symlink/reparse-point parent components, nonportable names, source changes during
collection, and a payload that contains a copy of the private seed. The source
tree admits at most 128 files and 512 directory entries within the archive budget.

The destination parent must already exist. Output publication never overwrites an
existing file, including a symlink. Identical valid inputs produce identical bytes;
the producer fixes timestamps to 1980-01-01 and archive permissions to 0644 for
data and 0755 for the entrypoint. It only creates an archive and never launches
the worker. This plugin implementation introduces no new requirement to sign or
notarize desktop executables or the application. Archive authentication is a
separate mechanism from platform application signing; Android signing does not
apply to this desktop-only plugin ecosystem.

Windows MSVC builds embed the same Common Controls v6 manifest in application,
test, and example executables. Tauri's default resource handling only covers
application binaries; the other executables also need this dependency to load
the linked dialog/tray APIs before their own code can run.

## Publisher trust

`src-tauri/src/plugins/publishers.json` is a release-owned policy embedded only in
desktop builds. Each publisher entry has a stable `key_id`, a 32-byte Ed25519
public key encoded as 64 lowercase hexadecimal digits, and an explicit list of
plugin IDs that key may sign. Key rotation can retain the old and new key IDs
for the same plugin during a release transition.

The policy has no publishers. It applies to manual signed-file selection;
configured exact archive pins are independently authorized by saved application
configuration and need no publisher keys. The manual path has no trust-on-first-use,
package-provided public key, webview key override, or environment override.
Fixture signing keys exist only in tests and are never added to this policy.

Native test or development callers may construct their own `TrustStore`; that is
an explicit native trust boundary, not an application user setting. A key valid
for one plugin cannot sign another plugin merely because the signature is valid.
Changing or removing release trust must take effect when installed packages are
reverified before a subsequent start.

## Application and native lifecycle

Tauri owns one desktop `PluginHost` and `PackageApplication`, shared by all
windows. The application opens its private plugin store once under its local data
directory. Only authenticated `config` windows may call manager snapshot, selection,
review, install/update, enable/disable, rollback, settings, or uninstall commands. Dashboard
windows can read contributions and dispatch advertised actions, but cannot manage
packages. File dialogs and trusted publisher configuration remain native-owned.
The management IPC surface is:

- `get_plugin_manager_snapshot`: authenticated inventory, runtime status,
  initialization errors, configured-plugin results, and manual installation availability.
- `retry_configured_plugins`: retry saved declarations in the current authenticated session.
- `remove_configured_plugin({ pluginId, expectedDeclarationRevision })`: remove
  a declared but uninstalled package from automatic restoration. The manager
  snapshot supplies the opaque `declaration_revision`; the native save gate
  compares it with the complete current encrypted declaration before persistence.
  This command never uninstalls a package or deletes settings/data, and rejects
  a changed declaration, installed package, or revoked session.
- `preview_plugin_package`: takes no path argument; opens the native dialog and
  returns `null` on cancellation or verified metadata with an opaque token.
- `install_plugin_package({ token, enable })` and
  `discard_plugin_package({ token })`: consume or discard the native-owned review.
- `set_plugin_enabled({ pluginId, enabled })`,
  `rollback_plugin_package({ pluginId })`, and
  `uninstall_plugin_package({ pluginId, deleteSettings? })`: manage an installed
  identity. Enable/disable and uninstall also update configured declarations;
  rollback requires an unmanaged identity. Omitted `deleteSettings` retains data.
- `get_plugin_settings({ pluginId })`: verified schema, ordinary values, opaque
  revision, and secret-presence flags.
- `save_plugin_settings({ pluginId, revision, values, secretChanges })`: validate
  and persist typed values/secret changes; return refreshed settings and a separate
  nullable `restart_error`.
- `get_retained_plugin_data`: bounded ciphertext metadata, current installed
  owner associations, and storage quotas; no stored values or key access.
- `delete_retained_plugin_data({ recordId, revision })`: remove one unchanged
  canonical record only when the native installed inventory has no owner for it.

Ordinary `save_config({ config })` calls preserve the latest saved declarations.
An intentional declaration edit can pass `desktopPluginsExpected`, containing
the complete previously read declaration array (including `[]` for an empty
baseline). The native config-update lock compares it before any write and rejects
a stale baseline. Existing authenticated configuration-window authority and
declaration validation still apply. Portable backup import is a separate explicit
declaration-restoration path.

Configured uninstall first saves disabled intent, durably disables the installed
record, then removes the declaration and package. These writes are not one
filesystem transaction. A later failure is reported with the remaining state
visible and retryable; an already disabled package stays stopped rather than
becoming an enabled unmanaged package on restart. Requested settings cleanup
retains its existing separate-write failure semantics.

The application uses `PackageManager::open_with_configuration` with its private
settings loader; `PackageManager::open(root, target, trust, host)` remains available
for native callers without configured workers. Its clones share one transaction
mutex. A lifetime OS file lock excludes another
manager/process from the same store until owned workers have been reaped. Call
`close().await` to complete cleanup explicitly; if process cleanup cannot be
confirmed, the lease must remain held until the hosting process exits.
Successful cleanup explicitly unlocks the file before closing it, so a descriptor
temporarily inherited by an unrelated process cannot delay reopening the store.
An unlock failure retains the lease; explicit close can be retried.

The store uses an atomically created `store-v1/` ownership directory, a bounded
`state.json` inventory, private
staging directories, and content-addressed package records. Each immutable archive
record retains its verified bytes and extracted payload; a configured replacement
may have the same semantic version with a different archive digest. At most eight plugins,
two retained versions per plugin after recovery, 512 MiB of total store data, and
8,192 filesystem entries are admitted. These storage limits are independent of
the worker's runtime CPU or memory use.

Package lifecycle operations are serialized. Verification and staging precede
activation; a candidate must complete the real worker handshake before becoming
active. The previous working package remains available for rollback. A failed
update must preserve the previous package state and restore its worker when the
original session is still authorized.

Operations capture the authentication epoch before waiting for the transaction
lock. Registration checks that same epoch under the host's authority lock.
Logout and re-login cannot authorize an old queued activation. Worker removal
waits for termination and reaping before releasing its registry slot or deleting
owned package files. The package manager has no MQTT/IGW handle and does not
reconnect core telemetry.

Opening a package store recovers metadata and interrupted staging work without
executing workers. After successful authentication, configured declarations are
reconciled and other installed packages recorded as enabled can resume. Every launch
rechecks the archive against its configured pin or approved publisher and verifies
the installed payload. Disabled packages stay stopped;
a queued restore cannot undo an explicit disable or uninstall. A failed restore
is visible in the manager. Only explicit package declarations cause automatic
downloads; preserved bundled HA/camera settings do not.

Logout, policy changes, and session expiry revoke the old epoch, clear previews
and contributions, and stop its workers. A later login may restore enabled packages
as fresh processes in a new epoch; it never authorizes old queued actions or late
results. Background expiry checks cover workers and pending package operations.
The package-store lease survives ordinary authentication transitions. Normal quit
waits for initialization, transactions, and process reaping before releasing the
lease and permitting exit; repeated quit requests wait for the same cleanup.

`plugin-host-update` signals changes without carrying worker data. Settings fetch
an authenticated snapshot with at most one request in flight and one pending
refresh. Native inventory metadata is cached by inventory/session revision;
frequent worker updates do not rehash all files. Mutation/restoration/session
changes invalidate that cache, and launch verification always reads and verifies
the archive and payload again. Package operations do not reload core configuration
or reconnect MQTT/IGW.

Native migration seeds are bound to the verified installed package and its
settings schema. They do not grant installation authority. Uninstall must leave
unrelated core/plugin data intact.

Ordinary caller cancellation does not abandon an in-progress transaction: an
owned task finishes or restores its state while holding the store lease. Process
interruption can leave staging or obsolete content that the next open removes.
State files are synced before atomic replacement; parent directories are synced
on Unix. This does not promise identical sudden-power-loss behavior across file
systems, and Windows directory metadata has no equivalent sync step in this API.
Force-killing the application can also leave a noncooperating OS process outside
normal worker cleanup; this checkpoint does not adopt orphan processes.

## Verification boundaries

Tests use disposable signing keys, actual produced archives, and separately
compiled fixture workers. They must establish deterministic packaging, signature
and inventory rejection, installation, failed activation, rollback, cancellation,
recovery, and removal. Application-service tests additionally cover file replacement
after review, single-use/expired consent, closed windows, authentication races,
enabled-only restoration, initialization/shutdown ordering, and empty release trust.
Vue tests exercise actual review/management controls, escaped metadata, cancellation,
busy states, listener cleanup, late responses, and burst refresh coalescing. Mobile
source/dependency/payload checks complement these desktop lifecycle tests. The
current checkpoint's local suite results, remaining target/artifact checks, and PR
merge are tracked separately in [TODO.md](../TODO.md). Browser visual smoke uses
mocked native IPC and does not establish native file-dialog GUI behavior.

A verified package establishes exact pinned content or approved publisher scope. Its worker still
runs with the user's OS privileges; this is not an OS sandbox. Permission
metadata alone does not grant host services. This checkpoint implements scoped
native settings and startup configuration for `plugin_configuration`, bounded
notifications for `desktop_notifications`, scoped clips/snapshots for `http_video`,
and configured live-view actions. General arbitrary network requests and
request-time secret access are not exposed.
Frigate opens its own MQTT connection and has no access to the core MQTT client
through this protocol.
Current source, fixture, hosted, and installed-app acceptance stays in
[TODO.md](../TODO.md).

This checkpoint does not establish production publisher provisioning, platform
code signing, production household behavior, physical inverter commands,
or mobile device usability.
