#[cfg(all(feature = "h3", feature = "h3-quinn"))]
mod server {
    use bytes::Bytes;
    use http::{Response, StatusCode};
    use http_body_util::Full;
    use quinn::{Endpoint, ServerConfig};
    use std::convert::Infallible;
    use std::sync::Arc;
    use vibeio::RuntimeBuilder;
    use vibeio_http::{Http3, Http3Options, HttpProtocol};

    // h3spec conformance server. Listen on 0.0.0.0:4433 with ALPN `h3` and answer
    // every request with a 200 response carrying a small non-empty body. Run via
    // `scripts/h3spec.sh`, which starts this binary, waits for UDP readiness, runs
    // h3spec (with `-n` so it tolerates the self-signed certificate), and
    // propagates its exit code.
    //
    // quinn's `Endpoint` (and each accepted connection) spawns tokio tasks, so the
    // accept loop runs on a tokio runtime. The native `Http3` driver is an async
    // future that must be polled by a vibeio runtime; each connection is handed to
    // its own vibeio runtime (driven by `block_on` on a dedicated thread), which is
    // what actually executes the HTTP/3 protocol logic.
    #[tokio::main]
    pub async fn main() -> Result<(), Box<dyn std::error::Error>> {
        let cert = rcgen::generate_simple_self_signed(vec!["localhost".into()])
            .expect("self-signed certificate");
        let cert_der: quinn::rustls::pki_types::CertificateDer<'static> = cert.cert.into();
        let mut tls = quinn::rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(
                vec![cert_der],
                quinn::rustls::pki_types::PrivateKeyDer::from(cert.signing_key),
            )
            .expect("tls config");
        tls.alpn_protocols = vec![b"h3".to_vec()];
        let quic_crypto =
            quinn::crypto::rustls::QuicServerConfig::try_from(tls).expect("quic crypto config");
        let server_config = ServerConfig::with_crypto(Arc::new(quic_crypto));

        let addr = "0.0.0.0:4433".parse().expect("socket address");
        let endpoint = Endpoint::server(server_config, addr).expect("quinn endpoint");

        println!("h3spec_server listening on {}", addr);

        loop {
            let connecting =
                match tokio::time::timeout(std::time::Duration::from_millis(50), endpoint.accept())
                    .await
                {
                    Ok(Some(c)) => c,
                    Ok(None) => break,
                    Err(_) => continue,
                };
            let connection = match connecting.await {
                Ok(connection) => connection,
                Err(err) => {
                    eprintln!("connection failed: {err}");
                    continue;
                }
            };
            // Each connection gets its own vibeio runtime, driven on a dedicated
            // thread. The h3 future is built here (after moving the connection in)
            // so it never has to cross a thread boundary as a value.
            //
            // In production, vibeio runtime would be reused...
            std::thread::spawn(move || {
                let runtime = match RuntimeBuilder::new().enable_timer(true).build() {
                    Ok(runtime) => runtime,
                    Err(err) => {
                        eprintln!("vibeio runtime build failed: {err}");
                        return;
                    }
                };
                let h3 = Http3::new(
                    vibeio_http::quinn::Connection::new(connection),
                    Http3Options::default(),
                );
                let _ = runtime.block_on(h3.handle(|_request| async move {
                    let response = Response::builder()
                        .status(StatusCode::OK)
                        .body(Full::new(Bytes::from_static(b"Hello World")))
                        .expect("valid response");
                    Ok::<_, Infallible>(response)
                }));
            });
        }
        Ok(())
    }
}

#[cfg(all(feature = "h3", feature = "h3-quinn"))]
use server::*;
#[cfg(not(all(feature = "h3", feature = "h3-quinn")))]
fn main() {
    unimplemented!("This example requires \"h3\" and \"h3-quinn\" features to be enabled.")
}
