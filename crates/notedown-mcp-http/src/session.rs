//! The MCP handler of the HTTP endpoint. The gate puts the account of the bearer token into
//! every request (`McpAccount`); each call runs on the `NotesServer` of that account. This
//! works for legacy sessions and for stateless requests (protocol 2026-07-28 and later), where
//! the factory builds a handler per request and `initialize` never runs.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use notedown_pipeline::mcp::NotesServer;
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, Implementation, ListToolsResult,
    PaginatedRequestParams, ServerCapabilities, ServerConfig, Tool,
};
use rmcp::service::RequestContext;
use rmcp::{ErrorData as McpError, RoleServer, ServerHandler};

use crate::gate::McpAccount;

/// The `NotesServer` of each account, shared by all handlers the factory builds.
#[derive(Clone, Default)]
pub(crate) struct Servers(Arc<Mutex<HashMap<String, NotesServer>>>);

pub(crate) struct AccountSession {
    data: PathBuf,
    servers: Servers,
}

impl AccountSession {
    pub(crate) fn new(data: PathBuf, servers: Servers) -> AccountSession {
        AccountSession { data, servers }
    }

    async fn server(&self, context: &RequestContext<RoleServer>) -> Result<NotesServer, McpError> {
        let slug = context
            .extensions
            .get::<axum::http::request::Parts>()
            .and_then(|p| p.extensions.get::<McpAccount>())
            .map(|a| a.0.clone())
            .ok_or_else(|| McpError::invalid_request("no account on this request", None))?;
        if let Some(s) = self
            .servers
            .0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&slug)
        {
            return Ok(s.clone());
        }
        let data = self.data.clone();
        let key = slug.clone();
        let server = tokio::task::spawn_blocking(move || NotesServer::new(&data, Some(&key)))
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        self.servers
            .0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(slug, server.clone());
        Ok(server)
    }
}

/// Like the instructions of `notedown mcp`, without the account name: `get_info` has no
/// request, and `status` names the account.
const INSTRUCTIONS: &str = "This server gives the handwritten notes of the notedown account you \
signed in with as text (status names it). The text is read from the pages by OCR. Every block of \
text has an anchor: notedown://<account>/<note>/<page>#<block>. get_changes shows what changed \
since the bookmark of a consumer; after the changes are ingested, ack moves the bookmark to upTo.\n\
Format: list-shaped results and the head of a delta arrive TOON-encoded. Indentation nests \
objects, and name[N]{f1,f2}: heads a uniform array with one comma-separated row per record. \
The texts of a delta follow as Markdown under headings that name the block.";

impl ServerHandler for AccountSession {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("notedown", env!("CARGO_PKG_VERSION")))
            .with_instructions(INSTRUCTIONS)
    }

    async fn list_tools(
        &self,
        request: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        let server = self.server(&context).await?;
        ServerHandler::list_tools(&server, request, context).await
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, McpError> {
        let server = self.server(&context).await?;
        ServerHandler::call_tool(&server, request, context).await
    }

    fn get_tool(&self, name: &str) -> Option<Tool> {
        // The tool list is the same for every account; any server answers it.
        self.servers
            .0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .next()
            .and_then(|s| ServerHandler::get_tool(s, name))
    }
}
