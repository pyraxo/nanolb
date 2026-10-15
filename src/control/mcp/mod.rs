mod tools;

use crate::app::App;
use crate::control::admin::{AdminError, dispatch};
use rmcp::{
    ErrorData, RoleServer, ServerHandler, ServiceExt,
    model::{
        CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, ListToolsResult, PaginatedRequestParams, ServerCapabilities,
        ServerConfig,
    },
    service::RequestContext,
    transport::{
        stdio,
        streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService, session::never::NeverSessionManager},
    },
};
use serde_json::Value;
use std::fmt::Display;
use std::future::Future;
use std::sync::Arc;
use tools::TOOLS;

pub(crate) trait Backend: Clone + Send + Sync + 'static {
    type Error: Display + Send;

    fn call(&self, op: String, args: Value) -> impl Future<Output = Result<Value, Self::Error>> + Send;
}

impl Backend for Arc<App> {
    type Error = AdminError;

    async fn call(&self, op: String, args: Value) -> Result<Value, AdminError> {
        dispatch(self, &op, args).await
    }
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum ServeError {
    #[error(transparent)]
    Init(Box<rmcp::service::ServerInitializeError>),
    #[error(transparent)]
    Join(#[from] tokio::task::JoinError),
}

#[derive(Clone)]
pub(crate) struct Handler<B> {
    backend: B,
}

impl<B: Backend> ServerHandler for Handler<B> {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_instructions("Admin tools for the nanolb LLM load balancer: manage accounts, API keys, routing settings and usage.")
    }

    async fn list_tools(&self, _: Option<PaginatedRequestParams>, _: RequestContext<RoleServer>) -> Result<ListToolsResult, ErrorData> {
        Ok(ListToolsResult::with_all_items(TOOLS.clone()))
    }

    async fn call_tool(&self, request: CallToolRequestParams, _: RequestContext<RoleServer>) -> Result<CallToolResponse, ErrorData> {
        let args = Value::Object(request.arguments.unwrap_or_default());
        let result = match self.backend.call(request.name.into_owned(), args).await {
            Ok(value) => CallToolResult::success(vec![ContentBlock::text(serde_json::to_string_pretty(&value).unwrap_or_default())]),
            Err(e) => CallToolResult::error(vec![ContentBlock::text(e.to_string())]),
        };
        Ok(result.into())
    }
}

pub(crate) async fn serve_stdio<B: Backend>(backend: B) -> Result<(), ServeError> {
    Handler { backend }
        .serve(stdio())
        .await
        .map_err(|e| ServeError::Init(Box::new(e)))?
        .waiting()
        .await?;
    Ok(())
}

pub(crate) fn http_service<B: Backend>(backend: B) -> StreamableHttpService<Handler<B>, NeverSessionManager> {
    StreamableHttpService::new(
        move || Ok(Handler { backend: backend.clone() }),
        Arc::new(NeverSessionManager::default()),
        StreamableHttpServerConfig::default()
            .with_legacy_session_mode(false)
            .with_json_response(true)
            .disable_allowed_hosts(),
    )
}
