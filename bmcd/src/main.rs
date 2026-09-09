// Copyright 2023 Turing Machines
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
#![deny(clippy::mod_module_files)]
mod api;
mod app;
mod authentication;
mod config;
mod hal;
mod persistency;
mod serial_service;
mod streaming_data_service;
mod usb_boot;
mod utils;

use crate::api::paths;
use crate::config::Config;
use crate::serial_service::{serial::SerialConnections, serial_config};
use crate::{
    api::legacy, api::metrics, authentication::linux_authenticator::LinuxAuthenticator,
    streaming_data_service::StreamingDataService,
};
use actix_files::{Files, NamedFile};
use actix_http::HttpService;
use actix_service::map_config;
use actix_web::{
    dev::{AppConfig, Server},
    http::{self, KeepAlive},
    web::{self, Data},
    App, HttpRequest, HttpResponse, HttpServer,
};
use anyhow::Context;
use app::{bmc_application::BmcApplication, event_application::run_event_listener};
use clap::{command, value_parser, Arg, ArgAction};
use config::Log;
use futures::future::join_all;
use openssl::{
    pkey::{PKey, Private},
    ssl::{select_next_proto, AlpnError, SslAcceptor, SslMethod},
    x509::X509,
};
use std::{
    fs::OpenOptions,
    io::Read,
    os::unix::net::UnixDatagram,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use tracing_appender::{non_blocking::WorkerGuard, rolling::Rotation};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter, Layer};

const HTTP_PORT: u16 = 80;

#[actix_web::main]
async fn main() -> anyhow::Result<()> {
    let arguments = arguments();

    // Printing the API document needs neither a config file nor a board, and
    // that is the point: the docs site has to render this spec, and the only
    // other place it exists is a running BMC. Handled before anything is
    // loaded so it works on a build machine.
    if arguments.get_flag("openapi") {
        println!(
            "{}",
            serde_json::to_string_pretty(&api::openapi::document())?
        );
        return Ok(());
    }

    let config_path: PathBuf = arguments
        .get_one::<PathBuf>("config")
        .expect("`config` is required unless `--openapi` was given")
        .into();
    let config = Config::load(&config_path).context("Error parsing config file")?;
    let _logger_lifetime = init_logger(&config.log);

    let tls = load_tls_config(&config.tls)?;
    let bmc = Data::new(BmcApplication::new(config.store.write_timeout).await?);
    let serial_service = Data::new(SerialConnections::new());
    let streaming_data_service = Data::new(StreamingDataService::new());
    let authentication = Arc::new(
        LinuxAuthenticator::new(
            "/api/bmc/authenticate",
            "Access to Baseboard Management Controller",
            config.authentication.token_expires,
            config.authentication.authentication_attempts,
        )
        .await?,
    );

    // The front-panel keys are read from /dev/input/event0, which exists only
    // on the board. Under `stubbed` -- the hardware-less build, which the
    // firmware never enables -- there is no such device and opening it is a
    // hard error, so the daemon cannot start at all on a workstation. Log it
    // and carry on rather than skipping silently: a build that quietly has no
    // power button would be worse than one that says so.
    #[cfg(feature = "stubbed")]
    if let Err(e) = run_event_listener(bmc.clone().into_inner()) {
        tracing::warn!("stubbed build: no front-panel input device ({e})");
    }
    #[cfg(not(feature = "stubbed"))]
    run_event_listener(bmc.clone().into_inner())?;

    // Ask the firmware sources what they offer before anyone asks us. The
    // fan-out reaches GitHub and an HTTP mirror and takes seconds; whoever
    // opens the firmware page first would otherwise pay for it, and that is
    // the person watching a board come back from an update.
    crate::app::firmware_catalog::prime();
    crate::app::ntp::ensure_source_dir().await;

    // The HTTPS listener is assembled by hand rather than with
    // `HttpServer::bind_openssl()`. That method takes an `SslAcceptorBuilder`
    // and then overwrites its ALPN configuration unconditionally -- see
    // `openssl_acceptor()` in actix-web's `server.rs`, which installs its own
    // `h2`-preferring select callback over whatever the caller configured --
    // so an ALPN policy set in `load_tls_config()` would be silently
    // discarded. What follows is what `bind_openssl()` does internally, minus
    // that overwrite, and with `.h1()` in place of `.finish()` so that the
    // HTTP/2 dispatcher is never constructed at all.
    //
    // Every default `HttpServer` would have applied is applied here, because
    // `HttpServiceBuilder`'s own defaults are not quite the same:
    // `client_request_timeout` (5 s) and `h1_allow_half_closed` (true) already
    // agree, but `client_disconnect_timeout` is zero here and one second
    // there, and zero means no shutdown deadline at all rather than a short
    // one. `local_addr` is the one thing not carried over -- it is per-socket
    // and this factory serves every address the host resolves to -- and it is
    // inert: actix-http stores it and no dispatcher reads it.
    //
    // `AppConfig::default()` stands in for the `AppConfig::new(true, host,
    // addr)` that `HttpServer` would have built. It is only ever read through
    // `ConnectionInfo`, as the fallback for `host()` and `scheme()` when a
    // request carries no `Host` header. Nothing in bmcd reads either; the peer
    // address the authenticator makes its loopback decision on comes from the
    // socket, not from here.
    // Cloned before the server factory takes ownership of them.
    let metrics_bmc = bmc.clone();
    let metrics_host = config.host.clone();
    let metrics_port = config.metrics_port;

    let run_server = Server::build()
        .workers(2)
        .bind("bmcd", (config.host.clone(), config.port), move || {
            let www_root = config.www.clone();
            let app = App::new()
                .service(
                    web::scope("/api/bmc")
                        .wrap(authentication.clone())
                        .app_data(bmc.clone())
                        .app_data(streaming_data_service.clone())
                        .app_data(serial_service.clone())
                        .configure(serial_config)
                        // Legacy API: `GET /api/bmc?opt=&type=`
                        .configure(legacy::config)
                        // The same operations, one path each, for clients
                        // that read the specification. Same authenticator,
                        // same app data, same dispatcher.
                        .configure(paths::config),
                )
                // Serve a static tree of files of the web UI. Must be the last item.
                .service(Files::new("/", &config.www).index_file("index.html"))
                .default_service(web::to(move || {
                    let www_index = www_root.join("index.html");
                    NamedFile::open_async(www_index)
                }));

            HttpService::build()
                .keep_alive(KeepAlive::Os)
                .client_disconnect_timeout(Duration::from_secs(1))
                .h1(map_config(app, |_| AppConfig::default()))
                .openssl(tls.clone())
        })?
        .run();

    // `/metrics` on its own listener: plain HTTP, no credential.
    //
    // It used to be a scope on this same TLS port behind a bespoke token. The
    // token existed for exactly one reason -- so that a credential in a scrape
    // config could not also reach `/api/bmc` and power a node off -- and on a
    // listener that serves nothing but `/metrics` there is nothing else to
    // reach. The property is kept; the mechanism is a port instead of a
    // secret, which is one fewer thing to mint, store, rotate and leak.
    //
    // Plain HTTP because the TLS it sat behind was scraped with verification
    // skipped: the board's certificate is expired and carries no SAN, so
    // nothing could verify it. Unverified TLS is a handshake per scrape on a
    // Cortex-A7 in exchange for nothing, and saying "plain, on a management
    // network" is more honest than implying a protection that was not there.
    //
    // What protects it is the network. This binds the same `host` as the API,
    // so binding the daemon to a management address later covers both.
    let metrics_server = HttpServer::new(move || {
        App::new()
            .app_data(metrics_bmc.clone())
            .service(web::scope("/metrics").configure(metrics::config))
    })
    .workers(1)
    .bind((metrics_host, metrics_port))?
    .run();

    let mut futures = vec![run_server, metrics_server];
    if config.redirect_http {
        // redirect requests to 'HTTPS'
        futures.push(
            HttpServer::new(move || {
                // Nothing but the redirect. `/info` used to be served here,
                // unauthenticated and in the clear: firmware version, build
                // time, IPv4 and the br0 MAC to anyone who could reach port 80,
                // and readable passively by anything on the segment. It was the
                // only registration of that handler, so there was no
                // authenticated equivalent to fall back to -- which is why this
                // is a removal rather than a move.
                App::new()
                    .app_data(Data::new(config.port))
                    .default_service(web::route().to(redirect))
            })
            .bind((config.host, HTTP_PORT))?
            .run(),
        );
    }

    // run server(s)
    join_all(futures).await;
    tracing::info!("exiting {}", env!("CARGO_PKG_NAME"));
    Ok(())
}

async fn redirect(request: HttpRequest, port: web::Data<u16>) -> HttpResponse {
    let host = request.connection_info().host().to_string();
    let path = request.uri().to_string();
    let redirect_url = format!("https://{}:{}{}", host, port.get_ref(), path);
    HttpResponse::PermanentRedirect()
        .append_header((http::header::LOCATION, redirect_url))
        .finish()
}

fn init_logger(log_config: &Log) -> WorkerGuard {
    let file_appender = tracing_appender::rolling::Builder::new()
        .rotation(Rotation::HOURLY)
        .max_log_files(3)
        .filename_prefix("bmcd")
        .filename_suffix("log")
        .build(std::env::temp_dir())
        .expect("error setting up log rotation");

    let (bmcd_log, guard) = tracing_appender::non_blocking(file_appender);
    let full_layer = tracing_subscriber::fmt::layer()
        .with_writer(bmcd_log)
        .with_ansi(log_config.coloring);

    let filter = EnvFilter::builder().parse_lossy(log_config.directive.clone());
    let stdout_layer = log_config.stdout.then_some(
        tracing_subscriber::fmt::layer()
            .without_time()
            .with_ansi(log_config.coloring)
            .with_writer(std::io::stdout)
            .compact(),
    );

    let layers = full_layer.and_then(stdout_layer).with_filter(filter);

    // The audit target goes to the system log as well as to the rotating
    // file. Both `/tmp` and `/var/log` are tmpfs on this board, so a line that
    // only reaches the file is gone at the next boot -- and a reboot is
    // exactly the event you would be trying to account for. Whether syslogd
    // then forwards anywhere is /etc/default/syslogd's business, not this
    // daemon's.
    let audit_layer = SyslogSink::open().map(|sink| {
        tracing_subscriber::fmt::layer()
            .with_writer(sink)
            .with_ansi(false)
            .without_time()
            .with_level(false)
            .with_target(false)
            .with_filter(tracing_subscriber::filter::filter_fn(|meta| {
                meta.target() == "audit"
            }))
    });

    tracing_subscriber::registry()
        .with(layers)
        .with(audit_layer)
        .init();

    tracing::info!("Turing Pi 2 BMC Daemon v{}", env!("CARGO_PKG_VERSION"));
    guard
}

/// Writes each formatted line to the system log as one datagram.
///
/// BusyBox `syslogd` listens on `/dev/log`, so this is all it takes to put a
/// line somewhere that outlives the process and can be forwarded off the
/// board. No syslog crate: the wire format for a local datagram is a priority
/// in angle brackets and then the text, and pulling a dependency in to write
/// eight bytes would be the larger change.
#[derive(Clone)]
struct SyslogSink {
    socket: Arc<UnixDatagram>,
}

impl SyslogSink {
    /// `None` when there is no system log to write to -- a workstation
    /// running the tests, or a board whose syslogd has not started. The audit
    /// line still reaches the rotating file in that case; this is the second
    /// copy, not the only one.
    fn open() -> Option<Self> {
        let socket = UnixDatagram::unbound().ok()?;
        match socket.connect("/dev/log") {
            Ok(()) => Some(SyslogSink {
                socket: Arc::new(socket),
            }),
            Err(e) => {
                eprintln!("no system log for audit lines: {e}");
                None
            }
        }
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for SyslogSink {
    type Writer = SyslogLine;

    fn make_writer(&'a self) -> Self::Writer {
        SyslogLine {
            socket: self.socket.clone(),
            line: Vec::new(),
        }
    }
}

/// One event, accumulated and sent when the formatter is done with it.
struct SyslogLine {
    socket: Arc<UnixDatagram>,
    line: Vec<u8>,
}

impl std::io::Write for SyslogLine {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.line.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        if self.line.is_empty() {
            return Ok(());
        }

        // authpriv.info: facility 10, severity 6. An audit trail is what
        // authpriv is for, and a reader filtering their collector by facility
        // should find these where they expect them.
        let mut datagram = format!("<86>bmcd[{}]: ", std::process::id()).into_bytes();
        datagram.extend_from_slice(self.line.trim_ascii_end());
        self.line.clear();

        // A full or absent socket must never take the daemon down, and must
        // never turn into a log line of its own -- that is how a logging path
        // becomes the outage.
        let _ = self.socket.send(&datagram);
        Ok(())
    }
}

impl Drop for SyslogLine {
    fn drop(&mut self) {
        use std::io::Write;
        let _ = self.flush();
    }
}

/// `--config` is required to run the daemon. `--openapi` prints the API
/// document and exits, so it is required-unless rather than required.
fn arguments() -> clap::ArgMatches {
    command!()
        .arg(
            Arg::new("config")
                .long("config")
                .value_parser(value_parser!(PathBuf))
                .required_unless_present("openapi"),
        )
        .arg(
            Arg::new("openapi")
                .long("openapi")
                .action(ArgAction::SetTrue)
                .help("Print this daemon's OpenAPI 3.1 document and exit"),
        )
        .get_matches()
}

fn load_keys_from_pem<P: AsRef<Path>>(
    private_key: P,
    certificate: P,
) -> anyhow::Result<(PKey<Private>, X509)> {
    let mut pkey = Vec::new();
    let mut cert = Vec::new();
    OpenOptions::new()
        .read(true)
        .open(private_key)
        .context("could not open private key file")?
        .read_to_end(&mut pkey)?;
    OpenOptions::new()
        .read(true)
        .open(certificate)
        .context("could not open cert file")?
        .read_to_end(&mut cert)?;

    let rsa_key = PKey::private_key_from_pem(&pkey)?;
    let x509 = X509::from_pem(&cert)?;
    Ok((rsa_key, x509))
}

/// The only protocol this daemon offers over ALPN, in OpenSSL's
/// length-prefixed wire form.
const ALPN_HTTP11: &[u8] = b"\x08http/1.1";

fn load_tls_config(tls_config: &config::Tls) -> anyhow::Result<SslAcceptor> {
    let (private_key, cert) = load_keys_from_pem(&tls_config.private_key, &tls_config.certificate)?;
    let mut tls = SslAcceptor::mozilla_intermediate(SslMethod::tls())?;
    tls.set_private_key(&private_key)?;
    tls.set_certificate(&cert)?;

    // Offer `http/1.1` and nothing else.
    //
    // Advertising `h2` puts HTTP/2 frame parsing in front of the
    // authentication middleware: the connection is handed to the h2 codec the
    // moment ALPN settles, so an unauthenticated caller reaches that parser
    // just by connecting. The h2 release this tree resolves to, 0.3.27, has a
    // known defect, there is no fixed 0.3.x to move to, and the newest
    // actix-http still depends on that line -- so the dependency cannot be
    // updated out of the problem. actix-web 5 is the framework answer and is
    // not something to take on in a security fix.
    //
    // Nothing here needs HTTP/2. A browser talking to a BMC and a Prometheus
    // scrape are both perfectly served by HTTP/1.1, so the cheapest correct
    // answer is to stop offering the protocol whose parser is the problem.
    //
    // A client that offers only `h2` gets `NOACK`: the handshake completes
    // with no ALPN protocol agreed, which is the same fallback actix-web gives
    // a client offering something it does not recognise.
    tls.set_alpn_select_callback(|_ssl, client_protocols| select_alpn(client_protocols));

    Ok(tls.build())
}

/// Pick a protocol from the client's ALPN list. `client_protocols` is
/// OpenSSL's wire form: a sequence of length-prefixed names.
fn select_alpn(client_protocols: &[u8]) -> Result<&[u8], AlpnError> {
    select_next_proto(ALPN_HTTP11, client_protocols).ok_or(AlpnError::NOACK)
}

#[cfg(test)]
mod tests {
    use super::*;
    use openssl::asn1::Asn1Time;
    use openssl::ec::{EcGroup, EcKey};
    use openssl::hash::MessageDigest;
    use openssl::nid::Nid;
    use openssl::ssl::{SslConnector, SslVerifyMode};
    use openssl::x509::X509Builder;
    use std::io::Write;
    use std::net::{TcpListener, TcpStream};

    /// `select_alpn` is the whole ALPN policy: `http/1.1` when the client will
    /// take it, and no agreement otherwise. It must never answer `h2`.
    #[test]
    fn alpn_never_selects_h2() {
        // What a browser sends, h2 first.
        assert_eq!(select_alpn(b"\x02h2\x08http/1.1"), Ok(&b"http/1.1"[..]));
        // The same list the other way round.
        assert_eq!(select_alpn(b"\x08http/1.1\x02h2"), Ok(&b"http/1.1"[..]));
        // A client that will only speak HTTP/1.1.
        assert_eq!(select_alpn(b"\x08http/1.1"), Ok(&b"http/1.1"[..]));
        // A client that will only speak HTTP/2 gets no agreement rather than
        // an agreement this daemon does not want to honour.
        assert!(matches!(select_alpn(b"\x02h2"), Err(AlpnError::NOACK)));
        // Anything else, likewise.
        assert!(matches!(
            select_alpn(b"\x08http/3.0"),
            Err(AlpnError::NOACK)
        ));
        assert!(matches!(select_alpn(b""), Err(AlpnError::NOACK)));
    }

    /// The policy above is worth nothing unless it survives into the acceptor
    /// that actually terminates TLS -- which is the failure mode this whole
    /// change exists to avoid, because `HttpServer::bind_openssl()` discards
    /// exactly this configuration without saying so. So run a real handshake
    /// against the real acceptor and read back what was negotiated.
    #[test]
    fn the_acceptor_negotiates_http11_and_never_h2() {
        let dir = tempdir::TempDir::new("bmcd-alpn").expect("temp dir");
        let key_path = dir.path().join("key.pem");
        let cert_path = dir.path().join("cert.pem");
        write_self_signed_pair(&key_path, &cert_path);

        let tls = config::Tls {
            private_key: key_path,
            certificate: cert_path,
        };

        // A browser's list: the acceptor must come back with http/1.1.
        assert_eq!(
            handshake_and_report_alpn(&tls, b"\x02h2\x08http/1.1"),
            Some(b"http/1.1".to_vec())
        );

        // h2 and nothing else: no protocol agreed, and the connection still
        // completes -- the same fallback a client offering something unknown
        // would have got from actix-web.
        assert_eq!(handshake_and_report_alpn(&tls, b"\x02h2"), None);
    }

    /// Runs one TLS handshake against the acceptor `load_tls_config` builds,
    /// with the client offering `client_alpn`, and returns what the client
    /// sees as the negotiated protocol.
    fn handshake_and_report_alpn(tls: &config::Tls, client_alpn: &[u8]) -> Option<Vec<u8>> {
        let acceptor = load_tls_config(tls).expect("acceptor");
        let listener = TcpListener::bind("127.0.0.1:0").expect("listener");
        let addr = listener.local_addr().expect("local addr");

        let server = std::thread::spawn(move || {
            let (socket, _) = listener.accept().expect("accept");
            let stream = acceptor.accept(socket).expect("server handshake");
            stream.ssl().selected_alpn_protocol().map(<[u8]>::to_vec)
        });

        let mut connector = SslConnector::builder(SslMethod::tls()).expect("connector");
        connector.set_verify(SslVerifyMode::NONE);
        connector
            .set_alpn_protos(client_alpn)
            .expect("client alpn list");

        let socket = TcpStream::connect(addr).expect("connect");
        let stream = connector
            .build()
            .configure()
            .expect("configure")
            .verify_hostname(false)
            .use_server_name_indication(false)
            .connect("localhost", socket)
            .expect("client handshake");

        let client_view = stream.ssl().selected_alpn_protocol().map(<[u8]>::to_vec);
        let server_view = server.join().expect("server thread");

        // Both ends have to agree, or the assertion above would be testing the
        // client's opinion of a handshake the server saw differently.
        assert_eq!(client_view, server_view);
        client_view
    }

    /// A throwaway self-signed EC P-384 certificate, written out as the PEM
    /// pair `load_keys_from_pem` expects.
    fn write_self_signed_pair(key_path: &Path, cert_path: &Path) {
        let group = EcGroup::from_curve_name(Nid::SECP384R1).expect("P-384");
        let ec_key = EcKey::generate(&group).expect("key");
        let pkey = PKey::from_ec_key(ec_key).expect("pkey");

        let mut builder = X509Builder::new().expect("x509 builder");
        builder.set_version(2).expect("version");
        builder
            .set_not_before(&Asn1Time::days_from_now(0).expect("not before"))
            .expect("not before");
        builder
            .set_not_after(&Asn1Time::days_from_now(1).expect("not after"))
            .expect("not after");
        builder.set_pubkey(&pkey).expect("pubkey");
        builder
            .sign(&pkey, MessageDigest::sha384())
            .expect("self-sign");
        let cert = builder.build();

        std::fs::File::create(key_path)
            .expect("key file")
            .write_all(&pkey.private_key_to_pem_pkcs8().expect("key pem"))
            .expect("write key");
        std::fs::File::create(cert_path)
            .expect("cert file")
            .write_all(&cert.to_pem().expect("cert pem"))
            .expect("write cert");
    }
}
