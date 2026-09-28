use crate::config::Configuration;
use tokio::sync::{mpsc, watch};

pub use inverter_worker_protocol::{Output, StopReason};
pub type HostFrame = inverter_worker_protocol::HostFrame<Configuration>;
pub const PLUGIN_ID: &str = "inverter-desktop.kerberos";

pub fn stdio() -> (
    mpsc::Receiver<HostFrame>,
    Output,
    watch::Receiver<StopReason>,
) {
    inverter_worker_protocol::stdio::<Configuration>()
}

pub fn validate_hello(frame: HostFrame) -> Result<String, &'static str> {
    match frame {
        HostFrame::Hello {
            protocol_version: 1,
            host_api_version,
            plugin_id,
        } if plugin_id == PLUGIN_ID => {
            let version =
                semver::Version::parse(&host_api_version).map_err(|_| "unsupported host API")?;
            if !semver::VersionReq::parse("^1.9")
                .expect("fixed version requirement")
                .matches(&version)
            {
                return Err("unsupported host API");
            }
            Ok(host_api_version)
        }
        _ => Err("invalid worker handshake"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;
    use std::io::{self, Write};
    #[test]
    fn requires_host_19_and_own_plugin_identity() {
        for (api, accepted) in [
            ("1.8.0", false),
            ("1.9.0", true),
            ("1.10.0", true),
            ("1.7.99", false),
            ("1.8.0-beta.1", false),
            ("2.0.0", false),
            ("invalid", false),
        ] {
            assert_eq!(
                validate_hello(HostFrame::Hello {
                    protocol_version: 1,
                    host_api_version: api.into(),
                    plugin_id: PLUGIN_ID.into()
                })
                .is_ok(),
                accepted
            );
        }
        assert!(validate_hello(HostFrame::Hello {
            protocol_version: 1,
            host_api_version: "1.9.0".into(),
            plugin_id: "inverter-desktop.frigate".into()
        })
        .is_err());
        assert!(validate_hello(HostFrame::Hello {
            protocol_version: 2,
            host_api_version: "1.9.0".into(),
            plugin_id: PLUGIN_ID.into()
        })
        .is_err());
    }
    struct PendingFlush {
        bytes: Vec<u8>,
        release: std::sync::mpsc::SyncSender<()>,
    }

    struct ControlledWriter {
        bytes: Vec<u8>,
        flushed: mpsc::Sender<PendingFlush>,
    }

    impl Write for ControlledWriter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.bytes.extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            let (release, released) = std::sync::mpsc::sync_channel(1);
            self.flushed
                .blocking_send(PendingFlush {
                    bytes: std::mem::take(&mut self.bytes),
                    release,
                })
                .map_err(|_| io::Error::from(io::ErrorKind::BrokenPipe))?;
            released
                .recv()
                .map_err(|_| io::Error::from(io::ErrorKind::BrokenPipe))
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn connection_waits_for_written_configuration_acknowledgement() {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let (frames, mut incoming) = mpsc::channel(2);
        assert!(frames
            .send(HostFrame::Hello {
                protocol_version: 1,
                host_api_version: "1.9.0".into(),
                plugin_id: PLUGIN_ID.into(),
            })
            .await
            .is_ok());
        let configuration = serde_json::from_value(serde_json::json!({"revision":"revision-1","values":{"mqtt_host":"127.0.0.1","mqtt_port":listener.local_addr().unwrap().port()},"secrets":{}})).unwrap();
        assert!(frames
            .send(HostFrame::Configuration { configuration })
            .await
            .is_ok());
        let (flushed, mut outgoing) = mpsc::channel(2);
        let output = Output::with_writer(ControlledWriter {
            bytes: Vec::new(),
            flushed,
        });
        let session = tokio::spawn(async move { crate::session(&mut incoming, &output).await });
        let ready = outgoing.recv().await.unwrap();
        ready.release.send(()).unwrap();
        let acknowledgement = outgoing.recv().await.unwrap();
        let value: Value = serde_json::from_slice(&acknowledgement.bytes).unwrap();
        assert_eq!(
            value,
            serde_json::json!({"type":"configuration_ready","revision":"revision-1"})
        );
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(100), listener.accept())
                .await
                .is_err(),
            "no TCP connection before the acknowledgement is written"
        );
        acknowledgement.release.send(()).unwrap();
        let connecting = outgoing.recv().await.unwrap();
        connecting.release.send(()).unwrap();
        let _connection =
            tokio::time::timeout(std::time::Duration::from_secs(2), listener.accept())
                .await
                .unwrap()
                .unwrap();
        session.abort();
        let _ = session.await;
    }
}
