use crate::{
    certificate_authority::CertificateAuthority, HttpContext, HttpHandler, RequestOrResponse,
    Rewind, WebSocketContext, WebSocketHandler,
};
use futures::{Sink, SinkExt, Stream, StreamExt};
use http::uri::{Authority, Scheme};
use hyper::{
    client::connect::Connect, header::Entry, server::conn::Http, service::service_fn,
    upgrade::Upgraded, Body, Client, Method, Request, Response, Uri,
};
use std::{future::Future, net::SocketAddr, sync::Arc};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite},
    task::JoinHandle,
    net::TcpStream,
};
use tokio_rustls::TlsAcceptor;
use tokio_tungstenite::{
    tungstenite::{self, Message},
    Connector, WebSocketStream,
};
use tracing::{error, info_span, instrument, Instrument, Span};

fn spawn_with_trace<T: Send + Sync + 'static>(
    fut: impl Future<Output = T> + Send + 'static,
    span: Span,
) -> JoinHandle<T> {
    tokio::spawn(fut.instrument(span))
}

pub(crate) struct InternalProxy<C, CA, H, W> {
    pub ca: Arc<CA>,
    pub client: Client<C>,
    pub http_handler: H,
    pub websocket_handler: W,
    pub websocket_connector: Option<Connector>,
    pub client_addr: SocketAddr,
}

impl<C, CA, H, W> Clone for InternalProxy<C, CA, H, W>
where
    C: Clone,
    H: Clone,
    W: Clone,
{
    fn clone(&self) -> Self {
        InternalProxy {
            ca: Arc::clone(&self.ca),
            client: self.client.clone(),
            http_handler: self.http_handler.clone(),
            websocket_handler: self.websocket_handler.clone(),
            websocket_connector: self.websocket_connector.clone(),
            client_addr: self.client_addr,
        }
    }
}

impl<C, CA, H, W> InternalProxy<C, CA, H, W>
where
    C: Connect + Clone + Send + Sync + 'static,
    CA: CertificateAuthority,
    H: HttpHandler,
    W: WebSocketHandler,
{
    #[instrument(
        skip_all,
        fields(
            version = ?req.version(),
            method = %req.method(),
            uri=%req.uri(),
            client_addr = %self.client_addr,
        )
    )]
    pub(crate) async fn proxy(
        mut self,
        req: Request<Body>,
    ) -> Result<Response<Body>, hyper::Error> {
        let ctx = HttpContext {
            client_addr: self.client_addr,
        };

        let req = match self
            .http_handler
            .handle_request(&ctx, req)
            .instrument(info_span!("handle_request"))
            .await
        {
            RequestOrResponse::Request(req) => req,
            RequestOrResponse::Response(res) => return Ok(res),
        };

        if req.method() == Method::CONNECT {
            self.process_connect(req)
        } else if hyper_tungstenite::is_upgrade_request(&req) {
            Ok(self.upgrade_websocket(req))
        } else {
            let res = self
                .client
                .request(normalize_request(req))
                .instrument(info_span!("proxy_request"))
                .await?;

            Ok(self
                .http_handler
                .handle_response(&ctx, res)
                .instrument(info_span!("handle_response"))
                .await)
        }
    }

    fn host_addr(self, uri: &http::Uri) -> Option<String> {
        uri.authority().map(|auth| auth.to_string())
    }

    async fn tunnel(mut upgraded: Upgraded, addr: String) -> std::io::Result<()> {
        let mut server = TcpStream::connect(addr).await?;
        tokio::io::copy_bidirectional(&mut upgraded, &mut server).await?;
        Ok(())
    }

    fn process_connect(self, mut req: Request<Body>) -> Result<Response<Body>, hyper::Error> {
        /*!req.uri().to_string().contains("apple-app-site")*/
        if false {
            let span = info_span!("process_connect_tunnel");
            let fut = async move {
                let addr = self.host_addr(req.uri()).unwrap();
                match hyper::upgrade::on(req).await {
                    Ok(upgraded) => {
                        match Self::tunnel(upgraded, addr).await {
                            Err(e) => {
                                error!("Failed to tunnel request: {}", e);
                                return;
                            }
                            _ => ()
                        }
                    }
                    Err(e) => {
                        error!("Failed to read from upgraded connection: {}", e);
                        return;
                    }
                }
            };
            spawn_with_trace(fut, span);
        } else {
            let span = info_span!("process_connect_proxy");
            let fut = async move {
                match hyper::upgrade::on(&mut req).await {
                    Ok(mut upgraded) => {
                        let mut buffer = [0; 4];
                        let bytes_read = match upgraded.read(&mut buffer).await {
                            Ok(bytes_read) => bytes_read,
                            Err(e) => {
                                error!("Failed to read from upgraded connection: {}", e);
                                return;
                            }
                        };

                        let upgraded = Rewind::new_buffered(
                            upgraded,
                            bytes::Bytes::copy_from_slice(buffer[..bytes_read].as_ref()),
                        );

                        if buffer == *b"GET " {
                            if let Err(e) = self.serve_stream(upgraded, Scheme::HTTP).await {
                                error!("Websocket connect error: {}", e);
                            }
                        } else if buffer[..2] == *b"\x16\x03" {
                            let authority = req
                                .uri()
                                .authority()
                                .expect("URI does not contain authority");

                            let server_config = self
                                .ca
                                .gen_server_config(authority)
                                .instrument(info_span!("gen_server_config"))
                                .await;

                            let stream =
                                match TlsAcceptor::from(server_config).accept(upgraded).await {
                                    Ok(stream) => stream,
                                    Err(e) => {
                                        error!("Failed to establish TLS connection: {}", e);
                                        return;
                                    }
                                };

                            if let Err(e) = self.serve_stream(stream, Scheme::HTTPS).await {
                                if !e.to_string().starts_with("error shutting down connection") {
                                    error!("HTTPS connect error: {}", e);
                                }
                            }
                        } else {
                            error!(
                                "Unknown protocol, read '{:02X?}' from upgraded connection",
                                &buffer[..bytes_read]
                            );
                        }
                    }
                    Err(e) => error!("Upgrade error: {}", e),
                };
            };
            spawn_with_trace(fut, span);
        }
        Ok(Response::new(Body::empty()))
    }

    #[instrument(skip_all)]
    fn upgrade_websocket(self, req: Request<Body>) -> Response<Body> {
        let mut req = {
            let (mut parts, _) = req.into_parts();

            parts.uri = {
                let mut parts = parts.uri.into_parts();

                parts.scheme = if parts.scheme.unwrap_or(Scheme::HTTP) == Scheme::HTTP {
                    Some("ws".try_into().expect("Failed to convert scheme"))
                } else {
                    Some("wss".try_into().expect("Failed to convert scheme"))
                };

                Uri::from_parts(parts).expect("Failed to build URI")
            };

            Request::from_parts(parts, ())
        };

        let (res, websocket) =
            hyper_tungstenite::upgrade(&mut req, None).expect("Request has missing headers");

        let span = info_span!("websocket");
        let fut = async move {
            match websocket.await {
                Ok(ws) => {
                    if let Err(e) = self.handle_websocket(ws, req).await {
                        error!("Failed to handle websocket: {}", e);
                    }
                }
                Err(e) => {
                    error!("Failed to upgrade to websocket: {}", e);
                }
            }
        };

        spawn_with_trace(fut, span);
        res
    }

    #[instrument(skip_all)]
    async fn handle_websocket(
        self,
        server_socket: WebSocketStream<Upgraded>,
        req: Request<()>,
    ) -> Result<(), tungstenite::Error> {
        let uri = req.uri().clone();

        #[cfg(any(feature = "rustls-client", feature = "native-tls-client"))]
        let (client_socket, _) =
            tokio_tungstenite::connect_async_tls_with_config(req, None, self.websocket_connector)
                .await?;

        #[cfg(not(any(feature = "rustls-client", feature = "native-tls-client")))]
        let (client_socket, _) = tokio_tungstenite::connect_async(req).await?;

        let (server_sink, server_stream) = server_socket.split();
        let (client_sink, client_stream) = client_socket.split();

        let InternalProxy {
            websocket_handler, ..
        } = self;

        spawn_message_forwarder(
            server_stream,
            client_sink,
            websocket_handler.clone(),
            WebSocketContext::ServerToClient {
                src: uri.clone(),
                dst: self.client_addr,
            },
        );

        spawn_message_forwarder(
            client_stream,
            server_sink,
            websocket_handler,
            WebSocketContext::ClientToServer {
                src: self.client_addr,
                dst: uri,
            },
        );

        Ok(())
    }

    #[instrument(skip_all)]
    async fn serve_stream<I>(self, stream: I, scheme: Scheme) -> Result<(), hyper::Error>
    where
        I: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let service = service_fn(|mut req| {
            if req.version() == hyper::Version::HTTP_10 || req.version() == hyper::Version::HTTP_11
            {
                let (mut parts, body) = req.into_parts();

                let authority = parts
                    .headers
                    .get(hyper::header::HOST)
                    .expect("Host is a required header")
                    .as_bytes();

                parts.uri = {
                    let mut parts = parts.uri.into_parts();
                    parts.scheme = Some(scheme.clone());
                    parts.authority =
                        Some(Authority::try_from(authority).expect("Failed to parse authority"));
                    Uri::from_parts(parts).expect("Failed to build URI")
                };

                req = Request::from_parts(parts, body);
            };

            self.clone().proxy(req)
        });

        Http::new()
            .serve_connection(stream, service)
            .with_upgrades()
            .await
    }
}

fn spawn_message_forwarder(
    mut stream: impl Stream<Item = Result<Message, tungstenite::Error>> + Unpin + Send + 'static,
    mut sink: impl Sink<Message, Error = tungstenite::Error> + Unpin + Send + 'static,
    mut handler: impl WebSocketHandler,
    ctx: WebSocketContext,
) {
    let span = info_span!("message_forwarder", context = ?ctx);
    let fut = async move {
        while let Some(message) = stream.next().await {
            match message {
                Ok(message) => {
                    let message = match handler.handle_message(&ctx, message).await {
                        Some(message) => message,
                        None => continue,
                    };

                    match sink.send(message).await {
                        Err(tungstenite::Error::ConnectionClosed) => (),
                        Err(e) => error!("Websocket send error: {}", e),
                        _ => (),
                    }
                }
                Err(e) => {
                    error!("Websocket message error: {}", e);

                    match sink.send(Message::Close(None)).await {
                        Err(tungstenite::Error::ConnectionClosed) => (),
                        Err(e) => error!("Websocket close error: {}", e),
                        _ => (),
                    };

                    break;
                }
            }
        }
    };

    spawn_with_trace(fut, span);
}

#[instrument(skip_all)]
fn normalize_request<T>(mut req: Request<T>) -> Request<T> {
    // Hyper will automatically add a Host header if needed.
    req.headers_mut().remove(hyper::header::HOST);

    // HTTP/2 supports multiple cookie headers, but HTTP/1.x only supports one.
    if let Entry::Occupied(mut cookies) = req.headers_mut().entry(hyper::header::COOKIE) {
        let joined_cookies = bstr::join(b"; ", cookies.iter());
        cookies.insert(joined_cookies.try_into().expect("Failed to join cookies"));
    }

    *req.version_mut() = hyper::Version::HTTP_11;
    req
}

#[cfg(test)]
mod tests {
    use super::*;

    mod normalize_request {
        use super::*;

        #[test]
        fn removes_host_header() {
            let req = Request::builder()
                .uri("http://example.com/")
                .header(hyper::header::HOST, "example.com")
                .body(())
                .unwrap();

            let req = normalize_request(req);

            assert_eq!(req.headers().get(hyper::header::HOST), None);
        }

        #[test]
        fn joins_cookies() {
            let req = Request::builder()
                .uri("http://example.com/")
                .header(hyper::header::COOKIE, "foo=bar")
                .header(hyper::header::COOKIE, "baz=qux")
                .body(())
                .unwrap();

            let req = normalize_request(req);

            assert_eq!(
                req.headers().get_all(hyper::header::COOKIE).iter().count(),
                1
            );

            assert_eq!(
                req.headers().get(hyper::header::COOKIE),
                Some(&"foo=bar; baz=qux".parse().unwrap())
            );
        }
    }
}
