//! TLS trust shared by remote Gateway HTTPS and inverter MQTT.
//! iOS uses Apple's trust evaluation; its system roots are not Unix PEM files.
//! Explicit roots on Android avoid a platform verifier that requires separate
//! JNI/Kotlin initialization which this application does not install.
use rumqttc::tokio_rustls::rustls;
use std::sync::Arc;

#[cfg(target_os = "ios")]
pub(crate) fn client_config() -> Result<rustls::ClientConfig, String> {
    apple_client_config()
}

#[cfg(not(target_os = "ios"))]
pub(crate) fn client_config() -> Result<rustls::ClientConfig, String> {
    client_config_with_roots(platform_certificates()?)
}

// Compile this same builder on macOS in tests to exercise the Apple backend.
// Production macOS retains its existing explicit-root behavior.
#[cfg(any(target_os = "ios", all(test, target_os = "macos")))]
pub(crate) fn apple_client_config() -> Result<rustls::ClientConfig, String> {
    use rustls_platform_verifier::BuilderVerifierExt;

    Ok(rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::aws_lc_rs::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .map_err(|_| "TLS cannot initialize supported TLS versions")?
    .with_platform_verifier()
    .map_err(|_| "TLS cannot initialize Apple certificate verification")?
    .with_no_client_auth())
}

#[cfg(any(not(target_os = "ios"), test))]
pub(crate) fn client_config_with_roots(
    certs: Vec<rustls::pki_types::CertificateDer<'static>>,
) -> Result<rustls::ClientConfig, String> {
    let mut roots = rustls::RootCertStore::empty();
    let (accepted, _) = roots.add_parsable_certificates(certs);
    if accepted == 0 {
        return Err(
            "TLS cannot load trusted system certificates; check the device's certificate store"
                .into(),
        );
    }
    Ok(rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::aws_lc_rs::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .map_err(|_| "TLS cannot initialize supported TLS versions")?
    .with_root_certificates(roots)
    .with_no_client_auth())
}

#[cfg(not(any(target_os = "android", target_os = "ios")))]
fn platform_certificates() -> Result<Vec<rustls::pki_types::CertificateDer<'static>>, String> {
    let result = rustls_native_certs::load_native_certs();
    // Some system stores contain unusable entries; retain successfully loaded roots.
    if result.certs.is_empty() {
        return Err(
            "TLS cannot load trusted system certificates; check the device's certificate store"
                .into(),
        );
    }
    Ok(result.certs)
}

#[cfg(target_os = "android")]
fn platform_certificates() -> Result<Vec<rustls::pki_types::CertificateDer<'static>>, String> {
    // Android 14+ updates the system roots through the Conscrypt APEX. The
    // rustls-native-certs generic Unix probe otherwise searches Termux paths.
    for directory in [
        "/apex/com.android.conscrypt/cacerts",
        "/system/etc/security/cacerts",
    ] {
        let path = std::path::Path::new(directory);
        if !path.is_dir() {
            continue;
        }
        let result = rustls_native_certs::load_certs_from_paths(None, Some(path));
        if !result.certs.is_empty() {
            return Ok(result.certs);
        }
    }
    Err(
        "TLS cannot load Android system certificates; update the device's security components"
            .into(),
    )
}

#[cfg(all(test, any(target_os = "ios", target_os = "macos")))]
mod apple_tests {
    use super::*;
    use rustls::client::danger::ServerCertVerifier;
    use rustls::pki_types::{pem::PemObject, CertificateDer, ServerName, UnixTime};
    use rustls_platform_verifier::Verifier;

    fn certificate() -> CertificateDer<'static> {
        CertificateDer::from_pem_slice(include_bytes!("testdata/apple-tls-server.pem")).unwrap()
    }

    fn verification_time() -> UnixTime {
        // Fixed within the leaf's 2026-09-01 .. 2027-03-01 validity period.
        UnixTime::since_unix_epoch(std::time::Duration::from_secs(1_790_726_400))
    }

    fn fixture_verifier() -> Verifier {
        // Trust this disposable fixture in this verifier instance only; no OS
        // trust store changes and no extra anchors in the production builder.
        Verifier::new_with_extra_roots(
            [
                CertificateDer::from_pem_slice(include_bytes!("testdata/apple-tls-root.pem"))
                    .unwrap(),
            ],
            Arc::new(rustls::crypto::aws_lc_rs::default_provider()),
        )
        .unwrap()
    }

    #[test]
    fn apple_verifier_accepts_trusted_matching_peer() {
        let result = fixture_verifier().verify_server_cert(
            &certificate(),
            &[],
            &ServerName::try_from("tls-fixture.invalid").unwrap(),
            &[],
            verification_time(),
        );
        assert!(result.is_ok(), "unexpected verification result: {result:?}");
    }

    #[test]
    fn apple_verifier_rejects_wrong_hostname_with_trusted_certificate() {
        let result = fixture_verifier().verify_server_cert(
            &certificate(),
            &[],
            &ServerName::try_from("wrong.invalid").unwrap(),
            &[],
            verification_time(),
        );
        assert!(
            matches!(
                result,
                Err(rustls::Error::InvalidCertificate(
                    rustls::CertificateError::NotValidForName
                ))
            ),
            "unexpected verification result: {result:?}"
        );
    }

    #[test]
    fn apple_verifier_rejects_certificate_before_its_validity_period() {
        let before_fixture_issuance = UnixTime::since_unix_epoch(std::time::Duration::from_secs(
            1_767_225_600, // 2026-01-01, leaf starts 2026-09-01.
        ));
        assert!(fixture_verifier()
            .verify_server_cert(
                &certificate(),
                &[],
                &ServerName::try_from("tls-fixture.invalid").unwrap(),
                &[],
                before_fixture_issuance,
            )
            .is_err());
    }

    #[test]
    fn apple_verifier_rejects_malformed_certificate() {
        assert!(fixture_verifier()
            .verify_server_cert(
                &CertificateDer::from(vec![0]),
                &[],
                &ServerName::try_from("tls-fixture.invalid").unwrap(),
                &[],
                verification_time(),
            )
            .is_err());
    }

    #[test]
    fn apple_verifier_rejects_expired_leaf() {
        let after_leaf_expiry = UnixTime::since_unix_epoch(std::time::Duration::from_secs(
            1_830_297_600, // 2028-01-01, leaf ends 2027-03-01; CA still valid.
        ));
        assert!(fixture_verifier()
            .verify_server_cert(
                &certificate(),
                &[],
                &ServerName::try_from("tls-fixture.invalid").unwrap(),
                &[],
                after_leaf_expiry,
            )
            .is_err());
    }

    #[test]
    fn apple_system_verifier_rejects_unknown_issuer() {
        let verifier =
            Verifier::new(Arc::new(rustls::crypto::aws_lc_rs::default_provider())).unwrap();
        let result = verifier.verify_server_cert(
            &certificate(),
            &[],
            &ServerName::try_from("tls-fixture.invalid").unwrap(),
            &[],
            verification_time(),
        );
        assert!(
            matches!(result, Err(rustls::Error::InvalidCertificate(_))),
            "unexpected verification result: {result:?}"
        );
    }
}
