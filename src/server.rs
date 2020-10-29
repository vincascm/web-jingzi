use std::{
    collections::HashMap,
    net::{SocketAddr, TcpListener, TcpStream},
};

use anyhow::{anyhow, Result};
use http_types::{Body, Request, Response, StatusCode};
use smol::{
    block_on,
    io::AsyncRead,
    net::{resolve, AsyncToSocketAddrs},
    spawn, Async,
};

use crate::constants::{CONFIG, FORWARD};

#[derive(Debug)]
pub struct Forward<'a> {
    domain: &'a HashMap<String, String>,
}

impl<'a> Forward<'a> {
    pub fn new(domain: &'a HashMap<String, String>) -> Forward<'a> {
        Forward { domain }
    }

    pub fn check_domain(&self) -> Result<()> {
        for i in self.domain.keys() {
            for j in self.domain.keys() {
                anyhow::ensure!(
                    !(j != i && j.contains(i)),
                    "conflict two domain \"{}\" and \"{}\"",
                    j,
                    i
                )
            }
        }
        Ok(())
    }

    async fn forward(&self, req: Request) -> http_types::Result<Response> {
        let mut req = req;

        match req.header("X-Web-Jingzi") {
            Some(_) => return http_error("may be circular request"),
            None => req.insert_header("X-Web-Jingzi", "true"),
        };

        let query: Vec<_> = req
            .url()
            .query_pairs()
            .map(|(q, v)| {
                let s = self.restore_domain(&v);
                format!("{}={}", q, s)
            })
            .collect();
        let query = query.join("&");
        let scheme = req.header("X-Scheme").map(|i| i.as_str().to_string());
        let path = req.url().path();
        let path = self.restore_domain(path);
        let url = req.url_mut();
        if !query.is_empty() {
            url.set_query(Some(&query));
        }
        if let Some(scheme) = scheme {
            if url.set_scheme(&scheme).is_err() {
                return http_error("invalid request");
            }
        }
        url.set_path(&path);
        if let Some(host) = url.host_str() {
            let host = self.restore_domain(host);
            url.set_host(Some(&host))?;
            req.insert_header("host", host);
        }
        self.restore_header(&mut req);

        if let Some(content_type) = req.content_type() {
            match content_type.essence() {
                "application/x-www-form-urlencoded" | "text/plain" => match req.body_string().await
                {
                    Ok(body) => {
                        let body = self.restore_domain(&body);
                        req.set_body(body);
                    }
                    Err(_) => error!("can not convert body to utf-8 string"),
                },
                _ => (),
            }
        }

        let host = match req.host().map(ToString::to_string) {
            Some(host) => host,
            None => return http_error("invalid request"),
        };
        let port = match req.url().port_or_known_default() {
            Some(port) => port,
            None => return http_error("invalid request"),
        };
        let stream = match &CONFIG.socks5_server {
            Some(server) => {
                let server = server.clone();
                let server = self.resolve(server).await?;
                trace!("socks5 dest: host: {}, port: {}", &host, port);
                socks5::connect_without_auth(server, (host.clone(), port).into()).await?
            }
            None => {
                let addr = format!("{}:{}", host, port);
                let addr = self.resolve(addr).await?;
                Async::<TcpStream>::connect(addr).await?
            }
        };

        let mut resp = match req.url().scheme() {
            "https" => {
                let stream = async_native_tls::connect(host, stream).await?;
                async_h1::connect(stream, req).await?
            }
            "http" => async_h1::connect(stream, req).await?,
            s => return http_error(&format!("unsupported scheme: {}", s)),
        };

        self.replace_header(&mut resp);

        if resp.status() == StatusCode::NotModified {
            return Ok(resp);
        }

        Coder::De.code(&mut resp);
        if let Some(content_type) = resp.content_type() {
            match content_type.essence() {
                "text/html"
                | "text/javascript"
                | "application/json"
                | "application/manifest+json" => match resp.body_string().await {
                    Ok(body) => {
                        let body = self.replace_domain(&body);
                        resp.set_body(body);
                    }
                    Err(_) => error!("can not convert body to utf-8 string"),
                },
                _ => (),
            }
        }
        Coder::En.code(&mut resp);
        Ok(resp)
    }

    fn replace_domain(&self, s: &str) -> String {
        let mut result = s.to_string();
        for (k, v) in self.domain {
            result = result.replace(v.as_str(), k);
        }
        result
    }

    fn restore_domain(&self, s: &str) -> String {
        let mut result = s.to_string();
        for (k, v) in self.domain {
            result = result.replace(k, v);
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
                let h = self.replace_domain(h.as_str());
                req.insert_header(*i, h);
            }
        }
    }

    fn restore_header(&self, req: &mut Request) {
        const HEADERS: &[&str] = &["origin", "referer"];

        for i in HEADERS {
            if let Some(h) = req.header(*i) {
                let h = self.restore_domain(h.as_str());
                req.insert_header(*i, h);
            }
        }
    }

    async fn resolve<T: AsyncToSocketAddrs>(&self, s: T) -> Result<SocketAddr> {
        Ok(*resolve(s)
            .await?
            .first()
            .ok_or_else(|| anyhow!("invalid address"))?)
    }
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
            match encoding {
                "gzip" => {
                    let body = resp.take_body();
                    match self {
                        Coder::En => Coder::set_body(resp, GzipEncoder::new(body)),
                        Coder::De => Coder::set_body(resp, GzipDecoder::new(body)),
                    }
                }
                "br" => {
                    let body = resp.take_body();
                    match self {
                        Coder::En => Coder::set_body(resp, BrotliEncoder::new(body)),
                        Coder::De => Coder::set_body(resp, BrotliDecoder::new(body)),
                    }
                }
                "deflate" => {
                    let body = resp.take_body();
                    match self {
                        Coder::En => Coder::set_body(resp, DeflateEncoder::new(body)),
                        Coder::De => Coder::set_body(resp, DeflateDecoder::new(body)),
                    }
                }
                e => error!("unhandled encoding: {}", e),
            }
        }
    }
}

fn http_error(error: &str) -> http_types::Result<Response> {
    let mut resp = Response::new(StatusCode::InternalServerError);
    resp.set_content_type(http_types::mime::PLAIN);
    resp.set_body(error);
    Ok(resp)
}

async fn serve(req: Request) -> http_types::Result<Response> {
    FORWARD.forward(req).await
}

pub fn run() -> Result<()> {
    FORWARD.check_domain()?;
    let listen_address: SocketAddr = CONFIG.listen_address.parse()?;
    block_on(async {
        let listener = Async::<TcpListener>::bind(listen_address)?;
        loop {
            let (stream, _) = listener.accept().await?;
            let stream = async_dup::Arc::new(stream);
            let task = spawn(async move {
                if let Err(err) = async_h1::accept(stream, serve).await {
                    error!("Connection error: {:#?}", err);
                }
            });
            task.detach();
        }
    })
}
