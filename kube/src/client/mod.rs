//! A basic API client for interacting with the Kubernetes API
//!
//! The [`Client`] uses standard kube error handling.
//!
//! This client can be used on its own or in conjuction with the [`Api`][crate::api::Api]
//! type for more structured interaction with the kubernetes API.
//!
//! The [`Client`] can also be used with [`Discovery`](crate::Discovery) to dynamically
//! retrieve the resources served by the kubernetes API.

use std::convert::{TryFrom, TryInto};

use bytes::Bytes;
use either::{Either, Left, Right};
use futures::{self, Stream, StreamExt, TryStream, TryStreamExt};
use http::{self, HeaderValue, Request, Response, StatusCode};
use hyper::Body;
use hyper_timeout::TimeoutConnector;
use k8s_openapi::apimachinery::pkg::apis::meta::v1 as k8s_meta_v1;
pub use kube_core::response::Status;
use serde::de::DeserializeOwned;
use serde_json::{self, Value};
#[cfg(feature = "ws")]
use tokio_tungstenite::{tungstenite as ws, WebSocketStream};
use tokio_util::{
    codec::{FramedRead, LinesCodec, LinesCodecError},
    io::StreamReader,
};
use tower::{buffer::Buffer, util::BoxService, BoxError, Service, ServiceBuilder, ServiceExt};


#[cfg(feature = "gzip")]
use crate::service::{accept_compressed, maybe_decompress};
use crate::{
    api::WatchEvent,
    error::{ConfigError, ErrorResponse},
    service::{set_cluster_url, set_default_headers, AuthLayer, Authentication, HttpsConnector, LogRequest},
    Config, Error, Result,
};

// Binary subprotocol v4. See `Client::connect`.
#[cfg(feature = "ws")]
const WS_PROTOCOL: &str = "v4.channel.k8s.io";

/// Client for connecting with a Kubernetes cluster.
///
/// The best way to instantiate the client is either by
/// inferring the configuration from the environment using
/// [`Client::try_default`] or with an existing [`Config`]
/// using [`Client::try_from`].
#[derive(Clone)]
pub struct Client {
    // - `Buffer` for cheap clone
    // - `BoxService` for dynamic response future type
    inner: Buffer<BoxService<Request<Body>, Response<Body>, BoxError>, Request<Body>>,
    default_ns: String,
}

impl Client {
    /// Create and initialize a [`Client`] using the given `Service`.
    ///
    /// Use [`Client::try_from`](Self::try_from) to create with a [`Config`].
    pub fn new<S>(service: S) -> Self
    where
        S: Service<Request<Body>, Response = Response<Body>, Error = BoxError> + Send + 'static,
        S::Future: Send + 'static,
    {
        Self::new_with_default_ns(service, "default")
    }

    /// Create and initialize a [`Client`] using the given `Service` and the default namespace.
    fn new_with_default_ns<S, T: Into<String>>(service: S, default_ns: T) -> Self
    where
        S: Service<Request<Body>, Response = Response<Body>, Error = BoxError> + Send + 'static,
        S::Future: Send + 'static,
    {
        Self {
            inner: Buffer::new(BoxService::new(service), 1024),
            default_ns: default_ns.into(),
        }
    }

    /// Create and initialize a [`Client`] using the inferred
    /// configuration.
    ///
    /// Will use [`Config::infer`] to try in-cluster enironment
    /// variables first, then fallback to the local kubeconfig.
    ///
    /// Will fail if neither configuration could be loaded.
    ///
    /// If you already have a [`Config`] then use [`Client::try_from`](Self::try_from)
    /// instead.
    pub async fn try_default() -> Result<Self> {
        Self::try_from(Config::infer().await?)
    }

    pub(crate) fn default_ns(&self) -> &str {
        &self.default_ns
    }

    async fn send(&self, request: Request<Body>) -> Result<Response<Body>> {
        let mut svc = self.inner.clone();
        let res = svc
            .ready()
            .await
            .map_err(Error::Service)?
            .call(request)
            .await
            .map_err(|err| {
                if err.is::<Error>() {
                    // Error decorating request
                    *err.downcast::<Error>().expect("kube::Error")
                } else if err.is::<hyper::Error>() {
                    // Error requesting
                    Error::HyperError(*err.downcast::<hyper::Error>().expect("hyper::Error"))
                } else {
                    // Errors from other middlewares
                    Error::Service(err)
                }
            })?;
        Ok(res)
    }

    /// Make WebSocket connection.
    #[cfg(feature = "ws")]
    #[cfg_attr(docsrs, doc(cfg(feature = "ws")))]
    pub async fn connect(
        &self,
        request: Request<Vec<u8>>,
    ) -> Result<WebSocketStream<hyper::upgrade::Upgraded>> {
        use http::header::HeaderValue;
        let (mut parts, body) = request.into_parts();
        parts
            .headers
            .insert(http::header::CONNECTION, HeaderValue::from_static("Upgrade"));
        parts
            .headers
            .insert(http::header::UPGRADE, HeaderValue::from_static("websocket"));
        parts.headers.insert(
            http::header::SEC_WEBSOCKET_VERSION,
            HeaderValue::from_static("13"),
        );
        let key = sec_websocket_key();
        parts.headers.insert(
            http::header::SEC_WEBSOCKET_KEY,
            key.parse().expect("valid header value"),
        );
        // Use the binary subprotocol v4, to get JSON `Status` object in `error` channel (3).
        // There's no official documentation about this protocol, but it's described in
        // [`k8s.io/apiserver/pkg/util/wsstream/conn.go`](https://git.io/JLQED).
        // There's a comment about v4 and `Status` object in
        // [`kublet/cri/streaming/remotecommand/httpstream.go`](https://git.io/JLQEh).
        parts.headers.insert(
            http::header::SEC_WEBSOCKET_PROTOCOL,
            HeaderValue::from_static(WS_PROTOCOL),
        );

        let res = self.send(Request::from_parts(parts, Body::from(body))).await?;
        verify_upgrade_response(&res, &key)?;
        match hyper::upgrade::on(res).await {
            Ok(upgraded) => {
                Ok(WebSocketStream::from_raw_socket(upgraded, ws::protocol::Role::Client, None).await)
            }

            Err(e) => Err(Error::HyperError(e)),
        }
    }

    /// Perform a raw HTTP request against the API and deserialize the response
    /// as JSON to some known type.
    pub async fn request<T>(&self, request: Request<Vec<u8>>) -> Result<T>
    where
        T: DeserializeOwned,
    {
        let text = self.request_text(request).await?;

        serde_json::from_str(&text).map_err(|e| {
            tracing::warn!("{}, {:?}", text, e);
            Error::SerdeError(e)
        })
    }

    /// Perform a raw HTTP request against the API and get back the response
    /// as a string
    pub async fn request_text(&self, request: Request<Vec<u8>>) -> Result<String> {
        let res = self.send(request.map(Body::from)).await?;
        let status = res.status();
        // trace!("Status = {:?} for {}", status, res.url());
        let body_bytes = hyper::body::to_bytes(res.into_body()).await?;
        let text = String::from_utf8(body_bytes.to_vec())?;
        handle_api_errors(&text, status)?;

        Ok(text)
    }

    /// Perform a raw HTTP request against the API and get back the response
    /// as a stream of bytes
    pub async fn request_text_stream(
        &self,
        request: Request<Vec<u8>>,
    ) -> Result<impl Stream<Item = Result<Bytes>>> {
        let res = self.send(request.map(Body::from)).await?;
        // trace!("Status = {:?} for {}", res.status(), res.url());
        Ok(res.into_body().map_err(Error::HyperError))
    }

    /// Perform a raw HTTP request against the API and get back either an object
    /// deserialized as JSON or a [`Status`] Object.
    pub async fn request_status<T>(&self, request: Request<Vec<u8>>) -> Result<Either<T, Status>>
    where
        T: DeserializeOwned,
    {
        let text = self.request_text(request).await?;
        // It needs to be JSON:
        let v: Value = serde_json::from_str(&text)?;
        if v["kind"] == "Status" {
            tracing::trace!("Status from {}", text);
            Ok(Right(serde_json::from_str::<Status>(&text).map_err(|e| {
                tracing::warn!("{}, {:?}", text, e);
                Error::SerdeError(e)
            })?))
        } else {
            Ok(Left(serde_json::from_str::<T>(&text).map_err(|e| {
                tracing::warn!("{}, {:?}", text, e);
                Error::SerdeError(e)
            })?))
        }
    }

    /// Perform a raw request and get back a stream of [`WatchEvent`] objects
    pub async fn request_events<T>(
        &self,
        request: Request<Vec<u8>>,
    ) -> Result<impl TryStream<Item = Result<WatchEvent<T>>>>
    where
        T: Clone + DeserializeOwned,
    {
        let res = self.send(request.map(Body::from)).await?;
        // trace!("Streaming from {} -> {}", res.url(), res.status().as_str());
        tracing::trace!("headers: {:?}", res.headers());

        let frames = FramedRead::new(
            StreamReader::new(res.into_body().map_err(|e| {
                // Client timeout. This will be ignored.
                if e.is_timeout() {
                    return std::io::Error::new(std::io::ErrorKind::TimedOut, e);
                }
                // Unexpected EOF from chunked decoder.
                // Tends to happen when watching for 300+s. This will be ignored.
                if e.to_string().contains("unexpected EOF during chunk") {
                    return std::io::Error::new(std::io::ErrorKind::UnexpectedEof, e);
                }
                std::io::Error::new(std::io::ErrorKind::Other, e)
            })),
            LinesCodec::new(),
        );

        Ok(frames.filter_map(|res| async {
            match res {
                Ok(line) => match serde_json::from_str::<WatchEvent<T>>(&line) {
                    Ok(event) => Some(Ok(event)),
                    Err(e) => {
                        // Ignore EOF error that can happen for incomplete line from `decode_eof`.
                        if e.is_eof() {
                            return None;
                        }

                        // Got general error response
                        if let Ok(e_resp) = serde_json::from_str::<ErrorResponse>(&line) {
                            return Some(Err(Error::Api(e_resp)));
                        }
                        // Parsing error
                        Some(Err(Error::SerdeError(e)))
                    }
                },

                Err(LinesCodecError::Io(e)) => match e.kind() {
                    // Client timeout
                    std::io::ErrorKind::TimedOut => {
                        tracing::warn!("timeout in poll: {}", e); // our client timeout
                        None
                    }
                    // Unexpected EOF from chunked decoder.
                    // Tends to happen after 300+s of watching.
                    std::io::ErrorKind::UnexpectedEof => {
                        tracing::warn!("eof in poll: {}", e);
                        None
                    }
                    _ => Some(Err(Error::ReadEvents(e))),
                },

                // Reached the maximum line length without finding a newline.
                // This should never happen because we're using the default `usize::MAX`.
                Err(LinesCodecError::MaxLineLengthExceeded) => {
                    Some(Err(Error::LinesCodecMaxLineLengthExceeded))
                }
            }
        }))
    }
}

/// Low level discovery methods using `k8s_openapi` types.
///
/// Consider using the [`discovery`](crate::discovery) module for
/// easier-to-use variants of this functionality.
impl Client {
    /// Returns apiserver version.
    pub async fn apiserver_version(&self) -> Result<k8s_openapi::apimachinery::pkg::version::Info> {
        self.request(Request::builder().uri("/version").body(vec![])?)
            .await
    }

    /// Lists api groups that apiserver serves.
    #[deprecated(since = "0.56.0", note = "Replaced by discovery module")]
    pub async fn list_api_groups(&self) -> Result<k8s_meta_v1::APIGroupList> {
        self.request(Request::builder().uri("/apis").body(vec![])?).await
    }

    /// Lists resources served in given API group.
    ///
    /// ### Example usage:
    /// ```rust
    /// # async fn scope(client: kube::Client) -> Result<(), Box<dyn std::error::Error>> {
    /// let apigroups = client.list_api_groups().await?;
    /// for g in apigroups.groups {
    ///     let ver = g
    ///         .preferred_version
    ///         .as_ref()
    ///         .or_else(|| g.versions.first())
    ///         .expect("preferred or versions exists");
    ///     let apis = client.list_api_group_resources(&ver.group_version).await?;
    ///     dbg!(apis);
    /// }
    /// # Ok(())
    /// # }
    /// ```
    #[deprecated(since = "0.56.0", note = "Replaced by discovery module")]
    pub async fn list_api_group_resources(&self, apiversion: &str) -> Result<k8s_meta_v1::APIResourceList> {
        let url = format!("/apis/{}", apiversion);
        self.request(Request::builder().uri(url).body(vec![])?).await
    }

    /// Lists versions of `core` a.k.a. `""` legacy API group.
    #[deprecated(since = "0.56.0", note = "Replaced by discovery module")]
    pub async fn list_core_api_versions(&self) -> Result<k8s_meta_v1::APIVersions> {
        self.request(Request::builder().uri("/api").body(vec![])?).await
    }

    /// Lists resources served in particular `core` group version.
    #[deprecated(since = "0.56.0", note = "Replaced by discovery module")]
    pub async fn list_core_api_resources(&self, version: &str) -> Result<k8s_meta_v1::APIResourceList> {
        let url = format!("/api/{}", version);
        self.request(Request::builder().uri(url).body(vec![])?).await
    }
}

/// Kubernetes returned error handling
///
/// Either kube returned an explicit ApiError struct,
/// or it someohow returned something we couldn't parse as one.
///
/// In either case, present an ApiError upstream.
/// The latter is probably a bug if encountered.
fn handle_api_errors(text: &str, s: StatusCode) -> Result<()> {
    if s.is_client_error() || s.is_server_error() {
        // Print better debug when things do fail
        // trace!("Parsing error: {}", text);
        if let Ok(errdata) = serde_json::from_str::<ErrorResponse>(text) {
            tracing::debug!("Unsuccessful: {:?}", errdata);
            Err(Error::Api(errdata))
        } else {
            tracing::warn!("Unsuccessful data error parse: {}", text);
            let ae = ErrorResponse {
                status: s.to_string(),
                code: s.as_u16(),
                message: format!("{:?}", text),
                reason: "Failed to parse error data".into(),
            };
            tracing::debug!("Unsuccessful: {:?} (reconstruct)", ae);
            Err(Error::Api(ae))
        }
    } else {
        Ok(())
    }
}

impl TryFrom<Config> for Client {
    type Error = Error;

    /// Convert [`Config`] into a [`Client`]
    fn try_from(config: Config) -> Result<Self> {
        let cluster_url = config.cluster_url.clone();
        let mut default_headers = config.headers.clone();
        let timeout = config.timeout;
        let default_ns = config.default_ns.clone();

        // AuthLayer is not necessary unless `RefreshableToken`
        let maybe_auth = match Authentication::try_from(&config.auth_info)? {
            Authentication::None => None,
            Authentication::Basic(s) => {
                let mut value =
                    HeaderValue::try_from(format!("Basic {}", &s)).map_err(ConfigError::InvalidBasicAuth)?;
                value.set_sensitive(true);
                default_headers.insert(http::header::AUTHORIZATION, value);
                None
            }
            Authentication::Token(s) => {
                let mut value = HeaderValue::try_from(format!("Bearer {}", &s))
                    .map_err(ConfigError::InvalidBearerToken)?;
                value.set_sensitive(true);
                default_headers.insert(http::header::AUTHORIZATION, value);
                None
            }
            Authentication::RefreshableToken(r) => Some(AuthLayer::new(r)),
        };

        let common = ServiceBuilder::new()
            .map_request(move |r| set_cluster_url(r, &cluster_url))
            .map_request(move |r| set_default_headers(r, default_headers.clone()))
            .into_inner();

        #[cfg(feature = "gzip")]
        let common = ServiceBuilder::new()
            .layer(common)
            .map_request(accept_compressed)
            .map_response(maybe_decompress)
            .into_inner();

        let https: HttpsConnector<_> = config.try_into()?;
        let mut connector = TimeoutConnector::new(https);
        if let Some(timeout) = timeout {
            // reqwest's timeout is applied from when the request stars connecting until
            // the response body has finished.
            // Setting both connect and read timeout should be close enough.
            connector.set_connect_timeout(Some(timeout));
            // Timeout for reading the response.
            connector.set_read_timeout(Some(timeout));
        }
        let client: hyper::Client<_, Body> = hyper::Client::builder().build(connector);

        let inner = ServiceBuilder::new()
            .layer(common)
            .option_layer(maybe_auth)
            .layer(tower::layer::layer_fn(LogRequest::new))
            .service(client);
        Ok(Self::new_with_default_ns(inner, default_ns))
    }
}

#[cfg(feature = "ws")]
// Verify upgrade response according to RFC6455.
// Based on `tungstenite` and added subprotocol verification.
fn verify_upgrade_response(res: &Response<Body>, key: &str) -> Result<()> {
    if res.status() != StatusCode::SWITCHING_PROTOCOLS {
        return Err(Error::ProtocolSwitch(res.status()));
    }

    let headers = res.headers();
    if !headers
        .get(http::header::UPGRADE)
        .and_then(|h| h.to_str().ok())
        .map(|h| h.eq_ignore_ascii_case("websocket"))
        .unwrap_or(false)
    {
        return Err(Error::MissingUpgradeWebSocketHeader);
    }

    if !headers
        .get(http::header::CONNECTION)
        .and_then(|h| h.to_str().ok())
        .map(|h| h.eq_ignore_ascii_case("Upgrade"))
        .unwrap_or(false)
    {
        return Err(Error::MissingConnectionUpgradeHeader);
    }

    let accept_key = ws::handshake::derive_accept_key(key.as_ref());
    if !headers
        .get(http::header::SEC_WEBSOCKET_ACCEPT)
        .map(|h| h == &accept_key)
        .unwrap_or(false)
    {
        return Err(Error::SecWebSocketAcceptKeyMismatch);
    }

    // Make sure that the server returned the correct subprotocol.
    if !headers
        .get(http::header::SEC_WEBSOCKET_PROTOCOL)
        .map(|h| h == WS_PROTOCOL)
        .unwrap_or(false)
    {
        return Err(Error::SecWebSocketProtocolMismatch);
    }

    Ok(())
}

/// Generate a random key for the `Sec-WebSocket-Key` header.
/// This must be nonce consisting of a randomly selected 16-byte value in base64.
#[cfg(feature = "ws")]
fn sec_websocket_key() -> String {
    let r: [u8; 16] = rand::random();
    base64::encode(&r)
}

#[cfg(test)]
mod tests {
    use crate::{Api, Client};

    use futures::pin_mut;
    use http::{Request, Response};
    use hyper::Body;
    use k8s_openapi::api::core::v1::Pod;
    use tower_test::mock;

    #[tokio::test]
    async fn test_mock() {
        let (mock_service, handle) = mock::pair::<Request<Body>, Response<Body>>();
        let spawned = tokio::spawn(async move {
            // Receive a request for pod and respond with some data
            pin_mut!(handle);
            let (request, send) = handle.next_request().await.expect("service not called");
            assert_eq!(request.method(), http::Method::GET);
            assert_eq!(request.uri().to_string(), "/api/v1/namespaces/default/pods/test");
            let pod: Pod = serde_json::from_value(serde_json::json!({
                "apiVersion": "v1",
                "kind": "Pod",
                "metadata": {
                    "name": "test",
                    "annotations": { "kube-rs": "test" },
                },
                "spec": {
                    "containers": [{ "name": "test", "image": "test-image" }],
                }
            }))
            .unwrap();
            send.send_response(
                Response::builder()
                    .body(Body::from(serde_json::to_vec(&pod).unwrap()))
                    .unwrap(),
            );
        });

        let pods: Api<Pod> = Api::namespaced(Client::new(mock_service), "default");
        let pod = pods.get("test").await.unwrap();
        assert_eq!(pod.metadata.annotations.unwrap().get("kube-rs").unwrap(), "test");
        spawned.await.unwrap();
    }
}
