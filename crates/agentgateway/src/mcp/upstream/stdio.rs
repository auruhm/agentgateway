use std::collections::HashMap;
use std::fmt;
use std::fmt::{Debug, Formatter};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use agent_core::prelude::*;
use futures_util::TryFutureExt;
use rmcp::model::{
	ClientJsonRpcMessage, ClientNotification, ClientRequest, JsonRpcMessage, JsonRpcRequest,
	RequestId, ServerJsonRpcMessage,
};
use rmcp::transport::{TokioChildProcess, Transport};
use tokio::sync::mpsc::Sender;
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, error, warn};

use crate::mcp::mergestream::Messages;
use crate::mcp::upstream::{IncomingRequestContext, UpstreamError};

pub struct Process {
	sender: mpsc::Sender<(ClientJsonRpcMessage, IncomingRequestContext)>,
	shutdown_tx: agent_core::responsechannel::Sender<(), Option<UpstreamError>>,
	event_stream: AtomicOption<mpsc::Sender<ServerJsonRpcMessage>>,
	pending_requests: Arc<Mutex<HashMap<RequestId, oneshot::Sender<ServerJsonRpcMessage>>>>,
	alive: Arc<AtomicBool>,
}

impl Process {
	pub fn is_alive(&self) -> bool {
		self.alive.load(Ordering::Acquire)
	}

	pub async fn stop(&self) -> Result<(), UpstreamError> {
		if !self.is_alive() {
			return Ok(());
		}
		let res = self
			.shutdown_tx
			.send_and_wait(())
			.await
			.map_err(|_| UpstreamError::Send)?;
		if let Some(err) = res {
			Err(err)
		} else {
			Ok(())
		}
	}
	pub async fn send_message(
		&self,
		req: JsonRpcRequest<ClientRequest>,
		ctx: &IncomingRequestContext,
	) -> Result<ServerJsonRpcMessage, UpstreamError> {
		if !self.is_alive() {
			return Err(UpstreamError::Recv);
		}
		let req_id = req.id.clone();
		let (sender, receiver) = oneshot::channel();

		self
			.pending_requests
			.lock()
			.unwrap()
			.insert(req_id.clone(), sender);

		if self
			.sender
			.send((JsonRpcMessage::Request(req), ctx.clone()))
			.await
			.is_err()
		{
			self.pending_requests.lock().unwrap().remove(&req_id);
			return Err(UpstreamError::Send);
		}

		let response = receiver.await.map_err(|_| UpstreamError::Recv)?;
		Ok(response)
	}
	pub async fn get_event_stream(&self) -> Result<Messages, UpstreamError> {
		if !self.is_alive() {
			return Err(UpstreamError::Recv);
		}
		let (tx, rx) = tokio::sync::mpsc::channel(10);
		// This transport assumes a single active downstream event-stream consumer per
		// upstream session. Replacing the sender is acceptable for current MCP usage,
		// where one session owns one active GET/SSE stream, but it is not a general
		// multi-subscriber broadcast mechanism.
		self.event_stream.store(Some(Arc::new(tx)));
		Ok(Messages::from(rx))
	}
	pub async fn send_notification(
		&self,
		req: ClientNotification,
		ctx: &IncomingRequestContext,
	) -> Result<(), UpstreamError> {
		if !self.is_alive() {
			return Err(UpstreamError::Send);
		}
		self
			.sender
			.send((JsonRpcMessage::notification(req), ctx.clone()))
			.await
			.map_err(|_| UpstreamError::Send)?;
		Ok(())
	}

	pub async fn send_client_message(
		&self,
		msg: ClientJsonRpcMessage,
		ctx: &IncomingRequestContext,
	) -> Result<(), UpstreamError> {
		if !self.is_alive() {
			return Err(UpstreamError::Send);
		}
		self
			.sender
			.send((msg, ctx.clone()))
			.await
			.map_err(|_| UpstreamError::Send)?;
		Ok(())
	}
}

impl Process {
	pub fn new(mut proc: impl MCPTransport) -> Self {
		let (sender_tx, mut sender_rx) =
			mpsc::channel::<(ClientJsonRpcMessage, IncomingRequestContext)>(10);
		let (shutdown_tx, mut shutdown_rx) =
			agent_core::responsechannel::new::<(), Option<UpstreamError>>(10);
		let pending_requests = Arc::new(Mutex::new(HashMap::<
			RequestId,
			oneshot::Sender<ServerJsonRpcMessage>,
		>::new()));
		let pending_requests_clone = pending_requests.clone();
		let event_stream: AtomicOption<Sender<ServerJsonRpcMessage>> = Default::default();
		let event_stream_send: AtomicOption<Sender<ServerJsonRpcMessage>> = event_stream.clone();
		let alive = Arc::new(AtomicBool::new(true));
		let alive_task = alive.clone();

		tokio::spawn(async move {
			let mut terminal_err = None;
			let mut shutdown_resp = None;
			let mut gateway_context: Option<IncomingRequestContext> = None;
			loop {
				tokio::select! {
					req = sender_rx.recv() => match req {
						Some((msg, ctx)) => {
							if gateway_context.is_none()
								&& let JsonRpcMessage::Request(request) = &msg
								&& let ClientRequest::InitializeRequest(initialize) = &request.request
								&& initialize.extensions.get::<super::lazy::DeferredHandshake>().is_some() {
								gateway_context = Some(proc.service_reply_context(&ctx));
							}
							if let Err(e) = proc.send(msg, &ctx).await {
								error!("Error sending message to stdio process: {:?}", e);
								terminal_err = Some(e);
								break;
							}
						},
						None => break,
					},
					msg = proc.receive() => {
						match msg {
							Some(JsonRpcMessage::Request(request)) if gateway_context.is_some() => {
								// Нельзя ждать downstream GET: backend может ждать pong перед init result.
								let ctx = gateway_context.as_ref().expect("guarded gateway context");
								if let Err(error) = proc.send(super::lazy::handshake_reply(request), ctx).await {
									terminal_err = Some(error);
									break;
								}
							},
							Some(JsonRpcMessage::Response(res)) => {
								let req_id = res.id.clone();
								if let Some(sender) = pending_requests_clone.lock().unwrap().remove(&req_id) {
									let _ = sender.send(ServerJsonRpcMessage::Response(res));
								}
							},
							Some(JsonRpcMessage::Error(err)) => {
								// An error carrying a request id terminates that request, like a response.
								// An id-less error can't be matched, so it belongs on the event stream.
								match err.id.as_ref() {
									Some(id) => {
										if let Some(sender) = pending_requests_clone.lock().unwrap().remove(id) {
											let _ = sender.send(ServerJsonRpcMessage::Error(err));
										} else {
											debug!("dropping stdio error for unknown request id {id:?}");
										}
									},
									None => {
										if let Some(sender) = event_stream_send.load().as_ref() {
											let _ = sender.send(JsonRpcMessage::Error(err)).await;
										}
									},
								}
							},
							Some(other) => {
								if let Some(sender) = event_stream_send.load().as_ref() {
									let _ = sender.send(other).await;
								}
							},
							None => {
								terminal_err = Some(UpstreamError::StdioShutdown);
								break;
							}
						}
					},
					req = shutdown_rx.recv() => match req {
						Some((_, resp)) => {
							shutdown_resp = Some(resp);
							break;
						},
						None => break,
					},
				}
			}

			alive_task.store(false, Ordering::Release);
			event_stream_send.store(None);
			pending_requests_clone.lock().unwrap().clear();

			let close_err = proc.close().await.err();
			if let Some(e) = close_err.as_ref() {
				warn!("Error shutting down stdio process: {:?}", e);
			}
			if let Some(resp) = shutdown_resp {
				let _ = resp.send(terminal_err.or(close_err));
			}
		});

		Self {
			sender: sender_tx,
			shutdown_tx,
			event_stream,
			pending_requests,
			alive,
		}
	}
}

impl Debug for Process {
	fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
		f.debug_struct("Process").finish()
	}
}

pub trait MCPTransport: Send + 'static {
	/// SSE требует credentials/policy identity для ответного POST, но не user request snapshot.
	fn service_reply_context(&self, ctx: &IncomingRequestContext) -> IncomingRequestContext {
		ctx.for_service_reply()
	}

	/// Send a message to the transport
	///
	/// Notice that the future returned by this function should be `Send` and `'static`.
	/// It's because the sending message could be executed concurrently.
	fn send(
		&mut self,
		item: ClientJsonRpcMessage,
		user_headers: &IncomingRequestContext,
	) -> impl Future<Output = Result<(), UpstreamError>> + Send + 'static;

	/// Receive a message from the transport, this operation is sequential.
	fn receive(&mut self) -> impl Future<Output = Option<ServerJsonRpcMessage>> + Send;

	/// Close the transport
	fn close(&mut self) -> impl Future<Output = Result<(), UpstreamError>> + Send;
}

impl MCPTransport for TokioChildProcess {
	fn service_reply_context(&self, _: &IncomingRequestContext) -> IncomingRequestContext {
		// stdio не использует HTTP auth/policies: не удерживаем даже handshake credentials.
		IncomingRequestContext::new(&::http::Request::new(()).into_parts().0)
	}

	fn send(
		&mut self,
		item: ClientJsonRpcMessage,
		_: &IncomingRequestContext,
	) -> impl Future<Output = Result<(), UpstreamError>> + Send + 'static {
		Transport::send(self, item).map_err(Into::into)
	}

	fn receive(&mut self) -> impl Future<Output = Option<ServerJsonRpcMessage>> + Send {
		Transport::receive(self)
	}

	fn close(&mut self) -> impl Future<Output = Result<(), UpstreamError>> + Send {
		Transport::close(self).map_err(Into::into)
	}
}

#[cfg(test)]
mod tests {
	use futures_util::StreamExt;
	use rmcp::model::{ClientRequest, JsonRpcRequest, RequestId};
	use tokio::time::{Duration, timeout};

	use super::*;

	#[derive(Clone)]
	struct PayloadOwner(Arc<Vec<u8>>);
	impl AsRef<[u8]> for PayloadOwner {
		fn as_ref(&self) -> &[u8] {
			self.0.as_slice()
		}
	}

	#[test]
	fn service_reply_context_does_not_retain_body_or_request_extensions() {
		let parts = ::http::Request::builder()
			.uri("http://hub/mcp?payload=do-not-retain")
			.header("authorization", "Bearer handshake-test")
			.header("x-api-key", "handshake-test-key")
			.header("mcp-method", "tools/call")
			.header("mcp-param-arguments", "private-arguments")
			.header("content-length", "1048576")
			.body(())
			.unwrap()
			.into_parts()
			.0;
		let mut ctx = IncomingRequestContext::new(&parts);
		let payload = Arc::new(vec![7u8; 1024 * 1024]);
		let weak = Arc::downgrade(&payload);
		*ctx.request.body_mut() = Some(bytes::Bytes::from_owner(PayloadOwner(payload.clone())));
		ctx.extensions_mut().insert(PayloadOwner(payload.clone()));
		ctx.extensions_mut().insert(crate::mcp::MCPInfo::default());
		ctx.extensions_mut().insert(crate::http::basicauth::Claims {
			username: "validated-user".into(),
		});
		ctx
			.extensions_mut()
			.insert(crate::http::filters::BackendRequestTimeout(
				Duration::from_secs(3),
			));
		drop(payload);
		let minimal = ctx.for_service_reply();
		drop(ctx);
		assert!(
			weak.upgrade().is_none(),
			"idle service context retained request payload"
		);
		assert!(minimal.request.body().is_none());
		assert!(minimal.request.uri().query().is_none());
		assert!(minimal.extensions().get::<PayloadOwner>().is_none());
		assert!(minimal.extensions().get::<crate::mcp::MCPInfo>().is_none());
		assert_eq!(
			minimal.request.headers()["authorization"],
			"Bearer handshake-test"
		);
		assert_eq!(minimal.request.headers()["x-api-key"], "handshake-test-key");
		assert!(
			!minimal
				.request
				.headers()
				.contains_key("mcp-param-arguments")
		);
		assert!(!minimal.request.headers().contains_key("mcp-method"));
		assert!(!minimal.request.headers().contains_key("content-length"));
		assert_eq!(
			minimal
				.extensions()
				.get::<crate::http::basicauth::Claims>()
				.unwrap()
				.username
				.as_str(),
			"validated-user"
		);
		assert_eq!(
			minimal
				.extensions()
				.get::<crate::http::filters::BackendRequestTimeout>()
				.unwrap()
				.0,
			Duration::from_secs(3)
		);
	}

	struct InitAfterPong {
		tx: mpsc::Sender<ServerJsonRpcMessage>,
		rx: mpsc::Receiver<ServerJsonRpcMessage>,
		init_id: Option<RequestId>,
		tool_call: bool,
		expected_authorization: Option<&'static str>,
	}

	impl MCPTransport for InitAfterPong {
		fn send(
			&mut self,
			message: ClientJsonRpcMessage,
			ctx: &IncomingRequestContext,
		) -> impl Future<Output = Result<(), UpstreamError>> + Send + 'static {
			let response = match message {
				JsonRpcMessage::Request(request) => {
					self.tool_call = matches!(request.request, ClientRequest::CallToolRequest(_));
					if self.tool_call {
						assert!(
							ctx.request.body().is_some(),
							"real call lost its policy body"
						);
					}
					self.init_id = Some(request.id);
					serde_json::json!({"jsonrpc": "2.0", "id": "handshake-ping", "method": "ping"})
				},
				JsonRpcMessage::Response(response) => {
					assert_eq!(response.id, RequestId::String("handshake-ping".into()));
					assert!(ctx.request.body().is_none());
					assert!(ctx.extensions().get::<PayloadOwner>().is_none());
					assert!(!ctx.request.headers().contains_key("x-last-request"));
					if let Some(expected) = self.expected_authorization {
						assert_eq!(ctx.request.headers()["authorization"], expected);
					}
					let result = if self.tool_call {
						serde_json::json!({"content": []})
					} else {
						serde_json::json!({"protocolVersion": "2025-03-26", "capabilities": {"tools": {}}, "serverInfo": {"name": "pong-first", "version": "1"}})
					};
					serde_json::json!({"jsonrpc": "2.0", "id": self.init_id.take().unwrap(), "result": result})
				},
				_ => panic!("unexpected handshake message"),
			};
			let tx = self.tx.clone();
			async move {
				tx.send(serde_json::from_value(response).unwrap())
					.await
					.map_err(|_| UpstreamError::Send)
			}
		}
		async fn receive(&mut self) -> Option<ServerJsonRpcMessage> {
			self.rx.recv().await
		}
		async fn close(&mut self) -> Result<(), UpstreamError> {
			Ok(())
		}
	}

	#[tokio::test]
	async fn deferred_initialize_services_ping_without_downstream_event_stream() {
		let (tx, rx) = mpsc::channel(4);
		let process = Process::new(InitAfterPong {
			tx,
			rx,
			init_id: None,
			tool_call: false,
			expected_authorization: None,
		});
		let mut initialize = rmcp::model::InitializeRequest::new(Default::default());
		initialize
			.extensions
			.insert(super::super::lazy::DeferredHandshake);
		let response = timeout(
			Duration::from_millis(500),
			process.send_message(
				JsonRpcRequest::new(RequestId::Number(0), initialize.into()),
				&IncomingRequestContext::empty(),
			),
		)
		.await
		.expect("stdio/SSE handshake deadlocked waiting for pong")
		.unwrap();
		assert!(matches!(response, ServerJsonRpcMessage::Response(_)));
		process.stop().await.unwrap();
	}

	#[tokio::test]
	async fn service_pongs_keep_handshake_context_not_last_user_request() {
		let (tx, rx) = mpsc::channel(4);
		let process = Process::new(InitAfterPong {
			tx,
			rx,
			init_id: None,
			tool_call: false,
			expected_authorization: Some("Bearer handshake-test"),
		});
		let parts = ::http::Request::builder()
			.header("authorization", "Bearer handshake-test")
			.body(())
			.unwrap()
			.into_parts()
			.0;
		let handshake = IncomingRequestContext::new(&parts);
		let mut initialize = rmcp::model::InitializeRequest::new(Default::default());
		initialize
			.extensions
			.insert(super::super::lazy::DeferredHandshake);
		timeout(
			Duration::from_secs(1),
			process.send_message(
				JsonRpcRequest::new(RequestId::Number(0), initialize.into()),
				&handshake,
			),
		)
		.await
		.unwrap()
		.unwrap();
		let mut call_context = handshake.clone();
		call_context
			.headers_mut()
			.insert("authorization", "Bearer last-request-test".parse().unwrap());
		call_context
			.headers_mut()
			.insert("x-last-request", "must-not-retain".parse().unwrap());
		let payload = Arc::new(vec![3u8; 1024 * 1024]);
		let weak = Arc::downgrade(&payload);
		*call_context.request.body_mut() =
			Some(bytes::Bytes::from_owner(PayloadOwner(payload.clone())));
		call_context
			.extensions_mut()
			.insert(PayloadOwner(payload.clone()));
		drop(payload);
		let call = rmcp::model::CallToolRequest::new(rmcp::model::CallToolRequestParams::new("echo"));
		timeout(
			Duration::from_secs(1),
			process.send_message(
				JsonRpcRequest::new(RequestId::Number(1), call.into()),
				&call_context,
			),
		)
		.await
		.unwrap()
		.unwrap();
		drop(call_context);
		assert!(
			weak.upgrade().is_none(),
			"live idle transport retained last user request"
		);
		process.stop().await.unwrap();
	}

	struct FailOnSendTransport;

	impl MCPTransport for FailOnSendTransport {
		fn send(
			&mut self,
			_: ClientJsonRpcMessage,
			_: &IncomingRequestContext,
		) -> impl Future<Output = Result<(), UpstreamError>> + Send + 'static {
			std::future::ready(Err(UpstreamError::InvalidRequest("boom".to_string())))
		}

		fn receive(&mut self) -> impl Future<Output = Option<ServerJsonRpcMessage>> + Send {
			std::future::pending()
		}

		fn close(&mut self) -> impl Future<Output = Result<(), UpstreamError>> + Send {
			std::future::ready(Ok(()))
		}
	}

	struct ErrorReplyTransport {
		tx: mpsc::Sender<ServerJsonRpcMessage>,
		rx: mpsc::Receiver<ServerJsonRpcMessage>,
	}

	impl MCPTransport for ErrorReplyTransport {
		fn send(
			&mut self,
			item: ClientJsonRpcMessage,
			_: &IncomingRequestContext,
		) -> impl Future<Output = Result<(), UpstreamError>> + Send + 'static {
			let tx = self.tx.clone();
			async move {
				if let JsonRpcMessage::Request(req) = item {
					let err = rmcp::model::JsonRpcError::new(
						Some(req.id),
						rmcp::ErrorData::invalid_request(
							"connection is locked to the legacy handshake era",
							None,
						),
					);
					let _ = tx.send(ServerJsonRpcMessage::Error(err)).await;
				}
				Ok(())
			}
		}

		fn receive(&mut self) -> impl Future<Output = Option<ServerJsonRpcMessage>> + Send {
			self.rx.recv()
		}

		fn close(&mut self) -> impl Future<Output = Result<(), UpstreamError>> + Send {
			std::future::ready(Ok(()))
		}
	}

	#[tokio::test]
	async fn test_process_error_reply_resolves_pending_request() {
		let (tx, rx) = mpsc::channel(4);
		let proc = Process::new(ErrorReplyTransport { tx, rx });
		let req = JsonRpcRequest {
			jsonrpc: Default::default(),
			id: RequestId::Number(1),
			request: ClientRequest::PingRequest(Default::default()),
		};

		let resp = timeout(
			Duration::from_secs(1),
			proc.send_message(req, &IncomingRequestContext::empty()),
		)
		.await
		.expect("an error reply must resolve the pending request, not hang")
		.unwrap();

		match resp {
			ServerJsonRpcMessage::Error(e) => assert_eq!(e.id, Some(RequestId::Number(1))),
			other => panic!("expected error reply, got {other:?}"),
		}
	}

	#[tokio::test]
	async fn test_process_fails_pending_requests_when_transport_dies() {
		let proc = Process::new(FailOnSendTransport);
		let req = JsonRpcRequest {
			jsonrpc: Default::default(),
			id: RequestId::Number(1),
			request: ClientRequest::PingRequest(Default::default()),
		};

		let err = proc
			.send_message(req, &IncomingRequestContext::empty())
			.await
			.unwrap_err();

		assert!(matches!(err, UpstreamError::Recv));
		assert!(!proc.is_alive());
	}

	#[tokio::test]
	async fn test_process_closes_event_stream_when_transport_dies() {
		let proc = Process::new(FailOnSendTransport);
		let mut events = proc.get_event_stream().await.unwrap();
		let req = JsonRpcRequest {
			jsonrpc: Default::default(),
			id: RequestId::Number(1),
			request: ClientRequest::PingRequest(Default::default()),
		};

		let _ = proc
			.send_message(req, &IncomingRequestContext::empty())
			.await;

		let next = timeout(Duration::from_secs(1), events.next())
			.await
			.unwrap();
		assert!(next.is_none());
	}

	#[tokio::test]
	async fn test_process_rejects_new_event_stream_when_dead() {
		let proc = Process::new(FailOnSendTransport);
		let req = JsonRpcRequest {
			jsonrpc: Default::default(),
			id: RequestId::Number(1),
			request: ClientRequest::PingRequest(Default::default()),
		};

		let _ = proc
			.send_message(req, &IncomingRequestContext::empty())
			.await;

		let err = match proc.get_event_stream().await {
			Ok(_) => panic!("expected dead process to reject new event stream"),
			Err(err) => err,
		};
		assert!(matches!(err, UpstreamError::Recv));
	}
}
