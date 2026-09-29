# Electricity tariff editor

The controller is the default source of electricity prices for the installation.
Every dashboard consumes `ui_config.electricity_tariff`; saved browser or older
application-configuration plans no longer override it automatically. Public
read-only dashboards display the controller tariff and expose no editing.

In an updated desktop/mobile app, **Configuration → Electricity tariff** edits
the shared plan immediately on inverter-control. Tariff editing, choosing a local
plan and the interval energy viewer all live in Configuration. The dashboard
contains the estimated cost (when available), current rate and measured grid
import/export in kWh; hover for
the source, billing period and calculation details.
Saving waits for a matching controller acknowledgement after atomic persistence;
a stale revision, validation failure or disk error leaves the previous plan intact.
Both direct MQTT and the updated authenticated inverter-gateway transport support
this command. A disconnected/older controller cannot claim successful saving.
The web dashboard displays the shared plan; use the application or controller's
installation tools to edit it.

In Configuration, choose **Use a local tariff on this device** explicitly for a
standalone local copy. The choice persists across window closures and app restarts,
separately for each installation on this device. A different installation defaults
to its controller until a local choice is explicitly saved there. Existing local
data is preserved for export and import; it is never silently uploaded to the
controller. **Use controller tariff** returns to the shared plan without deleting
the saved local copy. Tariff editing never changes inverter control
flags, charging policy or Emporia settings.

The Univer spreadsheet loads only when opened. Rows are half-hour periods,
Monday–Sunday in the tariff's IANA time zone. Prices are currency units/kWh,
not cents. Zero and negative prices are valid; blank, text, nonfinite and formula
cells are rejected. Cancel discards the draft. JSON import/export moves a plan
without account credentials. No initial numeric price is suggested.

For a flat tariff, daily grid kWh produces an explicitly approximate energy cost.
For time-of-use tariffs, the strip shows the current rate; a daily cost needs
interval consumption and is deliberately unavailable from a daily total alone.
The former hard-coded USD 0.31/kWh estimate has been removed. Taxes, fixed fees,
demand charges and consumption tiers are outside this energy-rate model. The editor does not invent any initial rate.

## Seasons and billing periods

A version 2 tariff contains a default weekly `rates` grid and a `seasons` array.
Each season has a `name`, distinct calendar `months` (1–12) and its own complete
48×7 `rates` grid. Months may not overlap. Months without an override use the
default grid. The editor opens the schedule active now; switching schedules
commits any cell being edited before showing the other grid. Import and export
retain all schedules, including those not currently visible. Use **Add season** to create a schedule, enter its name and select calendar
months. Edit its weekly prices or fill the selected week. **Remove this season**
removes only that draft override; the default schedule then covers those months.
All seasons must have nonoverlapping months before saving.

An optional `billingDay` (integer 1–31) starts the billing period in the tariff's
time zone. For example, day 17 on September 24 displays September 17–October 16.
Short months clamp days 29–31 to the last calendar day without shifting later
months. This is the period start, not the payment due date. Season prices still
switch at local midnight on the first of their months, even mid-billing-period.
The displayed period is not a calculated invoice: time-of-use totals require
interval consumption. Billing dates alone cannot reconstruct that consumption.

Legacy version 1 weekly files and stored plans remain readable and migrate to
version 2 without assuming a billing date. The storage key remains unchanged.
Old app versions reject version 2 instead of silently treating seasonal prices
as year-round prices. Update the receiving app before importing a new export.

## Controller persistence and migration

The controller stores the shared plan at
`/data/setupOptions/inverter-control/electricity-tariff.json`. It survives controller
updates. Clearing it writes an explicit `null`, preventing an older fallback file
from silently restoring a removed plan. Runtime edits apply without restarting
the control service. Offline CLI/deployment edits take effect at service restart.

Older `victron.energy-tariff` application modules remain in portable configuration
backups for data preservation, but are not active pricing sources. First-run app
setup now defers tariff configuration until a controller connection exists. Export
and import a reviewed legacy plan explicitly to migrate it; there is no automatic
upload that could overwrite a newer controller plan. Local browser files remain
available through the explicit local mode.

See the [controller installation and deployment guide](https://github.com/victron-venus/inverter-control/blob/main/docs/electricity-tariffs.md)
for SetupHelper, compact schedule files, validation and `TARIFF_FILE` provisioning.

## Emporia import

The companion `dbus-emporia-vue/scripts/export_tariff.py` reads the existing
configured device's `locationProperties` endpoint and emits only tariff fields.
Run it on the machine that already holds the driver's configuration and tokens:

```sh
python3 scripts/export_tariff.py --config /path/to/config.json \
  --device-gid 12345 --currency USD --output emporia-tariff.json
```

Use the actual configured device ID and the currency shown in the Emporia app.
Import the resulting JSON with **Import tariff**. The exporter refuses to
replace an existing file and never includes credentials or address fields.
The output must be a JSON filename without directory components; the file is
created in the current working directory with owner-only permissions.

A nonempty `utilityRateGid` identifies a selected utility plan; the available
PyEmVue device-properties contract does not provide the plan's time-of-use
schedule. Such an import retains the plan reference and leaves price cells
blank. Copy the actual schedule from the Emporia app, review it, and save.
It never treats `usageCentPerKwHour` as a complete TOU plan. For a legacy flat
plan without a utility plan ID, cents are converted to currency/kWh explicitly.

## Implementation

The tariff model and spreadsheet are shared with inverter-dashboard-vue; native
application settings and the compact dashboard presentation are maintained here. Native controller transport lives
only in the desktop/mobile build. Univer OSS packages are pinned to
1.0.2; no paid import/export or server plugin is required. The heavy editor is a
separate lazy chunk. JSON is the exchange format; XLSX is not part of this feature.
The browser stores local overrides per origin; desktop additionally scopes them by
portal ID (falling back to gateway or MQTT host). Settings use the saved connection
profile, not unsaved edits to the configuration form. Local mode and price edits
synchronize between this app's main and Configuration windows through native
events as well as browser storage events. Native events carry only the installation
scope and cause a fresh read; delayed notifications cannot replay old prices or
undo a removal. They
remain outside portable configuration backups and controller uploads. No cloud
synchronization runs.

Validation covers rates, missing data, source metadata, separate scopes, storage
failure, season changes, pending-cell preservation, short-month billing boundaries
and local-time/DST selection. Run the existing frontend build and test
commands after changing the mirrored files.

## Daily grid import and export

The daily statistics strip shows `Today ↓ import / ↑ export kWh` beside the
current rate. These values come from `daily_stats.grid_energy` in
inverter-control, using the selected physical Victron grid meter's aggregate
`/Ac/Energy/Forward` and `/Ac/Energy/Reverse` counters. Home Assistant is not a
source. Desktop never sums phase counters, integrates instantaneous power or
substitutes the older `grid_kwh` field.

The controller stores its counter baseline across restarts and uses the Cerbo
installation's time zone. A first observation after midnight, a meter reset or
an unverified day boundary starts partial coverage, labelled `Since HH:mm`.
The app does not present that partial amount as a full-day total. A complete
day requires a verified start-of-day baseline. Missing, invalid, stale (over
90 seconds old) or previous-day readings display dashes, including when an
older controller does not provide the new field. A measured zero remains zero.

Update inverter-control to a version that provides this telemetry to enable
the figures. The tooltip identifies the meter, date, time zone and coverage.
These counters do not change the tariff or calculate a time-of-use daily cost.

## Measured interval energy cost

Open **Configuration → Electricity tariff → Interval energy cost**. Import measured
grid-import CSV or JSON to calculate energy charges for a selected local date or
billing period. The shared calculator applies seasonal/weekday prices and DST,
reports missing duration, and excludes intervals crossing a price or date boundary
without inventing their within-interval consumption. A partial subtotal is visibly
marked and never presented as a full bill. See the [complete import format, provider
data preparation, limits and coverage guide](https://github.com/victron-venus/inverter-dashboard-vue/blob/main/docs/electricity-tariffs.md#measured-interval-energy-cost).

Readings stay in local browser/webview storage, scoped to the dashboard installation.
Import replaces that installation’s history; invalid input and storage failures
preserve the previous copy. They are not included in configuration backups, tariff
exports or controller uploads. Keep original exports for durable archival. Emporia
currently has no gross-import chart-history source in its upstream API client;
net mains energy and sampled grid power are not accepted substitutes. Automatic
history synchronization is not provided.
