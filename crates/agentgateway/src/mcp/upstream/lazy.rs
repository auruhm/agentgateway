//! Отдельная upstream negotiation для локального tools-фасада.
use std::sync::{Arc, RwLock};

use futures_util::StreamExt;
use rmcp::model::{
	ClientCapabilities, ClientJsonRpcMessage, ClientRequest, ClientResult, GetMeta, Implementation,
	InitializeRequest, InitializeRequestParams, JsonRpcRequest, ProtocolVersion, RequestId,
	ServerJsonRpcMessage, ServerRequest, ServerResult,
};

use super::{IncomingRequestContext, Upstream, UpstreamError, UpstreamGroup};
use crate::mcp::mergestream::Messages;
use crate::mcp::router::McpTarget;
use crate::proxy::httpproxy::PolicyClient;

/// Только внутренний initialize: transport отвечает на ping до подключения downstream stream.
#[derive(Clone)]
pub(super) struct DeferredHandshake;

#[derive(Debug)]
pub(crate) struct LazyUpstream {
	client: PolicyClient,
	target: Arc<McpTarget>,
	stateful: bool,
	pub(super) transport: tokio::sync::OnceCell<Arc<Upstream>>,
	initialized: tokio::sync::OnceCell<ProtocolVersion>,
	pub(super) connected: tokio::sync::watch::Sender<bool>,
	initialize: RwLock<Option<InitializeRequest>>,
}

impl LazyUpstream {
	pub(super) fn new(client: PolicyClient, target: Arc<McpTarget>, stateful: bool) -> Self {
		Self {
			client,
			target,
			stateful,
			transport: tokio::sync::OnceCell::new(),
			initialized: tokio::sync::OnceCell::new(),
			connected: tokio::sync::watch::channel(false).0,
			initialize: RwLock::new(None),
		}
	}

	pub(super) fn version(&self) -> Option<&ProtocolVersion> {
		self.initialized.get()
	}

	pub(super) fn defer_initialize(&self, initialize: InitializeRequest) {
		*self.initialize.write().expect("write lock") = Some(initialize);
	}

	pub(super) async fn ready(
		self: &Arc<Self>,
		ctx: &IncomingRequestContext,
	) -> Result<(&Arc<Upstream>, &ProtocolVersion), UpstreamError> {
		let transport = self
			.transport
			.get_or_try_init(|| async {
				UpstreamGroup::setup_upstream(&self.client, self.stateful, &self.target)
					.map(Arc::new)
					.map_err(|e| UpstreamError::Unavailable(e.to_string()))
			})
			.await?;
		let version = self
			.initialized
			.get_or_try_init(|| {
				let this = self.clone();
				let transport = transport.clone();
				let ctx = ctx.clone();
				async move {
					// Не вкладываем HTTP handshake в глубокий stateless -> search/fanout poll stack.
					// Отмена waiter должна отменять task, а не оставлять detached handshake.
					use tracing::Instrument;
					await_handshake(tokio::spawn(
						async move { this.negotiate(&transport, &ctx).await }.in_current_span(),
					))
					.await
				}
			})
			.await?;
		self.connected.send_replace(true);
		Ok((transport, version))
	}

	async fn negotiate(
		&self,
		transport: &Upstream,
		ctx: &IncomingRequestContext,
	) -> Result<ProtocolVersion, UpstreamError> {
		if matches!(transport, Upstream::OpenAPI(_)) {
			return Ok(ProtocolVersion::V_2026_07_28);
		}
		let initialize = self.initialize.read().expect("read lock").clone();
		if initialize.is_none() && matches!(transport, Upstream::McpStreamable(_)) {
			// Единственный fallback — discovery -> initialize. Tools/call никогда не пробуется повторно.
			let modern = ProtocolVersion::V_2026_07_28;
			let discovery: ClientRequest =
				serde_json::from_value(serde_json::json!({"method": "server/discover", "params": {}}))
					.map_err(|e| UpstreamError::InvalidRequest(e.to_string()))?;
			let mut discovery = JsonRpcRequest::new(RequestId::Number(0), discovery);
			let modern_ctx = adapt_request(&mut discovery, ctx, &modern)?;
			match self.exchange(transport, discovery, &modern_ctx).await {
				Ok(ServerResult::DiscoverResult(result)) if result.supported_versions.contains(&modern) => {
					return Ok(modern);
				},
				Ok(ServerResult::DiscoverResult(_)) => {},
				Err(error) if discovery_rejected(&error) => {},
				Err(error) => return Err(error),
				Ok(_) => {
					return Err(UpstreamError::Unavailable(
						"invalid deferred discovery response".into(),
					));
				},
			}
		}
		let mut params = initialize.map(|request| request.params).unwrap_or_else(|| {
			InitializeRequestParams::default().with_protocol_version(ProtocolVersion::V_2025_11_25)
		});
		// Фасад не транслирует client callbacks/MRTR: backend видит возможности gateway, не клиента.
		params.capabilities = ClientCapabilities::default();
		params.client_info = Implementation::new("agentgateway", env!("CARGO_PKG_VERSION"));
		let proposed = params.protocol_version.clone();
		let mut request = InitializeRequest::new(params);
		request.extensions.insert(DeferredHandshake);
		let handshake_ctx = backend_context(ctx, &proposed)?;
		let result = self
			.exchange(
				transport,
				JsonRpcRequest::new(RequestId::Number(0), request.into()),
				&handshake_ctx,
			)
			.await?;
		let ServerResult::InitializeResult(result) = result else {
			return Err(UpstreamError::Unavailable(
				"invalid deferred initialize response".into(),
			));
		};
		let version = result.protocol_version;
		if !ProtocolVersion::KNOWN_VERSIONS.contains(&version)
			|| crate::mcp::is_modern_version(&version)
		{
			return Err(UpstreamError::Unavailable(format!(
				"unsupported upstream initialize version {version}"
			)));
		}
		let negotiated_ctx = backend_context(ctx, &version)?;
		Box::pin(transport.generic_notification(
			self.target.name.as_str(),
			rmcp::model::InitializedNotification::default().into(),
			&negotiated_ctx,
		))
		.await?;
		Ok(version)
	}

	async fn exchange(
		&self,
		transport: &Upstream,
		request: JsonRpcRequest<ClientRequest>,
		ctx: &IncomingRequestContext,
	) -> Result<ServerResult, UpstreamError> {
		let id = request.id.clone();
		let mut stream = transport
			.generic_stream_boxed(self.target.name.as_str(), request, ctx)
			.await?;
		while let Some(message) = stream.next().await {
			match message? {
				ServerJsonRpcMessage::Response(response) if response.id == id => {
					return Ok(response.result);
				},
				ServerJsonRpcMessage::Error(error) if error.id.as_ref() == Some(&id) => {
					// Сохраняем код: fallback запрещён при auth/policy/прочих ошибках.
					return Err(UpstreamError::Handshake(error.error));
				},
				ServerJsonRpcMessage::Request(request) => {
					let reply = handshake_reply(request);
					Box::pin(transport.generic_client_message(self.target.name.as_str(), reply, ctx)).await?;
				},
				ServerJsonRpcMessage::Notification(_) => {},
				_ => {
					return Err(UpstreamError::Unavailable(
						"unmatched deferred handshake response id".into(),
					));
				},
			}
		}
		Err(UpstreamError::Recv)
	}
}

struct AbortHandshake(tokio::task::AbortHandle);

impl Drop for AbortHandshake {
	fn drop(&mut self) {
		self.0.abort();
	}
}

async fn await_handshake(
	task: tokio::task::JoinHandle<Result<ProtocolVersion, UpstreamError>>,
) -> Result<ProtocolVersion, UpstreamError> {
	let _abort_on_drop = AbortHandshake(task.abort_handle());
	task.await.map_err(|error| {
		UpstreamError::Unavailable(format!("deferred handshake task failed: {error}"))
	})?
}

pub(super) fn handshake_reply(
	request: rmcp::model::JsonRpcRequest<ServerRequest>,
) -> ClientJsonRpcMessage {
	if matches!(request.request, ServerRequest::PingRequest(_)) {
		ClientJsonRpcMessage::response(
			ClientResult::EmptyResult(rmcp::model::EmptyObject {}),
			request.id,
		)
	} else {
		ClientJsonRpcMessage::error(
			rmcp::ErrorData::new(
				rmcp::model::ErrorCode::METHOD_NOT_FOUND,
				"unsupported request during gateway handshake",
				None,
			),
			Some(request.id),
		)
	}
}

fn discovery_rejected(error: &UpstreamError) -> bool {
	match error {
		UpstreamError::Handshake(error) => error.code == rmcp::model::ErrorCode::METHOD_NOT_FOUND,
		UpstreamError::Http(crate::mcp::ClientError::Status(response)) => {
			matches!(response.status().as_u16(), 400 | 404 | 405)
		},
		UpstreamError::InvalidMethod(_) => true,
		_ => false,
	}
}

pub(super) fn backend_context(
	ctx: &IncomingRequestContext,
	version: &ProtocolVersion,
) -> Result<IncomingRequestContext, UpstreamError> {
	let mut ctx = ctx.clone();
	ctx.headers_mut().insert(
		"mcp-protocol-version",
		version
			.to_string()
			.parse()
			.map_err(|_| UpstreamError::InvalidRequest("invalid negotiated version header".into()))?,
	);
	Ok(ctx)
}

/// Меняются только protocol-owned поля; auth/policy inputs и пользовательская _meta сохраняются.
pub(super) fn adapt_request(
	request: &mut JsonRpcRequest<ClientRequest>,
	ctx: &IncomingRequestContext,
	version: &ProtocolVersion,
) -> Result<IncomingRequestContext, UpstreamError> {
	let modern = crate::mcp::is_modern_version(version);
	if !modern
		&& let ClientRequest::CallToolRequest(call) = &request.request
		&& (call.params.input_responses.is_some() || call.params.request_state.is_some())
	{
		return Err(UpstreamError::InvalidRequest(
			"legacy backend does not support inputResponses/requestState".into(),
		));
	}
	let meta = &mut request.request.get_meta_mut().0;
	for key in [
		"io.modelcontextprotocol/protocolVersion",
		"io.modelcontextprotocol/clientCapabilities",
		"io.modelcontextprotocol/clientInfo",
	] {
		meta.remove(key);
	}
	if modern {
		meta.insert(
			"io.modelcontextprotocol/protocolVersion".into(),
			serde_json::json!(version),
		);
		meta.insert(
			"io.modelcontextprotocol/clientCapabilities".into(),
			serde_json::json!({}),
		);
		meta.insert(
			"io.modelcontextprotocol/clientInfo".into(),
			serde_json::json!({"name": "agentgateway", "version": env!("CARGO_PKG_VERSION")}),
		);
	}
	backend_context(ctx, version)
}

/// Gateway не объявляет client callbacks backend-у: отвечает на ping, прочие запросы отклоняет.
pub(super) fn service_pings(
	stream: Messages,
	transport: Arc<Upstream>,
	target: String,
	ctx: IncomingRequestContext,
) -> Messages {
	Messages::from_stream(stream.filter_map(move |message| {
		let transport = transport.clone();
		let target = target.clone();
		let ctx = ctx.clone();
		async move {
			match message {
				Ok(ServerJsonRpcMessage::Request(request)) => {
					match Box::pin(transport.generic_client_message(&target, handshake_reply(request), &ctx))
						.await
					{
						Ok(()) => None,
						Err(error) => Some(Err(crate::mcp::ClientError::new(anyhow::anyhow!(
							error.to_string()
						)))),
					}
				},
				other => Some(other),
			}
		}
	}))
}

#[cfg(test)]
mod tests {
	use super::*;
	fn request() -> JsonRpcRequest<ClientRequest> {
		serde_json::from_value(serde_json::json!({"jsonrpc":"2.0", "id":1, "method":"tools/call", "params": {
   "name":"echo", "arguments": {}, "_meta": {"io.modelcontextprotocol/protocolVersion":"2026-07-28",
    "io.modelcontextprotocol/clientCapabilities": {}, "io.modelcontextprotocol/clientInfo": {"name":"client", "version":"1"},
    "progressToken":42, "tenant":"preserve"}
  }})).unwrap()
	}
	#[test]
	fn legacy_adapter_preserves_auth_and_user_meta_but_not_modern_envelope() {
		let parts = http::Request::builder()
			.header("authorization", "Bearer test-only")
			.header("mcp-protocol-version", "2026-07-28")
			.body(())
			.unwrap()
			.into_parts()
			.0;
		let ctx = IncomingRequestContext::new(&parts);
		let mut request = request();
		let backend = adapt_request(&mut request, &ctx, &ProtocolVersion::V_2025_03_26).unwrap();
		assert_eq!(
			backend.request.headers()["mcp-protocol-version"],
			"2025-03-26"
		);
		assert_eq!(
			backend.request.headers()["authorization"],
			ctx.request.headers()["authorization"]
		);
		assert_eq!(ctx.request.headers()["mcp-protocol-version"], "2026-07-28");
		let meta = request.request.get_meta();
		assert_eq!(meta.0.get("tenant").unwrap(), "preserve");
		assert_eq!(meta.0.get("progressToken").unwrap(), 42);
		assert!(
			!meta
				.0
				.contains_key("io.modelcontextprotocol/protocolVersion")
		);
		assert!(
			!meta
				.0
				.contains_key("io.modelcontextprotocol/clientCapabilities")
		);
	}
	#[test]
	fn modern_adapter_has_complete_gateway_envelope() {
		let mut request = request();
		adapt_request(
			&mut request,
			&IncomingRequestContext::empty(),
			&ProtocolVersion::V_2026_07_28,
		)
		.unwrap();
		assert!(
			request
				.request
				.get_meta()
				.missing_required_keys(&ProtocolVersion::V_2026_07_28)
				.is_empty()
		);
	}
	#[test]
	fn legacy_adapter_rejects_mrtr_before_semantic_send() {
		let mut request = request();
		let ClientRequest::CallToolRequest(call) = &mut request.request else {
			unreachable!()
		};
		call.params.request_state = Some("opaque-state".into());
		assert!(
			adapt_request(
				&mut request,
				&IncomingRequestContext::empty(),
				&ProtocolVersion::V_2025_03_26
			)
			.is_err()
		);
	}
	#[tokio::test]
	async fn handshake_waiter_cancellation_aborts_worker() {
		struct Released(Option<tokio::sync::oneshot::Sender<()>>);
		impl Drop for Released {
			fn drop(&mut self) {
				if let Some(sender) = self.0.take() {
					let _ = sender.send(());
				}
			}
		}
		let (started_tx, started_rx) = tokio::sync::oneshot::channel();
		let (released_tx, released_rx) = tokio::sync::oneshot::channel();
		let worker = tokio::spawn(async move {
			let _released = Released(Some(released_tx));
			let _ = started_tx.send(());
			std::future::pending::<Result<ProtocolVersion, UpstreamError>>().await
		});
		let mut waiter = Box::pin(await_handshake(worker));
		assert!(futures_util::poll!(waiter.as_mut()).is_pending());
		started_rx.await.unwrap();
		drop(waiter);
		tokio::time::timeout(std::time::Duration::from_secs(1), released_rx)
			.await
			.expect("cancelled waiter left a detached handshake task")
			.unwrap();
	}

	#[test]
	fn only_protocol_rejection_allows_discovery_fallback() {
		assert!(discovery_rejected(&UpstreamError::Handshake(
			rmcp::ErrorData::new(rmcp::model::ErrorCode::METHOD_NOT_FOUND, "legacy", None)
		)));
		assert!(!discovery_rejected(&UpstreamError::Handshake(
			rmcp::ErrorData::invalid_request("denied", None)
		)));
		assert!(!discovery_rejected(&UpstreamError::Unavailable(
			"network/auth failure".into()
		)));
	}
}
