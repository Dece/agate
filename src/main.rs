#![forbid(unsafe_code)]

mod certificates;
mod metadata;
use metadata::{FileOptions, PresetMeta};

use {
    once_cell::sync::Lazy,
    percent_encoding::{percent_decode_str, percent_encode, AsciiSet, CONTROLS},
    rcgen::{Certificate, CertificateParams, DnType},
    rustls::{NoClientAuth, ServerConfig},
    std::{
        borrow::Cow,
        error::Error,
        ffi::OsStr,
        fmt::Write,
        fs::{self, File},
        io::Write as _,
        net::SocketAddr,
        path::{self, Component, Path, PathBuf},
        sync::Arc,
    },
    tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::{TcpListener, TcpStream},
        runtime::Runtime,
        sync::Mutex,
    },
    tokio_rustls::{server::TlsStream, TlsAcceptor},
    url::{Host, Url},
};

fn main() -> Result {
    env_logger::Builder::from_env(
        // by default only turn on logging for agate
        env_logger::Env::default().default_filter_or("agate=info"),
    )
    .init();
    Runtime::new()?.block_on(async {
        let default = PresetMeta::Parameters(
            ARGS.language
                .as_ref()
                .map_or(String::new(), |lang| format!(";lang={}", lang)),
        );
        let mimetypes = Arc::new(Mutex::new(FileOptions::new(default)));
        let listener = TcpListener::bind(&ARGS.addrs[..]).await?;
        log::info!("Listening on {:?}...", ARGS.addrs);
        loop {
            let (stream, _) = listener.accept().await?;
            let arc = mimetypes.clone();
            tokio::spawn(async {
                match RequestHandle::new(stream, arc).await {
                    Ok(handle) => match handle.handle().await {
                        Ok(info) => log::info!("{}", info),
                        Err(err) => log::warn!("{}", err),
                    },
                    Err(log_line) => {
                        log::warn!("{}", log_line);
                    }
                }
            });
        }
    })
}

type Result<T = (), E = Box<dyn Error + Send + Sync>> = std::result::Result<T, E>;

static ARGS: Lazy<Args> = Lazy::new(|| {
    args().unwrap_or_else(|s| {
        eprintln!("{}", s);
        std::process::exit(1);
    })
});

struct Args {
    addrs: Vec<SocketAddr>,
    content_dir: PathBuf,
    certs: Arc<certificates::CertStore>,
    hostnames: Vec<Host>,
    language: Option<String>,
    serve_secret: bool,
    log_ips: bool,
    only_tls13: bool,
    central_config: bool,
}

fn args() -> Result<Args> {
    let args: Vec<String> = std::env::args().collect();
    let mut opts = getopts::Options::new();
    opts.optopt(
        "",
        "content",
        "Root of the content directory (default ./content/)",
        "DIR",
    );
    opts.optopt(
        "",
        "certs",
        "Root of the certificate directory (default ./.certificates/)",
        "DIR",
    );
    opts.optmulti(
        "",
        "addr",
        "Address to listen on (default 0.0.0.0:1965 and [::]:1965; muliple occurences means listening on multiple interfaces)",
        "IP:PORT",
    );
    opts.optmulti(
        "",
        "hostname",
        "Domain name of this Gemini server, enables checking hostname and port in requests. (multiple occurences means basic vhosts)",
        "NAME",
    );
    opts.optopt(
        "",
        "lang",
        "RFC 4646 Language code for text/gemini documents",
        "LANG",
    );
    opts.optflag("h", "help", "Print this help text and exit.");
    opts.optflag("V", "version", "Print version information and exit.");
    opts.optflag(
        "3",
        "only-tls13",
        "Only use TLSv1.3 (default also allows TLSv1.2)",
    );
    opts.optflag(
        "",
        "serve-secret",
        "Enable serving secret files (files/directories starting with a dot)",
    );
    opts.optflag("", "log-ip", "Output the remote IP address when logging.");
    opts.optflag(
        "C",
        "central-conf",
        "Use a central .meta file in the content root directory. Decentral config files will be ignored.",
    );
    opts.optflag(
        "e",
        "ed25519",
        "Generate keys using the Ed25519 signature algorithm instead of the default ECDSA.",
    );

    let matches = opts.parse(&args[1..]).map_err(|f| f.to_string())?;

    if matches.opt_present("h") {
        eprintln!("{}", opts.usage(&format!("Usage: {} [options]", &args[0])));
        std::process::exit(0);
    }

    if matches.opt_present("V") {
        eprintln!("agate {}", env!("CARGO_PKG_VERSION"));
        std::process::exit(0);
    }

    // try to open the certificate directory
    let certs_path = matches.opt_get_default("certs", ".certificates".to_string())?;
    let (certs, certs_path) = match check_path(certs_path.clone()) {
        // the directory exists, try to load certificates
        Ok(certs_path) => match certificates::CertStore::load_from(&certs_path) {
            // all is good
            Ok(certs) => (Some(certs), certs_path),
            // the certificate directory did not contain certificates, but we can generate some
            // because the hostname option was given
            Err(certificates::CertLoadError::Empty) if matches.opt_present("hostname") => {
                (None, certs_path)
            }
            // failed loading certificates or missing hostname to generate them
            Err(e) => return Err(e.into()),
        },
        // the directory does not exist
        Err(_) => {
            // since certificate management should be automated, we are going to create the directory too
            log::info!(
                "The certificate directory {:?} does not exist, creating it.",
                certs_path
            );
            std::fs::create_dir(&certs_path).expect("could not create certificate directory");
            // we just created the directory, skip loading from it
            (None, PathBuf::from(certs_path))
        }
    };

    // If we have not loaded any certificates yet, we have to try to reload them later.
    // This ensures we get the right error message.
    let mut reload_certs = certs.is_none();

    let mut hostnames = vec![];
    for s in matches.opt_strs("hostname") {
        let hostname = Host::parse(&s)?;

        // check if we have a certificate for that domain
        if let Host::Domain(ref domain) = hostname {
            if !matches!(certs, Some(ref certs) if certs.has_domain(domain)) {
                log::info!("No certificate or key found for {:?}, generating them.", s);

                let mut cert_params = CertificateParams::new(vec![domain.clone()]);
                cert_params
                    .distinguished_name
                    .push(DnType::CommonName, domain);

                // <CertificateParams as Default>::default() already implements a
                // date in the far future from the time of writing: 4096-01-01

                if matches.opt_present("e") {
                    cert_params.alg = &rcgen::PKCS_ED25519;
                }

                // generate the certificate with the configuration
                let cert = Certificate::from_params(cert_params)?;

                // make sure the certificate directory exists
                fs::create_dir(certs_path.join(domain))?;
                // write certificate data to disk
                let mut cert_file = File::create(certs_path.join(format!(
                    "{}/{}",
                    domain,
                    certificates::CERT_FILE_NAME
                )))?;
                cert_file.write_all(&cert.serialize_der()?)?;
                // write key data to disk
                let mut key_file = File::create(certs_path.join(format!(
                    "{}/{}",
                    domain,
                    certificates::KEY_FILE_NAME
                )))?;
                key_file.write_all(&cert.serialize_private_key_der())?;

                reload_certs = true;
            }
        }

        hostnames.push(hostname);
    }

    // if new certificates were generated, reload the certificate store
    let certs = if reload_certs {
        certificates::CertStore::load_from(&certs_path)?
    } else {
        // there must already have been certificates loaded
        certs.unwrap()
    };

    // parse listening addresses
    let mut addrs = vec![];
    for i in matches.opt_strs("addr") {
        addrs.push(i.parse()?);
    }
    if addrs.is_empty() {
        addrs = vec![
            "[::]:1965".parse().unwrap(),
            "0.0.0.0:1965".parse().unwrap(),
        ];
    }

    Ok(Args {
        addrs,
        content_dir: check_path(matches.opt_get_default("content", "content".into())?)?,
        certs: Arc::new(certs),
        hostnames,
        language: matches.opt_str("lang"),
        serve_secret: matches.opt_present("serve-secret"),
        log_ips: matches.opt_present("log-ip"),
        only_tls13: matches.opt_present("only-tls13"),
        central_config: matches.opt_present("central-conf"),
    })
}

fn check_path(s: String) -> Result<PathBuf, String> {
    let p = PathBuf::from(s);
    if p.as_path().exists() {
        Ok(p)
    } else {
        Err(format!("No such file: {:?}", p))
    }
}

/// TLS configuration.
static TLS: Lazy<TlsAcceptor> = Lazy::new(acceptor);

fn acceptor() -> TlsAcceptor {
    let mut config = ServerConfig::new(NoClientAuth::new());
    if ARGS.only_tls13 {
        config.versions = vec![rustls::ProtocolVersion::TLSv1_3];
    }
    config.cert_resolver = ARGS.certs.clone();
    TlsAcceptor::from(Arc::new(config))
}

struct RequestHandle {
    stream: TlsStream<TcpStream>,
    log_line: String,
    metadata: Arc<Mutex<FileOptions>>,
}

impl RequestHandle {
    /// Creates a new request handle for the given stream. If establishing the TLS
    /// session fails, returns a corresponding log line.
    async fn new(stream: TcpStream, metadata: Arc<Mutex<FileOptions>>) -> Result<Self, String> {
        let local_addr = stream.local_addr().unwrap().to_string();

        // try to get the remote IP address if desired
        let peer_addr = if ARGS.log_ips {
            stream
                .peer_addr()
                .map_err(|_| {
                    format!(
                        // use nonexistent status code 01 if peer IP is unknown
                        "{} - \"\" 01 \"IP error\" error:could not get peer address",
                        local_addr,
                    )
                })?
                .ip()
                .to_string()
        } else {
            // Do not log IP address, but something else so columns still line up.
            "-".into()
        };

        let log_line = format!("{} {}", local_addr, peer_addr,);

        match TLS.accept(stream).await {
            Ok(stream) => Ok(Self {
                stream,
                log_line,
                metadata,
            }),
            // use nonexistent status code 00 if connection was not established
            Err(e) => Err(format!("{} \"\" 00 \"TLS error\" error:{}", log_line, e)),
        }
    }

    /// Do the necessary actions to handle this request. Returns a corresponding
    /// log line as Err or Ok, depending on if the request finished with or
    /// without errors.
    async fn handle(mut self) -> Result<String, String> {
        // not already in error condition
        let result = match self.parse_request().await {
            Ok(url) => self.send_response(url).await,
            Err((status, msg)) => self.send_header(status, msg).await,
        };

        if let Err(e) = result {
            Err(format!("{} error:{}", self.log_line, e))
        } else if let Err(e) = self.stream.shutdown().await {
            Err(format!("{} error:{}", self.log_line, e))
        } else {
            Ok(self.log_line)
        }
    }

    /// Return the URL requested by the client.
    async fn parse_request(&mut self) -> std::result::Result<Url, (u8, &'static str)> {
        // Because requests are limited to 1024 bytes (plus 2 bytes for CRLF), we
        // can use a fixed-sized buffer on the stack, avoiding allocations and
        // copying, and stopping bad clients from making us use too much memory.
        let mut request = [0; 1026];
        let mut buf = &mut request[..];
        let mut len = 0;

        // Read until CRLF, end-of-stream, or there's no buffer space left.
        //
        // Since neither CR nor LF can be part of a URI according to
        // ISOC-RFC 3986, we could use BufRead::read_line here, but that does
        // not allow us to cap the number of read bytes at 1024+2.
        let result = loop {
            let bytes_read = if let Ok(read) = self.stream.read(buf).await {
                read
            } else {
                break Err((59, "Request ended unexpectedly"));
            };
            len += bytes_read;
            if request[..len].ends_with(b"\r\n") {
                break Ok(());
            } else if bytes_read == 0 {
                break Err((59, "Request ended unexpectedly"));
            }
            buf = &mut request[len..];
        }
        .and_then(|()| std::str::from_utf8(&request[..len - 2]).or(Err((59, "Non-UTF-8 request"))));

        let request = result.map_err(|e| {
            // write empty request to log line for uniformity
            write!(self.log_line, " \"\"").unwrap();
            e
        })?;

        // log literal request (might be different from or not an actual URL)
        write!(self.log_line, " \"{}\"", request).unwrap();

        let url = Url::parse(request).or(Err((59, "Invalid URL")))?;

        // Validate the URL:
        // correct scheme
        if url.scheme() != "gemini" {
            return Err((53, "Unsupported URL scheme"));
        }

        // no userinfo and no fragment
        if url.password().is_some() || !url.username().is_empty() || url.fragment().is_some() {
            return Err((59, "URL contains fragment or userinfo"));
        }

        // correct host
        if let Some(host) = url.host() {
            // do not use "contains" here since it requires the same type and does
            // not allow to check for Host<&str> if the vec contains Hostname<String>
            if !ARGS.hostnames.is_empty() && !ARGS.hostnames.iter().any(|h| h == &host) {
                return Err((53, "Proxy request refused"));
            }
        } else {
            return Err((59, "URL does not contain a host"));
        }

        // correct port
        if let Some(port) = url.port() {
            // Validate that the port in the URL is the same as for the stream this request came in on.
            if port != self.stream.get_ref().0.local_addr().unwrap().port() {
                return Err((53, "proxy request refused"));
            }
        }
        Ok(url)
    }

    /// Send the client the file located at the requested URL.
    async fn send_response(&mut self, url: Url) -> Result {
        let mut path = std::path::PathBuf::from(&ARGS.content_dir);

        if ARGS.hostnames.len() > 1 {
            // basic vhosts, existence of host_str was checked by parse_request already
            path.push(url.host_str().expect("no hostname"));
        }

        if let Some(mut segments) = url.path_segments() {
            // append percent-decoded path segments
            for segment in segments.clone() {
                // To prevent directory traversal attacks, we need to
                // check that each filesystem path component in the URL
                // path segment is a normal component (not the root
                // directory, the parent directory, a drive label, or
                // another special component). Furthermore, since path
                // separators (e.g. the escaped forward slash %2F) in a
                // single URL path segment are non-structural, the URL
                // path segment should not contain multiple filesystem
                // path components.
                let decoded = percent_decode_str(segment).decode_utf8()?;
                let mut components = Path::new(decoded.as_ref()).components();
                // the first component must be a normal component; if
                // so, push it onto the PathBuf
                match components.next() {
                    None => (),
                    Some(Component::Normal(c)) => path.push(c),
                    Some(_) => return self.send_header(51, "Not found, sorry.").await,
                }
                // there must not be more than one component
                if components.next().is_some() {
                    return self.send_header(51, "Not found, sorry.").await;
                }
                // even if it's one component, there may be trailing path
                // separators at the end
                if decoded.ends_with(path::is_separator) {
                    return self.send_header(51, "Not found, sorry.").await;
                }
            }
            // check if hiding files is disabled
            if !ARGS.serve_secret
                // there is a configuration for this file, assume it should be served
                && !self.metadata.lock().await.exists(&path)
                // check if file or directory is hidden
                && segments.any(|segment| segment.starts_with('.'))
            {
                return self
                    .send_header(52, "If I told you, it would not be a secret.")
                    .await;
            }
        }

        if let Ok(metadata) = tokio::fs::metadata(&path).await {
            if metadata.is_dir() {
                if url.path().ends_with('/') || url.path().is_empty() {
                    // if the path ends with a slash or the path is empty, the links will work the same
                    // without a redirect
                    path.push("index.gmi");
                    if !path.exists() {
                        if path.with_file_name(".directory-listing-ok").exists() {
                            path.pop();
                            return self.list_directory(&path).await;
                        } else {
                            self.send_header(51, "Directory index disabled.").await?;
                            return Ok(());
                        }
                    }
                } else {
                    // if client is not redirected, links may not work as expected without trailing slash
                    let mut url = url;
                    url.set_path(&format!("{}/", url.path()));
                    return self.send_header(31, url.as_str()).await;
                }
            }
        }

        let data = self.metadata.lock().await.get(&path);

        if let PresetMeta::FullHeader(status, meta) = data {
            self.send_header(status, &meta).await?;
            // do not try to access the file
            return Ok(());
        }

        // Make sure the file opens successfully before sending a success header.
        let mut file = match tokio::fs::File::open(&path).await {
            Ok(file) => file,
            Err(e) => {
                self.send_header(51, "Not found, sorry.").await?;
                return Err(e.into());
            }
        };

        // Send header.
        let mime = match data {
            // this was already handled before opening the file
            PresetMeta::FullHeader(..) => unreachable!(),
            // treat this as the full MIME type
            PresetMeta::FullMime(mime) => mime.clone(),
            // guess the MIME type and add the parameters
            PresetMeta::Parameters(params) => {
                if path.extension() == Some(OsStr::new("gmi")) {
                    format!("text/gemini{}", params)
                } else {
                    let mime = mime_guess::from_path(&path).first_or_octet_stream();
                    format!("{}{}", mime.essence_str(), params)
                }
            }
        };
        self.send_header(20, &mime).await?;

        // Send body.
        tokio::io::copy(&mut file, &mut self.stream).await?;
        Ok(())
    }

    async fn list_directory(&mut self, path: &Path) -> Result {
        // https://url.spec.whatwg.org/#path-percent-encode-set
        const ENCODE_SET: AsciiSet = CONTROLS
            .add(b' ')
            .add(b'"')
            .add(b'#')
            .add(b'<')
            .add(b'>')
            .add(b'?')
            .add(b'`')
            .add(b'{')
            .add(b'}');

        log::info!("Listing directory {:?}", path);
        self.send_header(20, "text/gemini").await?;
        let mut entries = tokio::fs::read_dir(path).await?;
        let mut lines = vec![];
        while let Some(entry) = entries.next_entry().await? {
            let mut name = entry
                .file_name()
                .into_string()
                .or(Err("Non-Unicode filename"))?;
            if name.starts_with('.') {
                continue;
            }
            if entry.file_type().await?.is_dir() {
                name += "/";
            }
            let line = match percent_encode(name.as_bytes(), &ENCODE_SET).into() {
                Cow::Owned(url) => format!("=> {} {}\n", url, name),
                Cow::Borrowed(url) => format!("=> {}\n", url), // url and name are identical
            };
            lines.push(line);
        }
        lines.sort();
        for line in lines {
            self.stream.write_all(line.as_bytes()).await?;
        }
        Ok(())
    }

    async fn send_header(&mut self, status: u8, meta: &str) -> Result {
        // add response status and response meta
        write!(self.log_line, " {} \"{}\"", status, meta)?;

        self.stream
            .write_all(format!("{} {}\r\n", status, meta).as_bytes())
            .await?;
        Ok(())
    }
}
