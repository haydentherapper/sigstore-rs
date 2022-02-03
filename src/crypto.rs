//
// Copyright 2021 The Sigstore Authors.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use ecdsa::signature::Verifier;
use ecdsa::{Signature, VerifyingKey};
use p256::pkcs8::FromPublicKey;
use x509_parser::{
    certificate::X509Certificate, parse_x509_certificate, pem::parse_x509_pem, prelude::ASN1Time,
    x509::SubjectPublicKeyInfo,
};

use crate::errors::{Result, SigstoreError};

pub(crate) type CosignVerificationKey = VerifyingKey<p256::NistP256>;

/// Create a new Cosign Verification Key starting from the contents of
/// a cosign public key.
pub(crate) fn new_verification_key(contents: &str) -> Result<CosignVerificationKey> {
    VerifyingKey::<p256::NistP256>::from_public_key_pem(contents).map_err(|e| {
        SigstoreError::InvalidKeyFormat {
            error: e.to_string(),
        }
    })
}

/// Create a new Cosign Verification Key starting from ASN.1 DER-encoded
/// SubjectPublicKeyInfo (binary format).
pub(crate) fn new_verification_key_from_public_key_der(
    bytes: &[u8],
) -> Result<CosignVerificationKey> {
    VerifyingKey::<p256::NistP256>::from_public_key_der(bytes).map_err(|e| {
        SigstoreError::InvalidKeyFormat {
            error: e.to_string(),
        }
    })
}

/// Extract the public key stored inside of the given PEM-encoded certificate
///
/// Returns the DER key as a list of bytes
pub(crate) fn extract_public_key_from_pem_cert(cert: &[u8]) -> Result<Vec<u8>> {
    let (_, pem) = parse_x509_pem(cert)?;
    let (_, res_x509) = parse_x509_certificate(&pem.contents)?;

    Ok(res_x509.public_key().raw.to_owned())
}

/// Verify the signature provided has been actually generated by the given key against the
/// when signing the provided message.
pub(crate) fn verify_signature(
    verification_key: &CosignVerificationKey,
    signature_str: &str,
    msg: &[u8],
) -> Result<()> {
    let signature_raw = base64::decode(signature_str)?;
    let signature = Signature::<p256::NistP256>::from_der(&signature_raw)?;
    verification_key.verify(msg, &signature)?;
    Ok(())
}

/// Ensure the given certificate can be trusted for verifying cosign
/// signatures.
///
/// The following checks are performed against the given certificate:
/// * The certificate has been issued by the CA with the given SubjectPublicKeyInfo
/// * The certificate has the right set of key usages
/// * The certificate cannot be used before the current time
pub(crate) fn verify_certificate_can_be_trusted(
    certificate: &X509Certificate,
    ca_issuer_public_key: &SubjectPublicKeyInfo,
    integrated_time: i64,
) -> Result<()> {
    verify_issuer(certificate, ca_issuer_public_key)?;
    verify_certificate_key_usages(certificate)?;
    verify_certificate_has_san(certificate)?;
    verify_certificate_validity(certificate)?;
    verify_certificate_expiration(certificate, integrated_time)?;

    Ok(())
}

fn verify_issuer(
    certificate: &X509Certificate,
    ca_issuer_public_key: &SubjectPublicKeyInfo,
) -> Result<()> {
    certificate.verify_signature(Some(ca_issuer_public_key))?;
    Ok(())
}

fn verify_certificate_key_usages(certificate: &X509Certificate) -> Result<()> {
    let (_critical, key_usage) = certificate
        .tbs_certificate
        .key_usage()
        .ok_or(SigstoreError::CertificateWithoutDigitalSignatureKeyUsage)?;
    if !key_usage.digital_signature() {
        return Err(SigstoreError::CertificateWithoutDigitalSignatureKeyUsage);
    }

    let (_critical, ext_key_usage) = certificate
        .tbs_certificate
        .extended_key_usage()
        .ok_or(SigstoreError::CertificateWithoutCodeSigningKeyUsage)?;
    if !ext_key_usage.code_signing {
        return Err(SigstoreError::CertificateWithoutCodeSigningKeyUsage);
    }

    Ok(())
}

fn verify_certificate_has_san(certificate: &X509Certificate) -> Result<()> {
    let (_critical, _subject_alternative_name) = certificate
        .tbs_certificate
        .subject_alternative_name()
        .ok_or(SigstoreError::CertificateWithoutSubjectAlternativeName)?;
    Ok(())
}

fn verify_certificate_validity(certificate: &X509Certificate) -> Result<()> {
    // Comment taken from cosign verification code:
    // THIS IS IMPORTANT: WE DO NOT CHECK TIMES HERE
    // THE CERTIFICATE IS TREATED AS TRUSTED FOREVER
    // WE CHECK THAT THE SIGNATURES WERE CREATED DURING THIS WINDOW
    let validity = certificate.validity();
    let now = ASN1Time::now();
    if now < validity.not_before {
        Err(SigstoreError::CertificateValidityError(
            validity.not_before.to_rfc2822(),
        ))
    } else {
        Ok(())
    }
}

fn verify_certificate_expiration(
    certificate: &X509Certificate,
    integrated_time: i64,
) -> Result<()> {
    let it = ASN1Time::from_timestamp(integrated_time);
    let validity = certificate.validity();

    if it < validity.not_before {
        return Err(
            SigstoreError::CertificateExpiredBeforeSignaturesSubmittedToRekor {
                integrated_time: it.to_rfc2822(),
                not_before: validity.not_before.to_rfc2822(),
            },
        );
    }

    if it > validity.not_after {
        return Err(
            SigstoreError::CertificateIssuedAfterSignaturesSubmittedToRekor {
                integrated_time: it.to_rfc2822(),
                not_after: validity.not_after.to_rfc2822(),
            },
        );
    }

    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use chrono::{DateTime, Duration, Utc};
    use openssl::asn1::{Asn1Integer, Asn1Time};
    use openssl::bn::{BigNum, MsbOption};
    use openssl::conf::{Conf, ConfMethod};
    use openssl::ec::{EcGroup, EcKey};
    use openssl::hash::MessageDigest;
    use openssl::nid::Nid;
    use openssl::pkey;
    use openssl::x509::extension::{
        AuthorityKeyIdentifier, BasicConstraints, ExtendedKeyUsage, KeyUsage,
        SubjectAlternativeName, SubjectKeyIdentifier,
    };
    use openssl::x509::{X509Extension, X509NameBuilder, X509};
    use x509_parser::traits::FromDer;

    const PUBLIC_KEY: &str = r#"-----BEGIN PUBLIC KEY-----
MFkwEwYHKoZIzj0CAQYIKoZIzj0DAQcDQgAENptdY/l3nB0yqkXLBWkZWQwo6+cu
OSWS1X9vPavpiQOoTTGC0xX57OojUadxF1cdQmrsiReWg2Wn4FneJfa8xw==
-----END PUBLIC KEY-----"#;

    pub(crate) struct CertData {
        pub cert: X509,
        pub private_key: EcKey<pkey::Private>,
    }

    pub(crate) struct CertGenerationOptions {
        pub digital_signature_key_usage: bool,
        pub code_signing_extended_key_usage: bool,
        pub subject_email: Option<String>,
        pub subject_url: Option<String>,
        //TODO: remove macro once https://github.com/sfackler/rust-openssl/issues/1411
        //is fixed
        #[allow(dead_code)]
        pub subject_issuer: Option<String>,
        pub not_before: DateTime<chrono::Utc>,
        pub not_after: DateTime<chrono::Utc>,
    }

    impl Default for CertGenerationOptions {
        fn default() -> Self {
            let not_before = Utc::now().checked_sub_signed(Duration::days(1)).unwrap();
            let not_after = Utc::now().checked_add_signed(Duration::days(1)).unwrap();

            CertGenerationOptions {
                digital_signature_key_usage: true,
                code_signing_extended_key_usage: true,
                subject_email: Some(String::from("tests@sigstore-rs.dev")),
                subject_issuer: Some(String::from("https://sigstore.dev/oauth")),
                subject_url: None,
                not_before,
                not_after,
            }
        }
    }

    pub(crate) fn generate_certificate(
        issuer: Option<&CertData>,
        settings: CertGenerationOptions,
    ) -> anyhow::Result<CertData> {
        // Sigstore relies on NIST P-256
        // NIST P-256 is a Weierstrass curve specified in FIPS 186-4: Digital Signature Standard (DSS):
        // https://nvlpubs.nist.gov/nistpubs/FIPS/NIST.FIPS.186-4.pdf
        // Also known as prime256v1 (ANSI X9.62) and secp256r1 (SECG)
        let group = EcGroup::from_curve_name(Nid::X9_62_PRIME256V1).expect("Cannot create EcGroup");
        let private_key = EcKey::generate(&group).expect("Cannot create private key");
        let public_key = private_key.public_key();

        let ec_pub_key =
            EcKey::from_public_key(&group, &public_key).expect("Cannot create ec pub key");
        let pkey = pkey::PKey::from_ec_key(ec_pub_key).expect("Cannot create pkey");

        let mut x509_name_builder = X509NameBuilder::new()?;
        x509_name_builder.append_entry_by_text("O", "tests")?;
        x509_name_builder.append_entry_by_text("CN", "sigstore.test")?;
        let x509_name = x509_name_builder.build();

        let mut x509_builder = openssl::x509::X509::builder()?;
        x509_builder.set_subject_name(&x509_name)?;
        x509_builder
            .set_pubkey(&pkey)
            .expect("Cannot set public key");

        // set serial number
        let mut big = BigNum::new().expect("Cannot create BigNum");
        big.rand(152, MsbOption::MAYBE_ZERO, true)?;
        let serial_number = Asn1Integer::from_bn(&big)?;
        x509_builder.set_serial_number(&serial_number)?;

        // set version 3
        x509_builder.set_version(2)?;

        // x509 v3 extensions
        let conf = Conf::new(ConfMethod::default())?;
        let x509v3_context = match issuer {
            Some(issuer_data) => x509_builder.x509v3_context(Some(&issuer_data.cert), Some(&conf)),
            None => x509_builder.x509v3_context(None, Some(&conf)),
        };

        let mut extensions: Vec<X509Extension> = Vec::new();

        let x509_extension_subject_key_identifier =
            SubjectKeyIdentifier::new().build(&x509v3_context)?;
        extensions.push(x509_extension_subject_key_identifier);

        // CA usage
        if issuer.is_none() {
            // CA usage
            let x509_basic_constraint_ca =
                BasicConstraints::new().critical().ca().pathlen(1).build()?;
            extensions.push(x509_basic_constraint_ca);
        } else {
            let x509_basic_constraint_ca = BasicConstraints::new().critical().build()?;
            extensions.push(x509_basic_constraint_ca);
        }

        // set key usage
        if issuer.is_some() {
            if settings.digital_signature_key_usage {
                let key_usage = KeyUsage::new().critical().digital_signature().build()?;
                extensions.push(key_usage);
            }

            if settings.code_signing_extended_key_usage {
                let extended_key_usage = ExtendedKeyUsage::new().code_signing().build()?;
                extensions.push(extended_key_usage);
            }
        } else {
            let key_usage = KeyUsage::new()
                .critical()
                .crl_sign()
                .key_cert_sign()
                .build()?;
            extensions.push(key_usage);
        }

        // extensions that diverge, based on whether we're creating the CA or
        // a certificate issued by it
        if issuer.is_none() {
        } else {
            let x509_extension_authority_key_identifier = AuthorityKeyIdentifier::new()
                .keyid(true)
                .build(&x509v3_context)?;
            extensions.push(x509_extension_authority_key_identifier);

            if settings.subject_email.is_some() && settings.subject_url.is_some() {
                panic!(
                    "cosign doesn't generate certificates with a SAN that has both email and url"
                );
            }
            if let Some(email) = settings.subject_email {
                let x509_extension_san = SubjectAlternativeName::new()
                    .critical()
                    .email(&email)
                    .build(&x509v3_context)?;

                extensions.push(x509_extension_san);
            };
            if let Some(url) = settings.subject_url {
                let x509_extension_san = SubjectAlternativeName::new()
                    .critical()
                    .uri(&url)
                    .build(&x509v3_context)?;

                extensions.push(x509_extension_san);
            }
            //
            //TODO: uncomment once https://github.com/sfackler/rust-openssl/issues/1411
            //is fixed
            //if let Some(subject_issuer) = settings.subject_issuer {
            //    let sigstore_issuer_asn1_obj = Asn1Object::from_str("1.3.6.1.4.1.57264.1.1")?; //&SIGSTORE_ISSUER_OID.to_string())?;

            //    let value = format!("ASN1:UTF8String:{}", subject_issuer);

            //    let sigstore_subject_issuer_extension = X509Extension::new_nid(
            //        None,
            //        Some(&x509v3_context),
            //        sigstore_issuer_asn1_obj.nid(),
            //        //&subject_issuer,
            //        &value,
            //    )?;

            //    extensions.push(sigstore_subject_issuer_extension);
            //}
        }

        for ext in extensions {
            x509_builder.append_extension(ext)?;
        }

        // setup validity
        let not_before = Asn1Time::from_unix(settings.not_before.timestamp())?;
        let not_after = Asn1Time::from_unix(settings.not_after.timestamp())?;
        x509_builder.set_not_after(&not_after)?;
        x509_builder.set_not_before(&not_before)?;

        // set issuer
        if let Some(issuer_data) = issuer {
            let issuer_name = issuer_data.cert.subject_name();
            x509_builder.set_issuer_name(&issuer_name)?;
        } else {
            // self signed cert
            x509_builder.set_issuer_name(&x509_name)?;
        }

        // sign the cert
        let issuer_key = match issuer {
            Some(issuer_data) => issuer_data.private_key.clone(),
            None => private_key.clone(),
        };
        let issuer_pkey = pkey::PKey::from_ec_key(issuer_key).expect("Cannot create signer pkey");
        x509_builder
            .sign(&issuer_pkey, MessageDigest::sha256())
            .expect("Cannot sign certificate");

        let x509 = x509_builder.build();

        Ok(CertData {
            cert: x509,
            private_key,
        })
    }

    #[test]
    fn verify_cert_issuer_success() -> anyhow::Result<()> {
        let ca_data = generate_certificate(None, CertGenerationOptions::default())?;
        let ca_public_key_der = ca_data.private_key.public_key_to_der()?;
        let (_, spki) = SubjectPublicKeyInfo::from_der(&ca_public_key_der)?;

        let issued_cert = generate_certificate(Some(&ca_data), CertGenerationOptions::default())?;
        let issued_cert_pem = issued_cert.cert.to_pem()?;
        let (_, pem) = x509_parser::pem::parse_x509_pem(&issued_cert_pem)?;
        let (_, cert) = x509_parser::parse_x509_certificate(&pem.contents)?;

        assert!(verify_issuer(&cert, &spki).is_ok());

        Ok(())
    }

    #[test]
    fn verify_cert_issuer_failure() -> anyhow::Result<()> {
        let ca_data = generate_certificate(None, CertGenerationOptions::default())?;

        let issued_cert = generate_certificate(Some(&ca_data), CertGenerationOptions::default())?;
        let issued_cert_pem = issued_cert.cert.to_pem()?;
        let (_, pem) = x509_parser::pem::parse_x509_pem(&issued_cert_pem)?;
        let (_, cert) = x509_parser::parse_x509_certificate(&pem.contents)?;

        let another_ca_data = generate_certificate(None, CertGenerationOptions::default())?;
        let wrong_ca_public_key_der = another_ca_data.private_key.public_key_to_der()?;
        let (_, spki) = SubjectPublicKeyInfo::from_der(&wrong_ca_public_key_der)?;

        let err = verify_issuer(&cert, &spki).expect_err("Was expecting an error");
        let found = match err {
            SigstoreError::X509Error(_) => true,
            _ => false,
        };
        assert!(found, "Didn't get expected error, got {:?} instead", err);

        Ok(())
    }

    #[test]
    fn verify_cert_key_usages_success() -> anyhow::Result<()> {
        let ca_data = generate_certificate(None, CertGenerationOptions::default())?;

        let issued_cert = generate_certificate(Some(&ca_data), CertGenerationOptions::default())?;
        let issued_cert_pem = issued_cert.cert.to_pem()?;
        let (_, pem) = x509_parser::pem::parse_x509_pem(&issued_cert_pem)?;
        let (_, cert) = x509_parser::parse_x509_certificate(&pem.contents)?;

        assert!(verify_certificate_key_usages(&cert).is_ok());

        Ok(())
    }

    #[test]
    fn verify_cert_key_usages_failure_because_no_digital_signature() -> anyhow::Result<()> {
        let ca_data = generate_certificate(None, CertGenerationOptions::default())?;

        let issued_cert = generate_certificate(
            Some(&ca_data),
            CertGenerationOptions {
                digital_signature_key_usage: false,
                ..Default::default()
            },
        )?;
        let issued_cert_pem = issued_cert.cert.to_pem()?;
        let (_, pem) = x509_parser::pem::parse_x509_pem(&issued_cert_pem)?;
        let (_, cert) = x509_parser::parse_x509_certificate(&pem.contents)?;

        let err =
            verify_certificate_key_usages(&cert).expect_err("Was supposed to return an error");
        let found = match err {
            SigstoreError::CertificateWithoutDigitalSignatureKeyUsage => true,
            _ => false,
        };
        assert!(found, "Didn't get expected error, got {:?} instead", err);

        Ok(())
    }

    #[test]
    fn verify_cert_key_usages_failure_because_no_code_signing() -> anyhow::Result<()> {
        let ca_data = generate_certificate(None, CertGenerationOptions::default())?;

        let issued_cert = generate_certificate(
            Some(&ca_data),
            CertGenerationOptions {
                code_signing_extended_key_usage: false,
                ..Default::default()
            },
        )?;
        let issued_cert_pem = issued_cert.cert.to_pem()?;
        let (_, pem) = x509_parser::pem::parse_x509_pem(&issued_cert_pem)?;
        let (_, cert) = x509_parser::parse_x509_certificate(&pem.contents)?;

        let err =
            verify_certificate_key_usages(&cert).expect_err("Was supposed to return an error");
        let found = match err {
            SigstoreError::CertificateWithoutCodeSigningKeyUsage => true,
            _ => false,
        };
        assert!(found, "Didn't get expected error, got {:?} instead", err);

        Ok(())
    }

    #[test]
    fn verify_cert_failure_because_no_san() -> anyhow::Result<()> {
        let ca_data = generate_certificate(None, CertGenerationOptions::default())?;

        let issued_cert = generate_certificate(
            Some(&ca_data),
            CertGenerationOptions {
                subject_email: None,
                subject_url: None,
                ..Default::default()
            },
        )?;
        let issued_cert_pem = issued_cert.cert.to_pem()?;
        let (_, pem) = x509_parser::pem::parse_x509_pem(&issued_cert_pem)?;
        let (_, cert) = x509_parser::parse_x509_certificate(&pem.contents)?;

        let error = verify_certificate_has_san(&cert).expect_err("Didn'g get an error");
        let found = match error {
            SigstoreError::CertificateWithoutSubjectAlternativeName => true,
            _ => false,
        };
        assert!(found, "Didn't get the expected error: {}", error);

        Ok(())
    }

    #[test]
    fn verify_cert_validity_success() -> anyhow::Result<()> {
        let ca_data = generate_certificate(None, CertGenerationOptions::default())?;

        let issued_cert = generate_certificate(Some(&ca_data), CertGenerationOptions::default())?;
        let issued_cert_pem = issued_cert.cert.to_pem()?;
        let (_, pem) = x509_parser::pem::parse_x509_pem(&issued_cert_pem)?;
        let (_, cert) = x509_parser::parse_x509_certificate(&pem.contents)?;

        assert!(verify_certificate_validity(&cert).is_ok());

        Ok(())
    }

    #[test]
    fn verify_cert_validity_failure() -> anyhow::Result<()> {
        let ca_data = generate_certificate(None, CertGenerationOptions::default())?;

        let issued_cert = generate_certificate(
            Some(&ca_data),
            CertGenerationOptions {
                not_before: Utc::now().checked_add_signed(Duration::days(5)).unwrap(),
                not_after: Utc::now().checked_add_signed(Duration::days(6)).unwrap(),
                ..Default::default()
            },
        )?;
        let issued_cert_pem = issued_cert.cert.to_pem()?;
        let (_, pem) = x509_parser::pem::parse_x509_pem(&issued_cert_pem)?;
        let (_, cert) = x509_parser::parse_x509_certificate(&pem.contents)?;

        let err = verify_certificate_validity(&cert).expect_err("Was expecting an error");
        let found = match err {
            SigstoreError::CertificateValidityError(_) => true,
            _ => false,
        };
        assert!(found, "Didn't get expected error, got {:?} instead", err);

        Ok(())
    }

    #[test]
    fn verify_cert_expiration_success() -> anyhow::Result<()> {
        let ca_data = generate_certificate(None, CertGenerationOptions::default())?;

        let integrated_time = Utc::now();

        let issued_cert = generate_certificate(
            Some(&ca_data),
            CertGenerationOptions {
                not_before: Utc::now().checked_sub_signed(Duration::days(1)).unwrap(),
                not_after: Utc::now().checked_add_signed(Duration::days(1)).unwrap(),
                ..Default::default()
            },
        )?;
        let issued_cert_pem = issued_cert.cert.to_pem()?;
        let (_, pem) = x509_parser::pem::parse_x509_pem(&issued_cert_pem)?;
        let (_, cert) = x509_parser::parse_x509_certificate(&pem.contents)?;

        assert!(verify_certificate_expiration(&cert, integrated_time.timestamp(),).is_ok());

        Ok(())
    }

    #[test]
    fn verify_cert_expiration_failure() -> anyhow::Result<()> {
        let ca_data = generate_certificate(None, CertGenerationOptions::default())?;

        let integrated_time = Utc::now().checked_add_signed(Duration::days(5)).unwrap();

        let issued_cert = generate_certificate(
            Some(&ca_data),
            CertGenerationOptions {
                not_before: Utc::now().checked_sub_signed(Duration::days(1)).unwrap(),
                not_after: Utc::now().checked_add_signed(Duration::days(1)).unwrap(),
                ..Default::default()
            },
        )?;
        let issued_cert_pem = issued_cert.cert.to_pem().unwrap();
        let (_, pem) = x509_parser::pem::parse_x509_pem(&issued_cert_pem)?;
        let (_, cert) = x509_parser::parse_x509_certificate(&pem.contents)?;

        let err = verify_certificate_expiration(&cert, integrated_time.timestamp())
            .expect_err("Was expecting an error");
        let found = match err {
            SigstoreError::CertificateIssuedAfterSignaturesSubmittedToRekor {
                integrated_time: _,
                not_after: _,
            } => true,
            _ => false,
        };
        assert!(found, "Didn't get expected error, got {:?} instead", err);

        Ok(())
    }

    #[test]
    fn verify_signature_success() {
        let signature = String::from("MEUCIQD6q/COgzOyW0YH1Dk+CCYSt4uAhm3FDHUwvPI55zwnlwIgE0ZK58ZOWpZw8YVmBapJhBqCfdPekIknimuO0xH8Jh8=");
        let verification_key = new_verification_key(PUBLIC_KEY).unwrap();
        let msg = r#"{"critical":{"identity":{"docker-reference":"registry-testing.svc.lan/busybox"},"image":{"docker-manifest-digest":"sha256:f3cfc9d0dbf931d3db4685ec659b7ac68e2a578219da4aae65427886e649b06b"},"type":"cosign container image signature"},"optional":null}"#;

        let outcome = verify_signature(&verification_key, &signature, &msg.as_bytes());
        assert!(outcome.is_ok());
    }

    #[test]
    fn verify_signature_failure_because_wrong_msg() {
        let signature = String::from("MEUCIQD6q/COgzOyW0YH1Dk+CCYSt4uAhm3FDHUwvPI55zwnlwIgE0ZK58ZOWpZw8YVmBapJhBqCfdPekIknimuO0xH8Jh8=");
        let verification_key = new_verification_key(PUBLIC_KEY).unwrap();
        let msg = "hello world";

        let err = verify_signature(&verification_key, &signature, &msg.as_bytes())
            .expect_err("Was expecting an error");
        let found = match err {
            SigstoreError::EcdsaError(_) => true,
            _ => false,
        };
        assert!(found, "Didn't get expected error, got {:?} instead", err);
    }

    #[test]
    fn verify_signature_failure_because_wrong_signature() {
        let signature = String::from("this is a signature");
        let verification_key = new_verification_key(PUBLIC_KEY).unwrap();
        let msg = r#"{"critical":{"identity":{"docker-reference":"registry-testing.svc.lan/busybox"},"image":{"docker-manifest-digest":"sha256:f3cfc9d0dbf931d3db4685ec659b7ac68e2a578219da4aae65427886e649b06b"},"type":"cosign container image signature"},"optional":null}"#;

        let err = verify_signature(&verification_key, &signature, &msg.as_bytes())
            .expect_err("Was expecting an error");
        let found = match err {
            SigstoreError::Base64DecodeError(_) => true,
            _ => false,
        };
        assert!(found, "Didn't get expected error, got {:?} instead", err);
    }

    #[test]
    fn verify_signature_failure_because_wrong_verification_key() {
        let signature = String::from("MEUCIQD6q/COgzOyW0YH1Dk+CCYSt4uAhm3FDHUwvPI55zwnlwIgE0ZK58ZOWpZw8YVmBapJhBqCfdPekIknimuO0xH8Jh8=");

        let verification_key = new_verification_key(
            r#"-----BEGIN PUBLIC KEY-----
MFkwEwYHKoZIzj0CAQYIKoZIzj0DAQcDQgAETJP9cqpUQsn2ggmJniWGjHdlsHzD
JsB89BPhZYch0U0hKANx5TY+ncrm0s8bfJxxHoenAEFhwhuXeb4PqIrtoQ==
-----END PUBLIC KEY-----"#,
        )
        .unwrap();
        let msg = r#"{"critical":{"identity":{"docker-reference":"registry-testing.svc.lan/busybox"},"image":{"docker-manifest-digest":"sha256:f3cfc9d0dbf931d3db4685ec659b7ac68e2a578219da4aae65427886e649b06b"},"type":"cosign container image signature"},"optional":null}"#;

        let err = verify_signature(&verification_key, &signature, &msg.as_bytes())
            .expect_err("Was expecting an error");
        let found = match err {
            SigstoreError::EcdsaError(_) => true,
            _ => false,
        };
        assert!(found, "Didn't get expected error, got {:?} instead", err);
    }
}
