use std::{
    borrow::Cow,
    collections::HashSet,
    net::{SocketAddr, TcpListener, TcpStream},
    sync::Arc,
};

use anyhow::{anyhow, Result};
use http_types::{headers::HeaderValue, Body, Cookie, Request, Response, StatusCode};
use smol::{
    block_on,
    io::AsyncRead,
    lock::Mutex,
    net::{resolve, AsyncToSocketAddrs},
    spawn, Async,
};

use crate::{config::Account, constants::CONFIG};

const COOKIE_NAME: &str = "__wj_token";

#[derive(Debug)]
struct Forward {
    tokens: Mutex<HashSet<String>>,
}

impl Forward {
    fn new() -> Forward {
        Forward {
            tokens: Mutex::new(HashSet::new()),
        }
    }

    fn check_domain_list_in_domain<'a>(domain_list: &[&'a str], domain: &&str) -> Option<&'a str> {
        for i in domain_list {
            if domain.contains(i) {
                return Some(i);
            }
        }
        None
    }

    async fn forward(&self, mut req: Request) -> http_types::Result<Response> {
        if CONFIG.authorization.enabled {
            if let Some(domain_list) = &CONFIG.authorization.domain_list {
                if let Some(d) = req.url().domain() {
                    let domain_list: Vec<_> = domain_list.iter().map(|i| i.as_str()).collect();
                    if let Some(domain) = Self::check_domain_list_in_domain(&domain_list, &d) {
                        // login
                        if req.url().path() == "/__wm__login" {
                            if let Some(account_list) = &CONFIG.authorization.account {
                                let account: Account = req.body_json().await?;
                                if account_list.contains(&account) {
                                    use time::{Duration, OffsetDateTime};
                                    use uuid::Uuid;
                                    let token = Uuid::new_v4().to_string();
                                    let mut tokens = self.tokens.lock().await;
                                    tokens.insert(token.clone());
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
                                    return Ok(resp);
                                } else {
                                    return Self::result(false);
                                }
                            }
                        // check authorization
                        } else {
                            let cookies_header = match req.header("Cookie") {
                                Some(c) => c,
                                None => return Self::show_login_page(),
                            };
                            let mut token = None;
                            'outer: for i in cookies_header {
                                for item in i.as_str().split("; ") {
                                    let cookie: Vec<_> = item.split('=').collect();
                                    if cookie.len() == 2 && cookie[0] == COOKIE_NAME {
                                        token = Some(cookie[1]);
                                        break 'outer;
                                    }
                                }
                            }
                            match token {
                                Some(token) => {
                                    let tokens = self.tokens.lock().await;
                                    if !tokens.contains(token) {
                                        return Self::show_login_page();
                                    }
                                }
                                None => return Self::show_login_page(),
                            }
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
                let s = Self::restore_domain(v.into());
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
        let path = Self::restore_domain(path.into());
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
            let host = Self::restore_domain(host.into());
            url.set_host(Some(&host))?;
            req.insert_header("host", host);
        }
        Self::restore_header(&mut req);
        if let Some(content_type) = req.content_type() {
            match content_type.essence() {
                "text/html"
                | "text/plain"
                | "text/javascript"
                | "application/json"
                | "application/manifest+json"
                | "application/x-www-form-urlencoded" => match req.body_string().await {
                    Ok(body) => {
                        let body = Self::restore_domain(body.into());
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
        let stream = match &CONFIG.socks5_server {
            Some(server) => {
                let server = Self::resolve(server).await?;
                trace!("socks5 dest: host: {}, port: {}", &host, port);
                socks5::connect_without_auth(server, (host.to_string(), port).into()).await?
            }
            None => {
                let addr = format!("{}:{}", host, port);
                let addr = Self::resolve(addr).await?;
                Async::<TcpStream>::connect(addr).await?
            }
        };

        let mut resp = match req.url().scheme() {
            "https" => {
                let stream = async_native_tls::connect(host, stream).await?;
                async_h1::connect(stream, req).await?
            }
            "http" => async_h1::connect(stream, req).await?,
            s => return Self::http_error(&format!("unsupported scheme: {}", s)),
        };

        Self::replace_header(&mut resp);

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
                            let body = Self::replace_domain(body.into());
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

    fn replace_domain(s: Cow<str>) -> String {
        let mut result = s.into_owned();
        for (k, v) in &CONFIG.domain_name {
            result = result.replace(v, k);
        }
        result
    }

    fn restore_domain(s: Cow<str>) -> String {
        let mut result = s.into_owned();
        for (k, v) in &CONFIG.domain_name {
            result = result.replace(k, v);
        }
        result
    }

    fn replace_header(req: &mut Response) {
        const HEADERS: &[&str] = &[
            "location",
            "set-cookie",
            "access-control-allow-origin",
            "content-security-policy",
            "x-frame-options",
        ];

        for i in HEADERS {
            if let Some(h) = req.header(*i) {
                let h = Self::replace_domain(h.as_str().into());
                req.insert_header(*i, h);
            }
        }
    }

    fn restore_header(req: &mut Request) {
        const HEADERS: &[&str] = &["origin", "referer"];

        for i in HEADERS {
            if let Some(h) = req.header(*i) {
                let h = Self::restore_domain(h.as_str().into());
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
        let coder = async_std::io::BufReader::new(coder);
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
    block_on(async {
        CONFIG.check_domain()?;
        let listen_address: SocketAddr = CONFIG.listen_address.parse()?;
        let listener = Async::<TcpListener>::bind(listen_address)?;
        let forward = Forward::new();
        let forward = Arc::new(forward);
        loop {
            let (stream, _) = listener.accept().await?;
            let stream = async_dup::Arc::new(stream);
            let forward = forward.clone();
            let task = spawn(async move {
                let f = |req| async { forward.forward(req).await };
                if let Err(err) = async_h1::accept(stream, f).await {
                    error!("Connection error: {:#?}", err);
                }
            });
            task.detach();
        }
    })
}
