use anyhow::Result;
use async_trait::async_trait;
use reqwest::{header::HeaderMap, Client, Request, RequestBuilder, Response, ResponseBuilderExt};
use serde_json::{json, Value};
use tokio::time;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;

use crate::models;
use crate::output_tracker::OutputTrackerAsync;

// Structure to store request information for verification
#[derive(Clone, Debug)]
pub struct RequestInfo {
    pub url: String,
    pub method: String,
    pub headers: HeaderMap,
    pub body: Option<Vec<u8>>,
}

impl RequestInfo {
    fn from_request(request: &Request) -> Self {
        RequestInfo {
            url: request.url().to_string(),
            method: request.method().to_string(),
            headers: request.headers().clone(),
            body: None, // Note: Request body can't be accessed after it's built
        }
    }
}

#[async_trait]
trait LightHttpClient: Send + Sync {
    fn get(&self, url: &str) -> RequestBuilder;
    async fn execute(&self, request: Request) -> Result<Response>;
}

#[async_trait]
impl LightHttpClient for Client {
    fn get(&self, url: &str) -> RequestBuilder {
        self.get(url)
    }

    async fn execute(&self, request: Request) -> Result<Response> {
        Ok(self.execute(request).await?)
    }
}

#[derive(Clone)]
pub struct HttpClient {
    client: Arc<dyn LightHttpClient>,
    request_tracker: OutputTrackerAsync<RequestInfo>,
}

impl HttpClient {
    pub fn new(client: reqwest::Client) -> Self {
        Self {
            client: Arc::new(client),
            request_tracker: Default::default(),
        }
    }
    pub fn new_nullable(client: NullableClient) -> Self {
        Self {
            client: Arc::new(client),
            request_tracker: Default::default(),
        }
    }

    pub async fn request_tracker(&self) -> OutputTrackerAsync<RequestInfo> {
        self.request_tracker.enable().await;
        self.request_tracker.clone()
    }

    pub fn get(&self, url: &str) -> RequestBuilder {
        self.client.get(url)
    }

    pub async fn execute(&self, request: Request) -> Result<Response> {
        self.request_tracker
            .push(RequestInfo::from_request(&request))
            .await;

        Ok(self.client.execute(request).await?)
    }
}

#[derive(Clone, Default)]
pub struct NullableClient {
    responses: Arc<RwLock<HashMap<String, Response>>>,
    default_response: Arc<RwLock<Option<Box<dyn Fn() -> Response + Send + Sync + 'static>>>>,
}

impl NullableClient {
    pub fn new() -> Self {
        Self::default()
    }

    pub async fn set_response(&self, url: &str, response: Response) {
        self.responses
            .write()
            .await
            .insert(url.to_string(), response);
    }

    pub async fn set_default_response(&self, res: Box<dyn Fn() -> Response + Send + Sync + 'static>) {
        *self.default_response.write().await = Some(res);
    }
}

#[async_trait]
impl LightHttpClient for NullableClient {
    fn get(&self, url: &str) -> RequestBuilder {
        Client::new().get(url)
    }

    async fn execute(&self, request: Request) -> Result<Response> {
        time::sleep(Duration::from_millis(1)).await; // else we spam the thread with responses
        // Get the configured response or return a default one
        let url = request.url().to_string();
        if let Some(response) = self.responses.write().await.remove(&url) {
            Ok(response)
        } else if let Some(res) = &*self.default_response.read().await {
            Ok(res())
        } else {
            Err(anyhow::anyhow!("no response"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_nullable() -> Result<()> {
        let client = NullableClient::new();

        // Configure mock response
        let mock_response = http::response::Builder::new()
            .status(200)
            .body("ok")
            .unwrap()
            .into();
        client
            .set_response("https://api.example.com/topic", mock_response)
            .await;

        let client = HttpClient::new_nullable(client);
        let request_tracker = client.request_tracker().await;

        let req = client
            .get("https://api.example.com/topic")
            .header("Content-Type", "application/x-ndjson")
            .header("Transfer-Encoding", "chunked")
            .build()
            .unwrap();

        // Execute request
        let response = client.execute(req).await?;

        assert_eq!(response.status(), 200);
        assert_eq!(response.bytes().await.unwrap(), b"ok"[..]);

        // Verify recorded requests
        let requests = request_tracker.items().await;
        assert_eq!(requests.len(), 1);

        let request = &requests[0];
        assert_eq!(request.method, "GET");
        assert_eq!(
            request.headers.get("Content-Type").unwrap(),
            "application/x-ndjson"
        );
        assert_eq!(request.headers.get("Transfer-Encoding").unwrap(), "chunked");

        Ok(())
    }

    #[tokio::test]
    async fn test_nullable_with_failing_response() -> Result<()> {
        let client = NullableClient::new();

        // Configure mock response
        let mock_response = http::response::Builder::new()
            .status(400)
            .body("fail")
            .unwrap()
            .into();
        client
            .set_response("https://api.example.com/topic", mock_response)
            .await;

        let req = client
            .get("https://api.example.com/topic")
            .header("Content-Type", "application/x-ndjson")
            .header("Transfer-Encoding", "chunked")
            .build()
            .unwrap();

        // Execute request
        let response = client.execute(req).await?;
        let response: Result<_, _> = response.error_for_status();

        dbg!(&response);
        assert!(matches!(response, Err(_)));

        Ok(())
    }
}
