// Copyright 2026 Oleg Tsarev
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

//! The certificate this listener serves, and the ability to change it while
//! it is serving.
//!
//! The acceptor used to hold the pair directly: `set_certificate` and
//! `set_private_key` on the `SslAcceptorBuilder`, once, at startup. That is
//! why installing a certificate meant restarting the daemon, and why the
//! client-CA endpoint answers `reload_required` to this day.
//!
//! A restart is a poor answer for a BMC. It drops every session, including the
//! one that asked, so the caller sees a dropped connection rather than a
//! result, and on a board reached only through its own interface that reads as
//! "the install broke it".
//!
//! So the pair lives here instead, behind a lock, and the acceptor reaches for
//! it on each new connection through OpenSSL's servername callback. Replacing
//! it is a write to this store: connections already up keep the certificate
//! they negotiated, and the next one gets the new one.
//!
//! ## Why the servername callback
//!
//! It is SNI's hook, but OpenSSL runs it once per handshake whether or not the
//! client sent a server name -- it is the "final" handler for that extension,
//! and final handlers run for extensions that were absent too. That matters
//! here more than usual: a BMC is normally reached by address, and a client
//! connecting to an address sends no SNI at all. `serves_the_current_pair_to_a_client_without_sni`
//! is the test that keeps this true.

use crate::api::metrics;
use openssl::pkey::{PKey, Private};
use openssl::x509::X509;
use std::sync::{Arc, RwLock};

/// The marker the board's own generator signs with.
///
/// `generate_self_signedx509.sh` decides whether a certificate is its own to
/// replace by looking for this in the issuer, and refuses to touch anything
/// else. The same test decides what this daemon calls the certificate, so the
/// two halves cannot disagree about which certificates are the board's own and
/// which are the operator's.
pub const SELF_SIGNED_ISSUER: &str = "Turing Pi BMC self-signed";

/// Where a certificate came from, as `/metrics` and the interface report it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// Issued on the board by `generate_self_signedx509.sh`.
    SelfSigned,
    /// Somebody put it there: through this API, or by hand over SSH.
    Installed,
}

impl Source {
    /// The label value. Stable, because alert rules will match on it.
    pub fn as_str(self) -> &'static str {
        match self {
            Source::SelfSigned => "self-signed",
            Source::Installed => "installed",
        }
    }

    /// Read from the issuer, never from a record of how the file was written.
    ///
    /// A file written by this API is not necessarily an operator's
    /// certificate -- `DELETE` writes a self-signed one through the same path
    /// -- and a certificate placed by hand over SSH was never seen by this
    /// API at all. The certificate itself is the only witness that is right in
    /// both cases.
    pub fn of(cert: &X509) -> Source {
        let issuer = cert
            .issuer_name()
            .entries()
            .filter_map(|e| e.data().to_string().ok())
            .collect::<Vec<_>>()
            .join(" ");

        if issuer.contains(SELF_SIGNED_ISSUER) {
            Source::SelfSigned
        } else {
            Source::Installed
        }
    }
}

/// One certificate, its key, and whatever chain came with it.
pub struct Served {
    pub key: PKey<Private>,
    pub leaf: X509,
    /// Intermediates, leaf excluded. Sent so a client that trusts only the
    /// root can build a path; empty for a self-signed certificate.
    pub chain: Vec<X509>,
    pub source: Source,
}

impl Served {
    /// What `/metrics` says about this pair.
    pub fn facts(&self) -> metrics::Certificate {
        metrics::Certificate {
            expires_unix: crate::certificate_expiry_unix(&self.leaf),
            key: self
                .leaf
                .public_key()
                .ok()
                .as_ref()
                .map(crate::describe_key),
            source: Some(self.source.as_str().to_string()),
        }
    }
}

/// The pair every new connection is given, swappable while the listener runs.
#[derive(Clone)]
pub struct ServedCertificate(Arc<RwLock<Arc<Served>>>);

impl ServedCertificate {
    pub fn new(served: Served) -> Self {
        ServedCertificate(Arc::new(RwLock::new(Arc::new(served))))
    }

    /// The pair to serve now.
    ///
    /// A poisoned lock cannot happen here -- nothing panics while holding it,
    /// the guards below are the only writers, and both do their work before
    /// taking the lock -- but if it ever did, refusing to serve TLS would take
    /// the board off the network entirely. `into_inner` keeps serving the last
    /// good pair instead, which is the same certificate the connection before
    /// the panic got.
    pub fn current(&self) -> Arc<Served> {
        match self.0.read() {
            Ok(guard) => guard.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }

    /// Serve a different pair from the next connection onward.
    pub fn replace(&self, served: Served) {
        let served = Arc::new(served);
        match self.0.write() {
            Ok(mut guard) => *guard = served,
            Err(poisoned) => *poisoned.into_inner() = served,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use openssl::asn1::Asn1Time;
    use openssl::ec::{EcGroup, EcKey};
    use openssl::hash::MessageDigest;
    use openssl::nid::Nid;
    use openssl::x509::X509Builder;

    /// A certificate with `cn` as both subject and issuer.
    fn signed_by(cn: &str) -> (PKey<Private>, X509) {
        let group = EcGroup::from_curve_name(Nid::SECP384R1).unwrap();
        let key = PKey::from_ec_key(EcKey::generate(&group).unwrap()).unwrap();

        let mut name = openssl::x509::X509NameBuilder::new().unwrap();
        name.append_entry_by_nid(Nid::COMMONNAME, cn).unwrap();
        let name = name.build();

        let mut builder = X509Builder::new().unwrap();
        builder.set_version(2).unwrap();
        builder.set_subject_name(&name).unwrap();
        builder.set_issuer_name(&name).unwrap();
        builder.set_pubkey(&key).unwrap();
        builder
            .set_not_before(&Asn1Time::days_from_now(0).unwrap())
            .unwrap();
        builder
            .set_not_after(&Asn1Time::days_from_now(30).unwrap())
            .unwrap();
        builder.sign(&key, MessageDigest::sha256()).unwrap();
        (key, builder.build())
    }

    /// The board's own generator uses exactly this issuer, and this is the
    /// only thing that tells the two kinds of certificate apart.
    #[test]
    fn the_generators_own_issuer_reads_as_self_signed() {
        let (_, cert) = signed_by(SELF_SIGNED_ISSUER);
        assert_eq!(Source::of(&cert), Source::SelfSigned);
    }

    /// Anything else is somebody's real certificate, including another
    /// self-signed one. "Self-signed" here means "issued by this board", not
    /// "not issued by a CA" -- the distinction that matters is whether the
    /// board may replace it.
    #[test]
    fn another_self_signed_certificate_is_not_ours() {
        let (_, cert) = signed_by("some other self-signed thing");
        assert_eq!(Source::of(&cert), Source::Installed);
    }

    #[test]
    fn replacing_changes_what_current_hands_out() {
        let (key, cert) = signed_by(SELF_SIGNED_ISSUER);
        let store = ServedCertificate::new(Served {
            key,
            leaf: cert,
            chain: Vec::new(),
            source: Source::SelfSigned,
        });
        assert_eq!(store.current().source, Source::SelfSigned);

        let (key, cert) = signed_by("CN=a real CA");
        store.replace(Served {
            key,
            leaf: cert,
            chain: Vec::new(),
            source: Source::Installed,
        });
        assert_eq!(store.current().source, Source::Installed);
        assert_eq!(store.current().facts().source.as_deref(), Some("installed"));
    }
}
