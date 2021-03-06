#[macro_use]
extern crate failure;

use std::fs;
use std::io::{self, Write};
use std::net::ToSocketAddrs;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use failure::Error;
use futures::TryFutureExt;
use structopt::StructOpt;
use tokio::runtime::current_thread::Runtime;
use tracing::{error, info};
use url::Url;

mod common;

type Result<T> = std::result::Result<T, Error>;

/// HTTP/0.9 over QUIC client
#[derive(StructOpt, Debug)]
#[structopt(name = "client")]
struct Opt {
    /// Perform NSS-compatible TLS key logging to the file specified in `SSLKEYLOGFILE`.
    #[structopt(long = "keylog")]
    keylog: bool,

    url: Url,

    /// Override hostname used for certificate verification
    #[structopt(long = "host")]
    host: Option<String>,

    /// Custom certificate authority to trust, in DER format
    #[structopt(parse(from_os_str), long = "ca")]
    ca: Option<PathBuf>,

    /// Simulate NAT rebinding after connecting
    #[structopt(long = "rebind")]
    rebind: bool,
}

fn main() {
    tracing::subscriber::set_global_default(
        tracing_subscriber::FmtSubscriber::builder()
            .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
            .finish(),
    )
    .unwrap();
    let opt = Opt::from_args();
    let code = {
        if let Err(e) = run(opt) {
            eprintln!("ERROR: {}", e);
            1
        } else {
            0
        }
    };
    ::std::process::exit(code);
}

fn run(options: Opt) -> Result<()> {
    let url = options.url;
    let remote = (url.host_str().unwrap(), url.port().unwrap_or(4433))
        .to_socket_addrs()?
        .next()
        .ok_or(format_err!("couldn't resolve to an address"))?;

    let mut endpoint = quinn::Endpoint::builder();
    let mut client_config = quinn::ClientConfigBuilder::default();
    client_config.protocols(common::ALPN_QUIC_HTTP);
    if options.keylog {
        client_config.enable_keylog();
    }
    if let Some(ca_path) = options.ca {
        client_config
            .add_certificate_authority(quinn::Certificate::from_der(&fs::read(&ca_path)?)?)?;
    } else {
        let dirs = directories::ProjectDirs::from("org", "quinn", "quinn-examples").unwrap();
        match fs::read(dirs.data_local_dir().join("cert.der")) {
            Ok(cert) => {
                client_config.add_certificate_authority(quinn::Certificate::from_der(&cert)?)?;
            }
            Err(ref e) if e.kind() == io::ErrorKind::NotFound => {
                info!("local server certificate not found");
            }
            Err(e) => {
                error!("failed to open local server certificate: {}", e);
            }
        }
    }

    endpoint.default_client_config(client_config.build());

    let (endpoint_driver, endpoint, _) = endpoint.bind(&"[::]:0".parse().unwrap())?;
    let mut runtime = Runtime::new()?;
    runtime.spawn(endpoint_driver.unwrap_or_else(|e| eprintln!("IO error: {}", e)));

    let request = format!("GET {}\r\n", url.path());
    let start = Instant::now();
    let rebind = options.rebind;
    let host = options
        .host
        .as_ref()
        .map_or_else(|| url.host_str(), |x| Some(&x))
        .ok_or(format_err!("no hostname specified"))?;
    let r: Result<()> = runtime.block_on(async {
        let new_conn = endpoint
            .connect(&remote, &host)?
            .await
            .map_err(|e| format_err!("failed to connect: {}", e))?;
        eprintln!("connected at {:?}", start.elapsed());
        tokio::runtime::current_thread::spawn(
            new_conn
                .driver
                .unwrap_or_else(|e| eprintln!("connection lost: {}", e)),
        );
        let conn = new_conn.connection;
        let (mut send, recv) = conn
            .open_bi()
            .await
            .map_err(|e| format_err!("failed to open stream: {}", e))?;
        if rebind {
            let socket = std::net::UdpSocket::bind("[::]:0").unwrap();
            let addr = socket.local_addr().unwrap();
            eprintln!("rebinding to {}", addr);
            endpoint
                .rebind(socket, &tokio_net::driver::Handle::default())
                .expect("rebind failed");
        }

        send.write_all(request.as_bytes())
            .await
            .map_err(|e| format_err!("failed to send request: {}", e))?;
        send.finish()
            .await
            .map_err(|e| format_err!("failed to shutdown stream: {}", e))?;
        let response_start = Instant::now();
        eprintln!("request sent at {:?}", response_start - start);
        let resp = recv
            .read_to_end(usize::max_value())
            .await
            .map_err(|e| format_err!("failed to read response: {}", e))?;
        let duration = response_start.elapsed();
        eprintln!(
            "response received in {:?} - {} KiB/s",
            duration,
            resp.len() as f32 / (duration_secs(&duration) * 1024.0)
        );
        io::stdout().write_all(&resp).unwrap();
        io::stdout().flush().unwrap();
        conn.close(0u32.into(), b"done");
        Ok(())
    });
    r?;

    // Allow the endpoint driver to automatically shut down
    drop(endpoint);

    // Let the connection finish closing gracefully
    runtime.run()?;

    Ok(())
}

fn duration_secs(x: &Duration) -> f32 {
    x.as_secs() as f32 + x.subsec_nanos() as f32 * 1e-9
}
