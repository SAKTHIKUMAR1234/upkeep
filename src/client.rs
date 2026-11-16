use std::path::PathBuf;

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::{Method, Request, Response, header};
use hyper_util::rt::TokioIo;
use serde_json::Value;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::{TcpStream, UnixStream};

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("cannot connect to {target}: {source} (is `upkeep serve` running?)")]
    Connect {
        target: String,
        source: std::io::Error,
    },
    #[error("http: {0}")]
    Http(#[from] hyper::Error),
    #[error("request failed with {status}: {message}")]
    Status { status: u16, message: String },
    #[error("invalid response: {0}")]
    Json(#[from] serde_json::Error),
}

/// Where the runner API listens.
#[derive(Clone, Debug)]
pub enum Endpoint {
    Unix(PathBuf),
    Tcp {
        address: String,
        token: Option<String>,
    },
}

/// A small HTTP/1 client for the runner API, over the unix socket or TCP.
#[derive(Clone, Debug)]
pub struct Client {
    endpoint: Endpoint,
}

impl Client {
    pub fn new(endpoint: Endpoint) -> Self {
        Self { endpoint }
    }

    pub async fn get(&self, path: &str) -> Result<Value, ClientError> {
        json_body(self.send(Method::GET, path, None).await?).await
    }

    pub async fn post(&self, path: &str, body: &Value) -> Result<Value, ClientError> {
        json_body(self.send(Method::POST, path, Some(body)).await?).await
    }

    /// Passes each received chunk of a streamed response to `sink`.
    pub async fn stream(&self, path: &str, mut sink: impl FnMut(&[u8])) -> Result<(), ClientError> {
        let response = self.send(Method::GET, path, None).await?;
        let mut body = checked(response).await?.into_body();
        while let Some(frame) = body.frame().await {
            if let Some(chunk) = frame?.data_ref() {
                sink(chunk);
            }
        }
        Ok(())
    }

    pub async fn send(
        &self,
        method: Method,
        path: &str,
        body: Option<&Value>,
    ) -> Result<Response<Incoming>, ClientError> {
        let mut request = Request::builder()
            .method(method)
            .uri(path)
            .header(header::HOST, "upkeep")
            .header(header::CONTENT_TYPE, "application/json");
        if let Endpoint::Tcp {
            token: Some(token), ..
        } = &self.endpoint
        {
            request = request.header(header::AUTHORIZATION, format!("Bearer {token}"));
        }
        let payload = body.map(Value::to_string).unwrap_or_default();
        let request = request
            .body(Full::new(Bytes::from(payload)))
            .expect("request parts are valid");

        match &self.endpoint {
            Endpoint::Unix(path) => {
                let stream =
                    UnixStream::connect(path)
                        .await
                        .map_err(|source| ClientError::Connect {
                            target: path.display().to_string(),
                            source,
                        })?;
                exchange(stream, request).await
            }
            Endpoint::Tcp { address, .. } => {
                let stream =
                    TcpStream::connect(address)
                        .await
                        .map_err(|source| ClientError::Connect {
                            target: address.clone(),
                            source,
                        })?;
                exchange(stream, request).await
            }
        }
    }
}

async fn exchange<T>(
    stream: T,
    request: Request<Full<Bytes>>,
) -> Result<Response<Incoming>, ClientError>
where
    T: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let (mut sender, connection) =
        hyper::client::conn::http1::handshake(TokioIo::new(stream)).await?;
    tokio::spawn(connection);
    Ok(sender.send_request(request).await?)
}

/// Turns a non-2xx response into `ClientError::Status` with the API's error message.
async fn checked(response: Response<Incoming>) -> Result<Response<Incoming>, ClientError> {
    if response.status().is_success() {
        return Ok(response);
    }
    let status = response.status().as_u16();
    let bytes = response.into_body().collect().await?.to_bytes();
    let message = serde_json::from_slice::<Value>(&bytes)
        .ok()
        .and_then(|value| {
            value
                .get("error")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .unwrap_or_else(|| String::from_utf8_lossy(&bytes).into_owned());
    Err(ClientError::Status { status, message })
}

async fn json_body(response: Response<Incoming>) -> Result<Value, ClientError> {
    let bytes = checked(response)
        .await?
        .into_body()
        .collect()
        .await?
        .to_bytes();
    Ok(serde_json::from_slice(&bytes)?)
}
