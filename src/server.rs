use std::{
    borrow::Cow,
    net::{SocketAddr, TcpListener, TcpStream},
    path::Path,
    sync::Arc,
};

use anyhow::{Result, anyhow};
use async_executor::Executor;
use async_io::{Async, block_on};
use async_net::{AsyncToSocketAddrs, resolve};
use futures_lite::io::{AsyncRead, BufReader};
use http_types::{
    Body, Cookie, Request, Response, StatusCode,
    headers::{CONTENT_LENGTH, HeaderValue},
};
use redb::{Database, TableDefinition, TableHandle};
use regex::Regex;
use tracing::error;

use crate::config::{Account, CONFIG};

const COOKIE_NAME: &str = "__wj_token";
const LOGIN_URL_PATH: &str = "/__wj__login";
const TOKENS: TableDefinition<&str, ()> = TableDefinition::new("tokens");

#[derive(Debug)]
struct Forward {
    replace_domain: Vec<(Regex, String)>,
    restore_domain: Vec<(Regex, String)>,
    db: Database,
}

impl Forward {
    fn new() -> Result<Forward> {
        let mut replace_domain = Vec::new();
        for (k, v) in &CONFIG.domain_name {
            let i = (Regex::new(&v.replace('.', "\\."))?, k.to_string());
            replace_domain.push(i);
        }
        let mut restore_domain = Vec::new();
        for (k, v) in &CONFIG.domain_name {
            let i = (Regex::new(&k.replace('.', "\\."))?, v.to_string());
            restore_domain.push(i);
        }

        let db_filename = Path::new(&CONFIG.data_dir).join("db.redb");
        let db = Database::create(db_filename)?;

        Ok(Forward {
            replace_domain,
            restore_domain,
            db,
        })
    }

    async fn forward(&self, mut req: Request) -> http_types::Result<Response> {
        if CONFIG.authorization.enabled {
            if let Some(domain_list) = &CONFIG.authorization.domain_list {
                if let Some(d) = req.url().domain() {
                    if let Some(domain) = domain_list.iter().find(|&i| d.contains(i)) {
                        if req.url().path() == LOGIN_URL_PATH {
                            return self.login(req, domain).await;
                        } else if !self.authorization(&req)? {
                            return Self::show_login_page();
                        }
                    }
                }
            }
        }

        match req.header("X-Web-Jingzi") {
            Some(_) => return Self::http_error("may be circular request"),
            None => req.insert_header("X-Web-Jingzi", "true"),
        };

        let query: Vec<_> = req
            .url()
            .query_pairs()
            .map(|(q, v)| {
                let s = self.replace_domain(v, false);
                format!("{}={}", q, s)
            })
            .collect();
        let query = query.join("&");
        let scheme = match req.url().domain() {
            Some(domain) => CONFIG
                .use_https
                .as_ref()
                .and_then(|use_https| {
                    if use_https.iter().any(|i| i == domain) {
                        Some("https".to_string())
                    } else {
                        None
                    }
                })
                .or_else(|| req.header("X-Scheme").map(|i| i.as_str().to_string())),
            None => return Self::http_error("missing domain in request"),
        };
        let path = req.url().path();
        let path = self.replace_domain(path.into(), false);
        let url = req.url_mut();
        if !query.is_empty() {
            url.set_query(Some(&query));
        }
        if let Some(scheme) = scheme {
            if url.set_scheme(&scheme).is_err() {
                return Self::http_error("invalid request");
            }
        }
        url.set_path(&path);
        if let Some(host) = url.host_str() {
            let host = self.replace_domain(host.into(), false);
            url.set_host(Some(&host))?;
            req.insert_header("host", host);
        }
        self.restore_header(&mut req);
        if let Some(content_type) = req.content_type() {
            match content_type.essence() {
                "text/html"
                | "text/plain"
                | "text/javascript"
                | "application/json"
                | "application/manifest+json"
                | "application/x-www-form-urlencoded" => match req.body_string().await {
                    Ok(body) => {
                        let body = self.replace_domain(body.into(), false);
                        req.set_body(body);
                    }
                    Err(_) => error!("can not convert body to utf-8 string"),
                },
                _ => (),
            }
        }

        let host = match req.host() {
            Some(host) => host,
            None => return Self::http_error("invalid request"),
        };
        let port = match req.url().port_or_known_default() {
            Some(port) => port,
            None => return Self::http_error("invalid request"),
        };
        let stream = Async::<TcpStream>::connect(Self::resolve((host, port)).await?).await?;

        let mut resp = match req.url().scheme() {
            "https" => {
                let stream = async_native_tls::connect(req.url(), stream).await?;
                async_h1::connect(stream, req).await?
            }
            "http" => async_h1::connect(stream, req).await?,
            s => return Self::http_error(&format!("unsupported scheme: {}", s)),
        };

        self.replace_header(&mut resp);

        if resp.status() == StatusCode::NotModified {
            return Ok(resp);
        }

        if let Some(content_type) = resp.content_type() {
            match content_type.essence() {
                "text/html"
                | "text/plain"
                | "text/javascript"
                | "application/json"
                | "application/manifest+json"
                | "application/x-www-form-urlencoded" => {
                    Coder::De.code(&mut resp);
                    match resp.body_string().await {
                        Ok(body) => {
                            let body = self.replace_domain(body.into(), true);
                            resp.set_body(body);
                        }
                        Err(_) => error!("can not convert body to utf-8 string"),
                    }
                    Coder::En.code(&mut resp);
                }
                _ => (),
            }
        }
        Ok(resp)
    }

    async fn login(&self, mut req: Request, domain: &str) -> http_types::Result<Response> {
        if let Some(account_list) = &CONFIG.authorization.account {
            let account: Account = req.body_json().await?;
            if account_list.contains(&account) {
                use time::{Duration, OffsetDateTime};

                use uuid::Uuid;
                let token = Uuid::new_v4().to_string();

                let write_txn = self.db.begin_write()?;
                {
                    let mut table = write_txn.open_table(TOKENS)?;
                    table.insert(token.as_str(), ())?;
                }
                write_txn.commit()?;

                let mut expires = OffsetDateTime::now_utc();
                expires += Duration::days(3650);
                let cookie = Cookie::build(COOKIE_NAME, &token)
                    .domain(domain)
                    .expires(expires)
                    .secure(true)
                    .http_only(true)
                    .finish();
                let cookie: HeaderValue = cookie.into();
                let mut resp = Self::result(true)?;
                resp.append_header("Set-Cookie", cookie);
                Ok(resp)
            } else {
                Self::result(false)
            }
        } else {
            Self::result(false)
        }
    }

    fn authorization(&self, req: &Request) -> Result<bool> {
        let cookies_header = match req.header("Cookie") {
            Some(c) => c,
            None => return Ok(false),
        };

        let token = cookies_header.iter().find_map(|cookie| {
            cookie.as_str().split("; ").find_map(|item| {
                let values: Vec<_> = item.split('=').collect();
                if values.len() == 2 && values[0] == COOKIE_NAME {
                    Some(values[1])
                } else {
                    None
                }
            })
        });

        Ok(match token {
            Some(token) => {
                let read_txn = self.db.begin_read()?;
                if read_txn
                    .list_tables()?
                    .into_iter()
                    .any(|i| i.name() == TOKENS.name())
                {
                    let table = read_txn.open_table(TOKENS)?;
                    table.get(token)?.is_some()
                } else {
                    false
                }
            }
            None => false,
        })
    }

    /// replace or restore domain
    fn replace_domain(&self, text: Cow<str>, is_replace: bool) -> String {
        let regex_domain = if is_replace {
            &self.replace_domain
        } else {
            &self.restore_domain
        };
        let mut result = text.into_owned();
        for (regex, rep) in regex_domain {
            result = regex.replace_all(&result, rep).to_string();
        }
        result
    }

    fn replace_header(&self, req: &mut Response) {
        const HEADERS: &[&str] = &[
            "location",
            "set-cookie",
            "access-control-allow-origin",
            "content-security-policy",
            "x-frame-options",
        ];

        for i in HEADERS {
            if let Some(h) = req.header(*i) {
                let h = self.replace_domain(h.as_str().into(), true);
                req.insert_header(*i, h);
            }
        }
    }

    fn restore_header(&self, req: &mut Request) {
        const HEADERS: &[&str] = &["origin", "referer"];

        for i in HEADERS {
            if let Some(h) = req.header(*i) {
                let h = self.replace_domain(h.as_str().into(), false);
                req.insert_header(*i, h);
            }
        }
    }

    async fn resolve<T: AsyncToSocketAddrs>(s: T) -> Result<SocketAddr> {
        Ok(*resolve(s)
            .await?
            .first()
            .ok_or_else(|| anyhow!("invalid address"))?)
    }

    fn http_error(error: &str) -> http_types::Result<Response> {
        let mut resp = Response::new(StatusCode::InternalServerError);
        resp.set_content_type(http_types::mime::PLAIN);
        resp.set_body(error);
        Ok(resp)
    }

    fn show_login_page() -> http_types::Result<Response> {
        let mut resp = Response::new(StatusCode::Ok);
        resp.set_content_type(http_types::mime::HTML);
        resp.set_body(&include_bytes!("login.html")[..]);
        Ok(resp)
    }

    fn result(success: bool) -> http_types::Result<Response> {
        let mut resp = Response::new(StatusCode::Ok);
        resp.set_content_type(http_types::mime::JSON);
        resp.set_body(format!("{{\"success\": {}}}", success));
        Ok(resp)
    }
}

macro_rules! set_code {
    ($response: ident, $coder: ident) => {{
        let body = $response.take_body();
        $response.remove_header(CONTENT_LENGTH);
        Self::set_body($response, $coder::new(body))
    }};
}

enum Coder {
    De,
    En,
}

impl Coder {
    fn set_body<T>(resp: &mut Response, coder: T)
    where
        T: AsyncRead + Unpin + Send + Sync + 'static,
    {
        let coder = BufReader::new(coder);
        let body = Body::from_reader(coder, None);
        resp.set_body(body);
    }

    fn code(&self, resp: &mut Response) {
        use async_compression::futures::bufread::{
            BrotliDecoder, BrotliEncoder, DeflateDecoder, DeflateEncoder, GzipDecoder, GzipEncoder,
        };

        if let Some(encoding) = resp.header("content-encoding") {
            let encoding = encoding.as_str();
            match self {
                Coder::En => match encoding {
                    "gzip" => set_code!(resp, GzipEncoder),
                    "br" => set_code!(resp, BrotliEncoder),
                    "deflate" => set_code!(resp, DeflateEncoder),
                    e => error!("unhandled encoding: {}", e),
                },
                Coder::De => match encoding {
                    "gzip" => set_code!(resp, GzipDecoder),
                    "br" => set_code!(resp, BrotliDecoder),
                    "deflate" => set_code!(resp, DeflateDecoder),
                    e => error!("unhandled encoding: {}", e),
                },
            };
        }
    }
}

pub fn run() -> Result<()> {
    let executor = Executor::new();
    block_on(executor.run(async {
        CONFIG.check_domain()?;
        let listen_address: SocketAddr = CONFIG.listen_address.parse()?;
        let listener = Async::<TcpListener>::bind(listen_address)?;
        let forward = Arc::new(Forward::new()?);
        loop {
            let (stream, _) = listener.accept().await?;
            let forward = forward.clone();
            executor
                .spawn(async move {
                    if let Err(err) = async_h1::accept(async_dup::Arc::new(stream), |req| async {
                        forward.forward(req).await
                    })
                    .await
                    {
                        error!("Connection error: {:#?}", err);
                    }
                })
                .detach();
        }
    }))
}
