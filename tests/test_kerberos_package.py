"""Kerberos camera metadata, native target staging and private live-view settings."""

import unittest

from camera_package_helpers import validate_camera_staging


class KerberosPackageTests(unittest.TestCase):
    """The standalone worker has its own native identity and explicit grants."""

    def test_native_packages_keep_live_view_destinations_private(self):
        """Live URLs cannot enter exported public settings or a worker payload."""
        manifest = validate_camera_staging(self, "kerberos")
        self.assertEqual(set(manifest["permissions"]), {
            "dashboard_contributions", "plugin_configuration", "network_mqtt",
            "desktop_notifications", "live_view",
        })
        self.assertNotIn("http_video", manifest)
        self.assertEqual(manifest["live_view"], {
            "urls_setting": "camera_live_urls", "preview_duration_seconds": 15,
            "mqtt_urls_setting": "mqtt_live_endpoints",
        })
        schema = manifest["config_schema"]
        self.assertEqual(schema["required"], ["mqtt_host"])
        fields = schema["properties"]
        self.assertEqual(fields["mqtt_topics"]["default"], "kerberos/agent/+;kerberos/hub/+")
        private = {key for key, field in fields.items() if field.get("writeOnly")}
        self.assertEqual(private, {"mqtt_username", "mqtt_password", "camera_live_urls"})
        live = fields["camera_live_urls"]
        self.assertEqual(live["maxLength"], 16384)
        self.assertNotIn("default", live)
        self.assertEqual(live["x-editor"]["schema"]["maxProperties"], 32)
        self.assertEqual(live["x-editor"]["schema"]["additionalProperties"]["maxLength"], 2048)
        self.assertNotIn("ha_token", fields)
        self.assertNotIn("frigate_base_url", fields)


if __name__ == "__main__":
    unittest.main()
