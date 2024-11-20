use anyhow::Result;
use async_trait::async_trait;
use reqwest::{header::HeaderMap, Client, Request, RequestBuilder, Response, ResponseBuilderExt};
use serde_json::{json, Value};
use tokio::time;
use std::collections::{HashMap, VecDeque};
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
    responses: Arc<RwLock<HashMap<String, VecDeque<Response>>>>,
    default_response: Arc<RwLock<Option<Box<dyn Fn() -> Response + Send + Sync + 'static>>>>,
}

/// Builder for configuring NullableClient
#[derive(Default)]
pub struct NullableClientBuilder {
    responses: HashMap<String, VecDeque<Response>>,
    default_response: Option<Box<dyn Fn() -> Response + Send + Sync + 'static>>,
}

impl NullableClientBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a single response for a specific URL
    pub fn response(mut self, url: impl Into<String>, response: Response) -> Self {
        self.responses
            .entry(url.into())
            .or_default()
            .push_back(response);
        self
    }

    /// Add multiple responses for a specific URL that will be returned in sequence
    pub fn responses(mut self, url: impl Into<String>, responses: Vec<Response>) -> Self {
        self.responses.insert(url.into(), responses.into());
        self
    }

    /// Set a default response generator for any unmatched URLs
    pub fn default_response(
        mut self,
        response: impl Fn() -> Response + Send + Sync + 'static,
    ) -> Self {
        self.default_response = Some(Box::new(response));
        self
    }

    /// Helper method to quickly add a JSON response
    pub fn json_response(
        self,
        url: impl Into<String>,
        status: u16,
        body: impl serde::Serialize,
    ) -> Result<Self> {
        let response = http::response::Builder::new()
            .status(status)
            .body(serde_json::to_string(&body)?)
            .unwrap()
            .into();
        Ok(self.response(url, response))
    }

    /// Helper method to quickly add a text response
    pub fn text_response(
        self,
        url: impl Into<String>,
        status: u16,
        body: impl Into<String>,
    ) -> Self {
        let response = http::response::Builder::new()
            .status(status)
            .body(body.into())
            .unwrap()
            .into();
        self.response(url, response)
    }

    pub fn build(self) -> NullableClient {
        NullableClient {
            responses: Arc::new(RwLock::new(self.responses.into_iter().map(|(k, v)| (k, v.into())).collect())),
            default_response: Arc::new(RwLock::new(self.default_response)),
        }
    }
}

impl NullableClient {
    pub fn builder() -> NullableClientBuilder {
        NullableClientBuilder::new()
    }
}

#[async_trait]
impl LightHttpClient for NullableClient {
    fn get(&self, url: &str) -> RequestBuilder {
        Client::new().get(url)
    }

    async fn execute(&self, request: Request) -> Result<Response> {
        time::sleep(Duration::from_millis(1)).await;
        let url = request.url().to_string();
        let mut responses = self.responses.write().await;
        
        if let Some(url_responses) = responses.get_mut(&url) {
            if let Some(response) = url_responses.pop_front() {
                // Remove the URL entry if no more responses
                if url_responses.is_empty() {
                    responses.remove(&url);
                }
                Ok(response)
            } else {
                if let Some(default_fn) = &*self.default_response.read().await {
                    Ok(default_fn())
                } else {
                    Err(anyhow::anyhow!("no response configured for URL: {}", url))
                }
            }
        } else if let Some(default_fn) = &*self.default_response.read().await {
            Ok(default_fn())
        } else {
            Err(anyhow::anyhow!("no response configured for URL: {}", url))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn test_nullable_with_builder() -> Result<()> {
        // Configure client using builder pattern
        let client = NullableClient::builder()
            .text_response("https://api.example.com/topic", 200, "ok")
            .json_response(
                "https://api.example.com/json",
                200,
                json!({ "status": "success" }),
            )?
            .default_response(|| {
                http::response::Builder::new()
                    .status(404)
                    .body("not found")
                    .unwrap()
                    .into()
            })
            .build();

        let http_client = HttpClient::new_nullable(client);
        let request_tracker = http_client.request_tracker().await;

        // Test successful text response
        let request = http_client.get("https://api.example.com/topic").build()?;
        let response = http_client.execute(request).await?;
        assert_eq!(response.status(), 200);
        assert_eq!(response.text().await?, "ok");

        // Test successful JSON response
        let request = http_client.get("https://api.example.com/json").build()?;
        let response = http_client.execute(request).await?;
        assert_eq!(response.status(), 200);
        assert_eq!(response.text().await?, r#"{"status":"success"}"#);

        // Test default response
        let request = http_client.get("https://api.example.com/unknown").build()?;
        let response = http_client.execute(request).await?;
        assert_eq!(response.status(), 404);
        assert_eq!(response.text().await?, "not found");

        // Verify recorded requests
        let requests = request_tracker.items().await;
        assert_eq!(requests.len(), 3);

        Ok(())
    }

    #[tokio::test]
    async fn test_sequence_of_responses() -> Result<()> {
        // Configure client with multiple responses for the same URL
        let client = NullableClient::builder()
            .responses(
                "https://api.example.com/sequence",
                vec![
                    http::response::Builder::new()
                        .status(200)
                        .body("first")
                        .unwrap()
                        .into(),
                    http::response::Builder::new()
                        .status(200)
                        .body("second")
                        .unwrap()
                        .into(),
                ],
            )
            .build();

        let http_client = HttpClient::new_nullable(client);

        // First request gets first response
        let request = http_client.get("https://api.example.com/sequence").build()?;
        let response = http_client.execute(request).await?;
        assert_eq!(response.text().await?, "first");

        // Second request gets second response
        let request = http_client.get("https://api.example.com/sequence").build()?;
        let response = http_client.execute(request).await?;
        assert_eq!(response.text().await?, "second");

        // Third request fails (no more responses)
        let request = http_client.get("https://api.example.com/sequence").build()?;
        let result = http_client.execute(request).await;
        assert!(result.is_err());

        Ok(())
    }
}