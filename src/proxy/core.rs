use std::convert::Infallible;
use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::atomic::AtomicU32;
use std::sync::Arc;
use std::task::Poll;

use crate::tls::{CertStore, CertVerifier};
use hyper::http::uri::{Authority, Scheme};
use hyper::server::conn::{AddrStream, Http};
use hyper::service::Service;
use hyper::upgrade::{self, Upgraded};
use hyper::{Body, Client, Method, Request, Response, Server, Uri};
use hyper::body::Bytes;
use rustls::{ServerConfig, ClientConfig};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc::{channel, Receiver, Sender};
use tokio::task::JoinHandle;
use tokio_rustls::{server::TlsStream, TlsAcceptor};
use tokio::{try_join, select};

use crate::proxy::ProxyEvent;

pub struct ProxyConfig {
    pub pubkey_path: String,
    pub privkey_path: String,
    pub listen: SocketAddr,
    pub starting_id: u32,
}

impl Default for ProxyConfig {
    fn default() -> Self {
        Self {
            pubkey_path: "data/cert".to_string(),
            privkey_path: "data/key".to_string(),
            listen: SocketAddr::from(([0, 0, 0, 0], 1337)),
            starting_id: 1, // Reserve id 0 for events not associated with requests
        }
    }
}

impl ProxyConfig {
    pub fn build(self) -> (ProxyServer, Receiver<ProxyEvent>) {
        ProxyServer::new(self)
    }
}

pub struct ProxyServer {
    listen: SocketAddr,
    events: Sender<ProxyEvent>,
    core: ProxyCore,
}

impl ProxyServer {
    pub fn new(conf: ProxyConfig) -> (Self, Receiver<ProxyEvent>) {
        let (tx, rx) = channel(128);
        let mut http_connector = hyper::client::HttpConnector::new();
        http_connector.enforce_http(false);
        let client_config = ClientConfig::builder()
            .with_safe_defaults()
            .with_custom_certificate_verifier(Arc::new(CertVerifier::new(tx.clone())))
            .with_no_client_auth();
        let client = hyper_rustls::HttpsConnectorBuilder::new()
            .with_tls_config(client_config)
            .https_or_http()
            .enable_http1()
            .build();
        (Self {
            listen: conf.listen,
            events: tx.clone(),
            core: ProxyCore {
                cert_store: Arc::new(CertStore::load_or_create(
                    &conf.pubkey_path,
                    &conf.privkey_path,
                )),
                channel: tx,
                id: Arc::new(AtomicU32::new(conf.starting_id)),
                fallback_host: None,
                client: Client::builder().build(client)
            },
        }, rx)
    }

    pub fn run(self) -> JoinHandle<Result<(), hyper::Error>> {
        tokio::spawn(Server::bind(&self.listen).serve(self))
    }
}

impl<'a> Service<&'a AddrStream> for ProxyServer {
    type Response = ProxyCore;

    type Error = Infallible;

    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(
        &mut self,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, _req: &'a AddrStream) -> Self::Future {
        let core = self.core.clone();
        let fut = async move { Ok(core) };
        Box::pin(fut)
    }
}

#[derive(Clone)]
pub struct ProxyCore {
    cert_store: Arc<CertStore>,
    channel: Sender<ProxyEvent>,
    id: Arc<AtomicU32>,
    fallback_host: Option<String>,
    client: Client<hyper_rustls::HttpsConnector<hyper::client::HttpConnector>, Body>,
}

impl Service<Request<Body>> for ProxyCore {
    type Response = Response<Body>;

    type Error = String;

    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(
        &mut self,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, mut req: Request<Body>) -> Self::Future {
        let proxy = self.clone();
        let host = req.uri().host().map(String::from);
        Box::pin(async move {
            if let &Method::CONNECT = req.method() {
                tokio::spawn(async move {
                    match upgrade::on(req).await {
                        Ok(upgraded) => {
                            proxy.do_tls_upgrade(upgraded, host).await.unwrap();
                        }
                        Err(e) => {
                            eprint!("Error upgrading connection: {}", e);
                        }
                    }
                });
                Ok(Response::default())
            } else {
                if let Some(host) = host.or(proxy.fallback_host) {
                    let id = proxy.id.fetch_add(1, crate::ORDERING);
                    let mut uri = req.uri().to_owned().into_parts();
                    uri.authority = Some(Authority::from_maybe_shared(host.clone()).unwrap());
                    if uri.scheme == None {
                        uri.scheme = Some(Scheme::HTTPS);
                    }
                    let uri = Uri::from_parts(uri).unwrap();
                    *req.uri_mut() = uri;
                    let (ser_req, req_upgrade) = super::request::Request::from_request(req, id, proxy.channel.clone()).await;
                    match proxy.client.request(ser_req.into()).await {
                        Err(e) => {
                            proxy.channel.send(super::ProxyEvent::err(id, e.to_string())).await.unwrap();
                            Ok(
                                Response::builder()
                                    .status(500)
                                    .body(hyper::Body::from("Internal Proxy Error"))
                                    .unwrap()
                            )
                        },
                        Ok(resp) => {
                            let (resp, resp_upgrade) = super::response::Response::from_response(resp, id, proxy.channel.clone()).await;
                            if let (Some(req_upgrade), Some(resp_upgrade)) = (req_upgrade, resp_upgrade) {
                                tokio::spawn( async move {
                                    println!("Both sides trying to upgrade, attempting");
                                    let chan = proxy.channel.clone();
                                    let chunk_id = AtomicU32::new(0);
                                    match try_join!(req_upgrade, resp_upgrade){
                                        Ok((mut req, mut resp)) => {
                                            chan.send(super::ProxyEvent::upgrade_open(id)).await.unwrap();
                                            loop {
                                                select!{
                                                    chunk = async {
                                                        let mut buf: [u8; 512] = [0; 512];
                                                        if let Ok(read) = req.read(&mut buf).await {
                                                            if read > 0 {
                                                                let bytes = Bytes::copy_from_slice(&buf[..read]);
                                                                let req_id = chunk_id.fetch_add(1, crate::ORDERING);
                                                                let (event, completion) = super::ProxyEvent::upgrade_tx(id, req_id, &bytes);
                                                                chan.send(event).await.unwrap();
                                                                match completion.await {
                                                                    Ok(super::ProxyState::UpgradeTx{id, chunk}) => Some(chunk),
                                                                    Ok(e) => {
                                                                        println!("Got unexpected result, ignoring: {:?}", e);
                                                                        Some(bytes)
                                                                    },
                                                                    Err(_) => {
                                                                        Some(bytes)
                                                                    }
                                                                }
                                                            } else {
                                                                None
                                                            }
                                                        } else {
                                                            None
                                                        }
                                                    } => {
                                                        if let Some(bytes) = chunk {
                                                            resp.write_all(&bytes).await.unwrap()
                                                        } else {
                                                            println!("Req disconnected, done");
                                                            break
                                                        }
                                                    },
                                                    chunk = async {
                                                        let mut buf: [u8; 512] = [0; 512];
                                                        if let Ok(read) = resp.read(&mut buf).await{
                                                            if read > 0 {
                                                                let bytes = Bytes::copy_from_slice(&buf[..read]);
                                                                let req_id = chunk_id.fetch_add(1, crate::ORDERING);
                                                                let (event, completion) = super::ProxyEvent::upgrade_rx(id, req_id, &bytes);
                                                                chan.send(event).await.unwrap();
                                                                match completion.await {
                                                                    Ok(super::ProxyState::UpgradeRx{id, chunk}) => Some(chunk),
                                                                    Ok(e) => {
                                                                        println!("Got unexpected result, ignoring: {:?}", e);
                                                                        Some(bytes)
                                                                    },
                                                                    Err(_) => {
                                                                        Some(bytes)
                                                                    }
                                                                }
                                                            } else {
                                                                None
                                                            }
                                                        } else {
                                                            None
                                                        }
                                                    } => {
                                                        if let Some(bytes) = chunk {
                                                            req.write_all(&bytes).await.unwrap()
                                                        } else {
                                                            println!("Resp disconnected, done");
                                                            break
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                        Err(e) => {
                                            eprintln!("Error upgrading: {}", e)
                                        }
                                    }
                                    chan.send(super::ProxyEvent::upgrade_close(id)).await.unwrap();
                                    println!("Done, closing socket");
                                });
                            };
                            Ok(resp.into())
                        }
                    }
                } else {
                    Err("No SNI or backup host".to_string())
                }
            }
        })
    }
}

impl ProxyCore {
    async fn do_tls_upgrade(&self, conn: Upgraded, fallback_host: Option<String>) -> hyper::Result<()>{
        let conf = Arc::new(
            ServerConfig::builder()
                .with_safe_default_cipher_suites()
                .with_safe_default_kx_groups()
                .with_safe_default_protocol_versions()
                .unwrap()
                .with_no_client_auth()
                .with_cert_resolver(CertStore::build_cert(
                    &self.cert_store,
                    fallback_host.to_owned(),
                )),
        );
        let accepted = TlsAcceptor::from(conf).accept(conn).await.unwrap();
        let mut service = self.clone();
        service.fallback_host = Self::get_host(&accepted, &fallback_host);
        Http::new().serve_connection(accepted, service).with_upgrades().await
    }

    fn get_host(conn: &TlsStream<Upgraded>, fallback_host: &Option<String>) -> Option<String> {
        conn.get_ref()
            .1
            .sni_hostname()
            .map(String::from)
            .or(fallback_host.to_owned())
    }
}