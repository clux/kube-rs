//! A basic API client with standard kube error handling

use serde_json::Value;
use either::{Right, Left};
use either::Either;
use http::StatusCode;
use http;
use serde::de::DeserializeOwned;
use serde_json;
use failure::ResultExt;
use futures::Stream;
use crate::{ApiError, Error, ErrorKind, Result};
use crate::config::Configuration;


#[allow(non_snake_case)]
#[derive(Deserialize, Debug)]
pub struct StatusDetails {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub name: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub group: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub kind: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub uid: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub causes: Vec<StatusCause>,
    #[serde(default, skip_serializing_if = "num::Zero::is_zero")]
    pub retryAfterSeconds: u32
}

#[derive(Deserialize, Debug)]
pub struct StatusCause {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub reason: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub message: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub field: String,
}

#[derive(Deserialize, Debug)]
pub struct Status {
    // TODO: typemeta
    // TODO: metadata that can be completely empty (listmeta...)
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub status: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub message: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub reason: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<StatusDetails>,
    #[serde(default, skip_serializing_if = "num::Zero::is_zero")]
    pub code: u16,
}

/// APIClient requires `config::Configuration` includes client to connect with kubernetes cluster.
#[derive(Clone)]
pub struct APIClient {
    configuration: Configuration,
}

impl APIClient {
    pub fn new(configuration: Configuration) -> Self {
        APIClient { configuration }
    }

    async fn send(&self, request: http::Request<Vec<u8>>) -> Result<reqwest::Response>
    {
        let (parts, body) = request.into_parts();
        let uri_str = format!("{}{}", self.configuration.base_path, parts.uri);
        trace!("{} {}", parts.method, uri_str);
        //trace!("Request body: {:?}", String::from_utf8_lossy(&body));
        let req = match parts.method {
            http::Method::GET => self.configuration.client.get(&uri_str),
            http::Method::POST => self.configuration.client.post(&uri_str),
            http::Method::DELETE => self.configuration.client.delete(&uri_str),
            http::Method::PUT => self.configuration.client.put(&uri_str),
            http::Method::PATCH => self.configuration.client.patch(&uri_str),
            other => Err(ErrorKind::InvalidMethod(other.to_string()))?
        }.headers(parts.headers).body(body).build().context(ErrorKind::RequestBuild)?;
        //trace!("Request Headers: {:?}", req.headers());
        let res = self.configuration.client.execute(req).await;
        Ok(res.context(ErrorKind::RequestSend)?)
    }


    pub async fn request<T>(&self, request: http::Request<Vec<u8>>) -> Result<T>
    where
        T: DeserializeOwned,
    {
        let res : reqwest::Response = self.send(request).await?;
        trace!("{} {}", res.status().as_str(), res.url());
        //trace!("Response Headers: {:?}", res.headers());
        let s = res.status();
        let text = res.text().await.context(ErrorKind::RequestParse)?;
        handle_api_errors(&text, &s)?;

        serde_json::from_str(&text).map_err(|e| {
            warn!("{}, {:?}", text, e);
            Error::from(ErrorKind::SerdeParse)
        })
    }

    pub async fn request_text(&self, request: http::Request<Vec<u8>>) -> Result<String>
    {
        let res : reqwest::Response = self.send(request).await?;
        trace!("{} {}", res.status().as_str(), res.url());
        //trace!("Response Headers: {:?}", res.headers());
        let s = res.status();
        let text = res.text().await.context(ErrorKind::RequestParse)?;
        handle_api_errors(&text, &s)?;

        Ok(text)
    }

    pub async fn request_status<T>(&self, request: http::Request<Vec<u8>>) -> Result<Either<T, Status>>
    where
        T: DeserializeOwned,
    {
        let res : reqwest::Response = self.send(request).await?;
        trace!("{} {}", res.status().as_str(), res.url());
        //trace!("Response Headers: {:?}", res.headers());
        let s = res.status();
        let text = res.text().await.context(ErrorKind::RequestParse)?;
        handle_api_errors(&text, &s)?;

        // It needs to be JSON:
        let v: Value = serde_json::from_str(&text).context(ErrorKind::SerdeParse)?;
        if v["kind"] == "Status" {
            trace!("Status from {}", text);
            Ok(Right(serde_json::from_str::<Status>(&text).map_err(|e| {
                warn!("{}, {:?}", text, e);
                Error::from(ErrorKind::SerdeParse)
            })?))
        } else {
            Ok(Left(serde_json::from_str::<T>(&text).map_err(|e| {
                warn!("{}, {:?}", text, e);
                Error::from(ErrorKind::SerdeParse)
            })?))
        }
    }

    pub async fn request_events<T>(&self, request: http::Request<Vec<u8>>) -> Result<Vec<T>>
    where
        T: DeserializeOwned,
    {
        let res : reqwest::Response = self.send(request).await?;
        trace!("{} {}", res.status().as_str(), res.url());
        //trace!("Response Headers: {:?}", res.headers());
        let s = res.status();
        let text = res.text().await.context(ErrorKind::RequestParse)?;
        handle_api_errors(&text, &s)?;

        // Should be able to coerce result into Vec<T> at this point
        let mut xs : Vec<T> = vec![];
        for l in text.lines() {
            let r = serde_json::from_str(&l).map_err(|e| {
                warn!("{} {:?}", l, e);
                Error::from(ErrorKind::SerdeParse)
            })?;
            xs.push(r);
        }
        Ok(xs)
    }

    pub fn unfold<T>(res: reqwest::Response) -> impl Stream<Item = Result<T>>
    where
        T: DeserializeOwned
    {
        futures::stream::unfold(res, |mut resp| async move {
            match resp.chunk().await {
                Ok(Some(l)) => {
                    trace!("Chunk: {:?}", l);
                    return match serde_json::from_slice(&l) {
                        Ok(t) => Some((Ok(t), resp)),
                        Err(e) => {
                            warn!("{} {:?}",  String::from_utf8_lossy(&l), e);
                            Some((Err(Error::from(ErrorKind::SerdeParse)), resp))
                        },
                    }
                },
                Ok(None) => None,
                Err(e) => {
                    warn!("{}: {:?}", e , e);
                    Some((Err(Error::from(ErrorKind::RequestSend)), resp))
                },
            }
        })
    }
}

/// Kubernetes returned error handling
///
/// Either kube returned an explicit ApiError struct,
/// or it someohow returned something we couldn't parse as one.
///
/// In either case, present an ApiError upstream.
/// The latter is probably a bug if encountered.
fn handle_api_errors(text: &str, s: &StatusCode) -> Result<()> {
    if s.is_client_error() || s.is_server_error() {
        // Print better debug when things do fail
        //trace!("Parsing error: {}", text);
        if let Ok(errdata) = serde_json::from_str::<ApiError>(text) {
            debug!("Unsuccessful: {:?}", errdata);
            Err(ErrorKind::Api(errdata).into())
        } else {
            warn!("Unsuccessful data error parse: {}", text);
            // Propagate errors properly via reqwest
            let ae = ApiError {
                status: s.to_string(),
                code: s.as_u16(),
                message: format!("{:?}", text),
                reason: "Failed to parse error data".into()
            };
            debug!("Unsuccessful: {:?} (reconstruct)", ae);
            Err(ErrorKind::Api(ae).into())
        }
    } else {
        Ok(())
    }
}
