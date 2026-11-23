//! `zportforward mcp`: an MCP server on stdio that proxies to the daemon, so an
//! editor's agent can list and forward ports.

use anyhow::Result;
use rmcp::handler::server::router::prompt::PromptRouter;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{
    CallToolResult, ContentBlock, Implementation, PromptMessage, Role, ServerCapabilities, ServerConfig,
};
use rmcp::{
    prompt, prompt_handler, prompt_router, schemars, tool, tool_handler, tool_router, ErrorData, ServerHandler,
    ServiceExt,
};
use serde::Deserialize;

use crate::control::{Client, Request, Response};

#[derive(Clone)]
struct Server {
    tool_router: ToolRouter<Self>,
    prompt_router: PromptRouter<Self>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct ForwardArgs {
    /// Port inside the dev container.
    port: u16,
    /// Host port to bind on 127.0.0.1. Defaults to the same number as `port`.
    host_port: Option<u16>,
    /// Container name or ID, as shown by list_ports. Needed only when several
    /// dev containers are running.
    container: Option<String>,
    /// Forward to this host as seen from inside the container (for example
    /// another compose service) instead of the container's own localhost.
    host: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct StopArgs {
    /// Port inside the dev container.
    port: u16,
    /// Container name or ID. Needed only when several dev containers are running.
    container: Option<String>,
}

async fn call(request: Request) -> Result<Response> {
    Client::connect_or_start().await?.request(&request).await
}

fn reply(result: Result<String>) -> std::result::Result<CallToolResult, ErrorData> {
    Ok(match result {
        Ok(text) => CallToolResult::success(vec![ContentBlock::text(text)]),
        Err(e) => CallToolResult::error(vec![ContentBlock::text(format!("{e:#}"))]),
    })
}

#[tool_router]
impl Server {
    fn new() -> Self {
        Self { tool_router: Self::tool_router(), prompt_router: Self::prompt_router() }
    }

    #[tool(
        description = "List running dev containers with their forwarded ports (container port, \
                       host port on 127.0.0.1, state) and other ports listening inside them."
    )]
    async fn list_ports(&self) -> std::result::Result<CallToolResult, ErrorData> {
        reply(async { Ok(serde_json::to_string_pretty(&call(Request::List).await?.containers)?) }.await)
    }

    #[tool(
        description = "Forward a port from a dev container to 127.0.0.1 on the host, so it can be \
                       opened in a local browser or client. Works for services bound to localhost \
                       inside the container."
    )]
    async fn forward_port(
        &self,
        Parameters(args): Parameters<ForwardArgs>,
    ) -> std::result::Result<CallToolResult, ErrorData> {
        let request = Request::Add {
            container: args.container,
            cwd: std::env::current_dir().ok(),
            port: args.port,
            host_port: args.host_port,
            host: args.host,
        };
        reply(async { Ok(call(request).await?.message.unwrap_or_default()) }.await)
    }

    #[tool(
        description = "Open the zportforward ports page in the user's browser: a live view of every dev \
                       container's forwarded ports, with controls to add and remove forwards. \
                       Returns the page's address."
    )]
    async fn open_ports_page(&self) -> std::result::Result<CallToolResult, ErrorData> {
        reply(
            async {
                let url = crate::ports_page_url().await?;
                Ok(match crate::open_browser(&url) {
                    Ok(()) => format!("Opened {url}"),
                    Err(e) => format!("The ports page is at {url} (could not open a browser: {e})"),
                })
            }
            .await,
        )
    }

    #[tool(description = "Stop forwarding a dev container port to the host.")]
    async fn stop_forward(
        &self,
        Parameters(args): Parameters<StopArgs>,
    ) -> std::result::Result<CallToolResult, ErrorData> {
        let request =
            Request::Remove { container: args.container, cwd: std::env::current_dir().ok(), port: args.port };
        reply(async { Ok(call(request).await?.message.unwrap_or_default()) }.await)
    }
}

#[prompt_router]
impl Server {
    /// Editors show prompts as slash commands, which makes this the closest
    /// thing to a "ports" action inside an agent thread: it opens the page
    /// as a side effect and hands the model a one-line note to acknowledge.
    #[prompt(name = "ports", description = "Open the zportforward ports page in the browser")]
    async fn ports(&self) -> Vec<PromptMessage> {
        let opened = async {
            let url = crate::ports_page_url().await?;
            crate::open_browser(&url)?;
            anyhow::Ok(url)
        };
        let text = match opened.await {
            Ok(url) => format!(
                "The zportforward ports page was just opened in my browser at {url}. \
                 Nothing else is needed: confirm in one short line and do not call any tools."
            ),
            Err(e) => format!(
                "Opening the zportforward ports page failed: {e:#}. \
                 Tell me in one or two lines what went wrong; do not call any tools."
            ),
        };
        vec![PromptMessage::new_text(Role::User, text)]
    }
}

#[tool_handler(router = self.tool_router)]
#[prompt_handler(router = self.prompt_router)]
impl ServerHandler for Server {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().enable_prompts().build())
            .with_server_info(Implementation::new("zportforward", env!("CARGO_PKG_VERSION")))
            .with_instructions(
                "Forwards ports from running dev containers to 127.0.0.1 on the user's machine. \
                 Ports listed in devcontainer.json `forwardPorts` are forwarded automatically; \
                 use forward_port for anything else the user wants to reach from the host.",
            )
    }
}

/// Bring the daemon up as soon as the editor starts this server, so the
/// ports from `forwardPorts` work without anyone asking for them.
async fn prepare_daemon() -> Result<()> {
    let mut client = Client::connect_or_start().await?;
    let daemon = client.request(&Request::List).await?.daemon;
    if daemon.version != env!("CARGO_PKG_VERSION") {
        // Left running from before an update: replace it with this version.
        client.stop().await?;
        client = Client::connect_or_start().await?;
    }
    if std::env::var_os("ZPORTFORWARD_AUTO").is_some() {
        client.request(&Request::SetAuto { on: true }).await?;
    }
    Ok(())
}

pub async fn run() -> Result<()> {
    tokio::spawn(async {
        if let Err(e) = prepare_daemon().await {
            crate::log!("could not prepare the daemon: {e:#}");
        }
    });
    let service = Server::new().serve(rmcp::transport::stdio()).await?;
    service.waiting().await?;
    Ok(())
}
