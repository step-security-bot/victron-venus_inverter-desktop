#[cfg(test)]
use rumqttc::tokio_rustls::rustls;
use rumqttc::Transport;
#[cfg(test)]
use std::sync::Arc;

pub(super) fn transport(
    tls: bool,
    username: Option<&str>,
    password: Option<&str>,
) -> Result<Transport, String> {
    if !tls {
        if username.is_some_and(|s| !s.is_empty()) || password.is_some_and(|s| !s.is_empty()) {
            return Err("MQTT credentials require TLS. Enable MQTT TLS and use your broker's TLS port, or remove credentials for an anonymous Cerbo connection on a trusted LAN.".into());
        }
        return Ok(Transport::tcp());
    }
    Ok(Transport::tls_with_config(
        crate::tls::client_config()?.into(),
    ))
}

#[cfg(test)]
fn tls_transport(
    certs: Vec<rustls::pki_types::CertificateDer<'static>>,
) -> Result<Transport, String> {
    Ok(Transport::tls_with_config(
        crate::tls::client_config_with_roots(certs)?.into(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustls::pki_types::pem::PemObject;

    // Disposable self-signed fixture, never a production trust anchor or credential.
    fn test_certificate() -> rustls::pki_types::CertificateDer<'static> {
        rustls::pki_types::CertificateDer::from_pem_slice(include_bytes!(
            "testdata/localhost-cert.pem"
        ))
        .unwrap()
    }

    fn tls_broker() -> (u16, std::thread::JoinHandle<std::io::Result<Vec<u8>>>) {
        use std::io::{Read, Write};
        use std::net::TcpListener;
        use std::time::{Duration, Instant};
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        listener.set_nonblocking(true).unwrap();
        let private_key = rustls::pki_types::PrivateKeyDer::from_pem_slice(include_bytes!(
            "testdata/localhost-key.pem"
        ))
        .unwrap();
        let config = rustls::ServerConfig::builder_with_provider(Arc::new(
            rustls::crypto::aws_lc_rs::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(vec![test_certificate()], private_key)
        .unwrap();
        let server = std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(5);
            let socket = loop {
                match listener.accept() {
                    Ok((socket, _)) => break socket,
                    Err(error)
                        if error.kind() == std::io::ErrorKind::WouldBlock
                            && Instant::now() < deadline =>
                    {
                        std::thread::sleep(Duration::from_millis(5))
                    }
                    Err(error) => return Err(error),
                }
            };
            socket.set_nonblocking(false)?;
            socket.set_read_timeout(Some(Duration::from_secs(5)))?;
            socket.set_write_timeout(Some(Duration::from_secs(5)))?;
            let connection = rustls::ServerConnection::new(Arc::new(config)).unwrap();
            let mut stream = rustls::StreamOwned::new(connection, socket);
            let mut first = [0];
            stream.read_exact(&mut first)?;
            assert_eq!(
                first[0], 0x10,
                "first decrypted MQTT packet must be CONNECT"
            );
            let mut length = 0;
            let mut multiplier = 1;
            loop {
                let mut encoded = [0];
                stream.read_exact(&mut encoded)?;
                length += (encoded[0] as usize & 127) * multiplier;
                if encoded[0] & 128 == 0 {
                    break;
                }
                multiplier *= 128;
                assert!(multiplier <= 128 * 128 * 128);
            }
            let mut connect = vec![0; length];
            stream.read_exact(&mut connect)?;
            stream.write_all(&[0x20, 0x02, 0x00, 0x00])?;
            stream.flush()?;
            // Let the client consume CONNACK and close first. Closing a socket
            // with unread TLS records can reset it and discard the response.
            let mut disconnect = [0; 256];
            let _ = stream.read(&mut disconnect);
            Ok(connect)
        });
        (port, server)
    }

    #[test]
    fn tls_probe_authenticates_with_a_trusted_matching_certificate() {
        let (port, server) = tls_broker();
        let transport = tls_transport(vec![test_certificate()]).unwrap();
        let result = super::super::probe_mqtt_connack(
            "localhost",
            port,
            Some("fixture-user"),
            Some("fixture-password"),
            transport,
        );
        let server_result = server.join().unwrap();
        assert!(result.is_ok(), "probe={result:?}, broker={server_result:?}");
        let decrypted = server_result.unwrap();
        assert!(decrypted
            .windows(b"fixture-password".len())
            .any(|w| w == b"fixture-password"));
    }

    #[test]
    fn tls_probe_rejects_wrong_hostname_before_sending_mqtt_credentials() {
        let (port, server) = tls_broker();
        let transport = tls_transport(vec![test_certificate()]).unwrap();
        assert!(super::super::probe_mqtt_connack(
            "127.0.0.1",
            port,
            Some("fixture-user"),
            Some("fixture-password"),
            transport
        )
        .is_err());
        assert!(server.join().unwrap().is_err());
    }

    #[test]
    fn tls_probe_rejects_an_untrusted_certificate_before_sending_credentials() {
        let (port, server) = tls_broker();
        let transport = transport(true, Some("fixture-user"), Some("fixture-password")).unwrap();
        assert!(super::super::probe_mqtt_connack(
            "localhost",
            port,
            Some("fixture-user"),
            Some("fixture-password"),
            transport
        )
        .is_err());
        assert!(server.join().unwrap().is_err());
    }

    #[cfg(any(target_os = "ios", target_os = "macos"))]
    #[test]
    fn apple_tls_rejects_untrusted_peer_before_sending_credentials() {
        let (port, server) = tls_broker();
        let config = crate::tls::apple_client_config().unwrap();
        assert!(super::super::probe_mqtt_connack(
            "localhost",
            port,
            Some("fixture-user"),
            Some("fixture-password"),
            Transport::tls_with_config(config.into()),
        )
        .is_err());
        assert!(server.join().unwrap().is_err());
    }

    #[test]
    fn anonymous_cerbo_tcp_remains_supported() {
        assert!(matches!(
            transport(false, None, None).unwrap(),
            Transport::Tcp
        ));
        assert!(matches!(
            transport(false, Some(""), Some("")).unwrap(),
            Transport::Tcp
        ));
    }

    #[test]
    fn either_credential_requires_tls() {
        for (user, password) in [
            (Some("user"), None),
            (None, Some("secret")),
            (Some("user"), Some("secret")),
        ] {
            assert!(transport(false, user, password)
                .err()
                .unwrap()
                .contains("require TLS"));
        }
    }

    #[test]
    fn public_probe_and_inverter_setup_reject_plaintext_credentials_before_network_access() {
        let error = super::super::test_mqtt_connection(
            "invalid.invalid",
            8883,
            Some("user"),
            Some("secret"),
            false,
        )
        .unwrap_err();
        assert!(error.contains("require TLS"));
        let mut client = super::super::MqttClient::new(
            "invalid.invalid".into(),
            8883,
            Some("user".into()),
            Some("secret".into()),
            "test".into(),
        );
        assert!(client
            .configure_transport(false)
            .unwrap_err()
            .contains("require TLS"));
    }

    #[test]
    fn tls_uses_trusted_roots_and_keeps_tls_transport() {
        assert!(matches!(
            transport(true, Some("user"), Some("secret")).unwrap(),
            Transport::Tls(_)
        ));
    }
}
