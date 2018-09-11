// Copyright 2017 Parity Technologies (UK) Ltd.
// This file is part of Substrate.

// Substrate is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Substrate is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Substrate.  If not, see <http://www.gnu.org/licenses/>.

//! On-demand requests service.

use std::collections::VecDeque;
use std::sync::{Arc, Weak};
use std::time::{Instant, Duration};
use futures::{Async, Future, Poll};
use futures::sync::oneshot::{channel, Receiver, Sender};
use linked_hash_map::LinkedHashMap;
use linked_hash_map::Entry;
use parking_lot::Mutex;
use client;
use client::light::fetcher::{Fetcher, FetchChecker, RemoteHeaderRequest,
	RemoteCallRequest, RemoteReadRequest};
use io::SyncIo;
use message;
use network_libp2p::{Severity, NodeIndex};
use service;
use runtime_primitives::traits::{Chain, Block as BlockT, Header as HeaderT};

/// Remote request timeout.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
/// Default request retry count.
const RETRY_COUNT: usize = 1;

/// On-demand service API.
pub trait OnDemandService<C: Chain>: Send + Sync {
	/// When new node is connected.
	fn on_connect(&self, peer: NodeIndex, role: service::Roles);

	/// When node is disconnected.
	fn on_disconnect(&self, peer: NodeIndex);

	/// Maintain peers requests.
	fn maintain_peers(&self, io: &mut SyncIo);

	/// When header response is received from remote node.
	fn on_remote_header_response(
		&self,
		io: &mut SyncIo,
		peer: NodeIndex,
		response: message::RemoteHeaderResponse<<C::Block as BlockT>::Header>
	);

	/// When read response is received from remote node.
	fn on_remote_read_response(&self, io: &mut SyncIo, peer: NodeIndex, response: message::RemoteReadResponse);

	/// When call response is received from remote node.
	fn on_remote_call_response(&self, io: &mut SyncIo, peer: NodeIndex, response: message::RemoteCallResponse);
}

/// On-demand requests service. Dispatches requests to appropriate peers.
pub struct OnDemand<C: Chain, E: service::ExecuteInContext<C>> {
	core: Mutex<OnDemandCore<C, E>>,
	checker: Arc<FetchChecker<C::Block>>,
}

/// On-demand remote call response.
pub struct RemoteResponse<T> {
	receiver: Receiver<Result<T, client::error::Error>>,
}

#[derive(Default)]
struct OnDemandCore<C: Chain, E: service::ExecuteInContext<C>> {
	service: Weak<E>,
	next_request_id: u64,
	pending_requests: VecDeque<Request<C::Block>>,
	active_peers: LinkedHashMap<NodeIndex, Request<C::Block>>,
	idle_peers: VecDeque<NodeIndex>,
}

struct Request<Block: BlockT> {
	id: u64,
	timestamp: Instant,
	retry_count: usize,
	data: RequestData<Block>,
}

enum RequestData<Block: BlockT> {
	RemoteHeader(RemoteHeaderRequest<Block::Header>, Sender<Result<Block::Header, client::error::Error>>),
	RemoteRead(RemoteReadRequest<Block::Header>, Sender<Result<Option<Vec<u8>>, client::error::Error>>),
	RemoteCall(RemoteCallRequest<Block::Header>, Sender<Result<client::CallResult, client::error::Error>>),
}

enum Accept<Block: BlockT> {
	Ok,
	CheckFailed(client::error::Error, RequestData<Block>),
	Unexpected(RequestData<Block>),
}

impl<T> Future for RemoteResponse<T> {
	type Item = T;
	type Error = client::error::Error;

	fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
		self.receiver.poll()
			.map_err(|_| client::error::ErrorKind::RemoteFetchCancelled.into())
			.and_then(|r| match r {
				Async::Ready(Ok(ready)) => Ok(Async::Ready(ready)),
				Async::Ready(Err(error)) => Err(error),
				Async::NotReady => Ok(Async::NotReady),
			})
	}
}

impl<C: Chain, E, C> OnDemand<C, E> where
	E: service::ExecuteInContext<C>,
	<C::Block as BlockT>::Header: HeaderT,
{
	/// Creates new on-demand service.
	pub fn new(checker: Arc<FetchChecker<C::Block>>) -> Self {
		OnDemand {
			checker,
			core: Mutex::new(OnDemandCore {
				service: Weak::new(),
				next_request_id: 0,
				pending_requests: VecDeque::new(),
				active_peers: LinkedHashMap::new(),
				idle_peers: VecDeque::new(),
			})
		}
	}

	/// Sets weak reference to network service.
	pub fn set_service_link(&self, service: Weak<E>) {
		self.core.lock().service = service;
	}

	/// Schedule && dispatch all scheduled requests.
	fn schedule_request<R>(
		&self,
		retry_count: Option<usize>,
		data: RequestData<C::Block>,
		result: R
	) -> R {
		let mut core = self.core.lock();
		core.insert(retry_count.unwrap_or(RETRY_COUNT), data);
		core.dispatch();
		result
	}

	/// Try to accept response from given peer.
	fn accept_response<F: FnOnce(Request<C::Block>) -> Accept<B>>(
		&self,
		rtype: &str,
		io: &mut SyncIo,
		peer: NodeIndex,
		request_id: u64,
		try_accept: F
	) {
		let mut core = self.core.lock();
		let request = match core.remove(peer, request_id) {
			Some(request) => request,
			None => {
				io.report_peer(peer, Severity::Bad(&format!("Invalid remote {} response from peer", rtype)));
				core.remove_peer(peer);
				return;
			},
		};

		let retry_count = request.retry_count;
		let (retry_count, retry_request_data) = match try_accept(request) {
			Accept::Ok => (retry_count, None),
			Accept::CheckFailed(error, retry_request_data) => {
				io.report_peer(peer, Severity::Bad(&format!("Failed to check remote {} response from peer: {}", rtype, error)));
				core.remove_peer(peer);

				if retry_count > 0 {
					(retry_count - 1, Some(retry_request_data))
				} else {
					trace!(target: "sync", "Failed to get remote {} response for given number of retries", rtype);
					retry_request_data.fail(client::error::ErrorKind::RemoteFetchFailed.into());
					(0, None)
				}
			},
			Accept::Unexpected(retry_request_data) => {
				io.report_peer(peer, Severity::Bad(&format!("Unexpected response to remote {} from peer", rtype)));
				core.remove_peer(peer);

				(retry_count, Some(retry_request_data))
			},
		};

		if let Some(request_data) = retry_request_data {
			core.insert(retry_count, request_data);
		}

		core.dispatch();
	}
}

impl<C, E> OnDemandService<C> for OnDemand<C, E> where
	C: Chain,
	E: service::ExecuteInContext<C>,
	<C::Block as BlockT>::Header: HeaderT,
{
	fn on_connect(&self, peer: NodeIndex, role: service::Roles) {
		if !role.intersects(service::Roles::FULL | service::Roles::AUTHORITY) { // TODO: correct?
			return;
		}

		let mut core = self.core.lock();
		core.add_peer(peer);
		core.dispatch();
	}

	fn on_disconnect(&self, peer: NodeIndex) {
		let mut core = self.core.lock();
		core.remove_peer(peer);
		core.dispatch();
	}

	fn maintain_peers(&self, io: &mut SyncIo) {
		let mut core = self.core.lock();
		for bad_peer in core.maintain_peers() {
			io.report_peer(bad_peer, Severity::Timeout);
		}
		core.dispatch();
	}

	fn on_remote_header_response(
		&self,
		io: &mut SyncIo,
		peer: NodeIndex,
		response: message::RemoteHeaderResponse<<C::Block as BlockT>::Header>
	) {
		self.accept_response("header", io, peer, response.id, |request| match request.data {
			RequestData::RemoteHeader(request, sender) => match self.checker.check_header_proof(&request, response.header, response.proof) {
				Ok(response) => {
					// we do not bother if receiver has been dropped already
					let _ = sender.send(Ok(response));
					Accept::Ok
				},
				Err(error) => Accept::CheckFailed(error, RequestData::RemoteHeader(request, sender)),
			},
			data @ _ => Accept::Unexpected(data),
		})
	}

	fn on_remote_read_response(
		&self,
		io: &mut SyncIo,
		peer: NodeIndex,
		response: message::RemoteReadResponse
	) {
		self.accept_response("read", io, peer, response.id, |request| match request.data {
			RequestData::RemoteRead(request, sender) => match self.checker.check_read_proof(&request, response.proof) {
				Ok(response) => {
					// we do not bother if receiver has been dropped already
					let _ = sender.send(Ok(response));
					Accept::Ok
				},
				Err(error) => Accept::CheckFailed(error, RequestData::RemoteRead(request, sender)),
			},
			data @ _ => Accept::Unexpected(data),
		})
	}

	fn on_remote_call_response(
		&self,
		io: &mut SyncIo,
		peer: NodeIndex,
		response: message::RemoteCallResponse
	) {
		self.accept_response("call", io, peer, response.id, |request| match request.data {
			RequestData::RemoteCall(request, sender) => match self.checker.check_execution_proof(&request, response.proof) {
				Ok(response) => {
					// we do not bother if receiver has been dropped already
					let _ = sender.send(Ok(response));
					Accept::Ok
				},
				Err(error) => Accept::CheckFailed(error, RequestData::RemoteCall(request, sender)),
			},
			data @ _ => Accept::Unexpected(data),
		})
	}
}

impl<C, E> Fetcher<C::Block> for OnDemand<C, E,> where
	C: Chain,
	E: service::ExecuteInContext<C>,
	<C::Block as BlockT>::Header: HeaderT,
{
	type RemoteHeaderResult = RemoteResponse<<C::Block as BlockT>::Header>;
	type RemoteReadResult = RemoteResponse<Option<Vec<u8>>>;
	type RemoteCallResult = RemoteResponse<client::CallResult>;

	fn remote_header(&self, request: RemoteHeaderRequest<<C::Block as BlockT>::Header>) -> Self::RemoteHeaderResult {
		let (sender, receiver) = channel();
		self.schedule_request(request.retry_count.clone(), RequestData::RemoteHeader(request, sender),
			RemoteResponse { receiver })
	}

	fn remote_read(&self, request: RemoteReadRequest<<C::Block as BlockT>::Header>) -> Self::RemoteReadResult {
		let (sender, receiver) = channel();
		self.schedule_request(request.retry_count.clone(), RequestData::RemoteRead(request, sender),
			RemoteResponse { receiver })
	}

	fn remote_call(&self, request: RemoteCallRequest<<C::Block as BlockT>::Header>) -> Self::RemoteCallResult {
		let (sender, receiver) = channel();
		self.schedule_request(request.retry_count.clone(), RequestData::RemoteCall(request, sender),
			RemoteResponse { receiver })
	}
}

impl<C, E> OnDemandCore<C, E> where
	C: Chain,
	E: service::ExecuteInContext<C>,
	<C::Block as BlockT>::Header: HeaderT,
{
	pub fn add_peer(&mut self, peer: NodeIndex) {
		self.idle_peers.push_back(peer);
	}

	pub fn remove_peer(&mut self, peer: NodeIndex) {
		if let Some(request) = self.active_peers.remove(&peer) {
			self.pending_requests.push_front(request);
			return;
		}

		if let Some(idle_index) = self.idle_peers.iter().position(|i| *i == peer) {
			self.idle_peers.swap_remove_back(idle_index);
		}
	}

	pub fn maintain_peers(&mut self) -> Vec<NodeIndex> {
		let now = Instant::now();
		let mut bad_peers = Vec::new();
		loop {
			match self.active_peers.front() {
				Some((_, request)) if now - request.timestamp >= REQUEST_TIMEOUT => (),
				_ => return bad_peers,
			}

			let (bad_peer, request) = self.active_peers.pop_front().expect("front() is Some as checked above");
			self.pending_requests.push_front(request);
			bad_peers.push(bad_peer);
		}
	}

	pub fn insert(&mut self, retry_count: usize, data: RequestData<C::Block>) {
		let request_id = self.next_request_id;
		self.next_request_id += 1;

		self.pending_requests.push_back(Request {
			id: request_id,
			timestamp: Instant::now(),
			retry_count,
			data,
		});
	}

	pub fn remove(&mut self, peer: NodeIndex, id: u64) -> Option<Request<C::Block>> {
		match self.active_peers.entry(peer) {
			Entry::Occupied(entry) => match entry.get().id == id {
				true => {
					self.idle_peers.push_back(peer);
					Some(entry.remove())
				},
				false => None,
			},
			Entry::Vacant(_) => None,
		}
	}

	pub fn dispatch(&mut self) {
		let service = match self.service.upgrade() {
			Some(service) => service,
			None => return,
		};

		while !self.pending_requests.is_empty() {
			let peer = match self.idle_peers.pop_front() {
				Some(peer) => peer,
				None => return,
			};

			let mut request = self.pending_requests.pop_front().expect("checked in loop condition; qed");
			request.timestamp = Instant::now();
			trace!(target: "sync", "Dispatching remote request {} to peer {}", request.id, peer);

			service.execute_in_context(|ctx| ctx.send_message(peer, request.message()));
			self.active_peers.insert(peer, request);
		}
	}
}

impl<C: Chain> Request<C> {
	pub fn message(&self) -> message::Message<C> {
		match self.data {
			RequestData::RemoteHeader(ref data, _) => message::generic::Message::RemoteHeaderRequest(
				message::RemoteHeaderRequest {
					id: self.id,
					block: data.block,
				}),
			RequestData::RemoteRead(ref data, _) => message::generic::Message::RemoteReadRequest(
				message::RemoteReadRequest {
					id: self.id,
					block: data.block,
					key: data.key.clone(),
				}),
			RequestData::RemoteCall(ref data, _) => message::generic::Message::RemoteCallRequest(
				message::RemoteCallRequest {
					id: self.id,
					block: data.block,
					method: data.method.clone(),
					data: data.call_data.clone(),
				}),
		}
	}
}

impl<Block: BlockT> RequestData<Block> {
	pub fn fail(self, error: client::error::Error) {
		// don't care if anyone is listening
		match self {
			RequestData::RemoteHeader(_, sender) => { let _ = sender.send(Err(error)); },
			RequestData::RemoteCall(_, sender) => { let _ = sender.send(Err(error)); },
			RequestData::RemoteRead(_, sender) => { let _ = sender.send(Err(error)); },
		}
	}
}

#[cfg(test)]
pub mod tests {
	use std::collections::VecDeque;
	use std::sync::Arc;
	use std::time::Instant;
	use futures::Future;
	use parking_lot::RwLock;
	use client;
	use client::light::fetcher::{Fetcher, FetchChecker, RemoteHeaderRequest,
		RemoteCallRequest, RemoteReadRequest};
	use message;
	use network_libp2p::NodeIndex;
	use service::{Roles, ExecuteInContext};
	use test::TestIo;
	use super::{REQUEST_TIMEOUT, OnDemand, OnDemandService};
	use test_client::runtime::{Block, Header};

	pub struct DummyExecutor;
	struct DummyFetchChecker { ok: bool }
	struct ConsensusMessage;

	impl ExecuteInContext<Block, ConsensusMessage> for DummyExecutor {
		fn execute_in_context<F: Fn(&mut ::protocol::Context<Block, ConsensusMessage>)>(&self, _closure: F) {}
	}

	impl FetchChecker<Block> for DummyFetchChecker {
		fn check_header_proof(
			&self,
			_request: &RemoteHeaderRequest<Header>,
			header: Option<Header>,
			_remote_proof: Vec<Vec<u8>>
		) -> client::error::Result<Header> {
			match self.ok {
				true if header.is_some() => Ok(header.unwrap()),
				_ => Err(client::error::ErrorKind::Backend("Test error".into()).into()),
			}
		}

		fn check_read_proof(&self, _request: &RemoteReadRequest<Header>, _remote_proof: Vec<Vec<u8>>) -> client::error::Result<Option<Vec<u8>>> {
			match self.ok {
				true => Ok(Some(vec![42])),
				false => Err(client::error::ErrorKind::Backend("Test error".into()).into()),
			}
		}

		fn check_execution_proof(&self, _request: &RemoteCallRequest<Header>, _remote_proof: Vec<Vec<u8>>) -> client::error::Result<client::CallResult> {
			match self.ok {
				true => Ok(client::CallResult {
					return_data: vec![42],
					changes: Default::default(),
				}),
				false => Err(client::error::ErrorKind::Backend("Test error".into()).into()),
			}
		}
	}

	fn dummy(ok: bool) -> (Arc<DummyExecutor>, Arc<OnDemand<Block, DummyExecutor, ConsensusMessage>>) {
		let executor = Arc::new(DummyExecutor);
		let service = Arc::new(OnDemand::new(Arc::new(DummyFetchChecker { ok })));
		service.set_service_link(Arc::downgrade(&executor));
		(executor, service)
	}

	fn total_peers(on_demand: &OnDemand<Block, DummyExecutor, ConsensusMessage>) -> usize {
		let core = on_demand.core.lock();
		core.idle_peers.len() + core.active_peers.len()
	}

	fn receive_call_response(on_demand: &OnDemand<Block, DummyExecutor, ConsensusMessage>, network: &mut TestIo, peer: NodeIndex, id: message::RequestId) {
		on_demand.on_remote_call_response(network, peer, message::RemoteCallResponse {
			id: id,
			proof: vec![vec![2]],
		});
	}

	fn dummy_header() -> Header {
		Header {
			parent_hash: Default::default(),
			number: 0,
			state_root: Default::default(),
			extrinsics_root: Default::default(),
			digest: Default::default(),
		}
	}

	#[test]
	fn knows_about_peers_roles() {
		let (_, on_demand) = dummy(true);
		on_demand.on_connect(0, Roles::LIGHT);
		on_demand.on_connect(1, Roles::FULL);
		on_demand.on_connect(2, Roles::AUTHORITY);
		assert_eq!(vec![1, 2], on_demand.core.lock().idle_peers.iter().cloned().collect::<Vec<_>>());
	}

	#[test]
	fn disconnects_from_idle_peer() {
		let (_, on_demand) = dummy(true);
		on_demand.on_connect(0, Roles::FULL);
		assert_eq!(1, total_peers(&*on_demand));
		on_demand.on_disconnect(0);
		assert_eq!(0, total_peers(&*on_demand));
	}

	#[test]
	fn disconnects_from_timeouted_peer() {
		let (_x, on_demand) = dummy(true);
		let queue = RwLock::new(VecDeque::new());
		let mut network = TestIo::new(&queue, None);

		on_demand.on_connect(0, Roles::FULL);
		on_demand.on_connect(1, Roles::FULL);
		assert_eq!(vec![0, 1], on_demand.core.lock().idle_peers.iter().cloned().collect::<Vec<_>>());
		assert!(on_demand.core.lock().active_peers.is_empty());

		on_demand.remote_call(RemoteCallRequest {
			block: Default::default(),
			header: dummy_header(),
			method: "test".into(),
			call_data: vec![],
			retry_count: None,
		});
		assert_eq!(vec![1], on_demand.core.lock().idle_peers.iter().cloned().collect::<Vec<_>>());
		assert_eq!(vec![0], on_demand.core.lock().active_peers.keys().cloned().collect::<Vec<_>>());

		on_demand.core.lock().active_peers[&0].timestamp = Instant::now() - REQUEST_TIMEOUT - REQUEST_TIMEOUT;
		on_demand.maintain_peers(&mut network);
		assert!(on_demand.core.lock().idle_peers.is_empty());
		assert_eq!(vec![1], on_demand.core.lock().active_peers.keys().cloned().collect::<Vec<_>>());
		assert!(network.to_disconnect.contains(&0));
	}

	#[test]
	fn disconnects_from_peer_on_response_with_wrong_id() {
		let (_x, on_demand) = dummy(true);
		let queue = RwLock::new(VecDeque::new());
		let mut network = TestIo::new(&queue, None);
		on_demand.on_connect(0, Roles::FULL);

		on_demand.remote_call(RemoteCallRequest {
			block: Default::default(),
			header: dummy_header(),
			method: "test".into(),
			call_data: vec![],
			retry_count: None,
		});
		receive_call_response(&*on_demand, &mut network, 0, 1);
		assert!(network.to_disconnect.contains(&0));
		assert_eq!(on_demand.core.lock().pending_requests.len(), 1);
	}

	#[test]
	fn disconnects_from_peer_on_incorrect_response() {
		let (_x, on_demand) = dummy(false);
		let queue = RwLock::new(VecDeque::new());
		let mut network = TestIo::new(&queue, None);
		on_demand.remote_call(RemoteCallRequest {
			block: Default::default(),
			header: dummy_header(),
			method: "test".into(),
			call_data: vec![],
			retry_count: Some(1),
		});

		on_demand.on_connect(0, Roles::FULL);
		receive_call_response(&*on_demand, &mut network, 0, 0);
		assert!(network.to_disconnect.contains(&0));
		assert_eq!(on_demand.core.lock().pending_requests.len(), 1);
	}

	#[test]
	fn disconnects_from_peer_on_unexpected_response() {
		let (_x, on_demand) = dummy(true);
		let queue = RwLock::new(VecDeque::new());
		let mut network = TestIo::new(&queue, None);
		on_demand.on_connect(0, Roles::FULL);

		receive_call_response(&*on_demand, &mut network, 0, 0);
		assert!(network.to_disconnect.contains(&0));
	}

	#[test]
	fn disconnects_from_peer_on_wrong_response_type() {
		let (_x, on_demand) = dummy(false);
		let queue = RwLock::new(VecDeque::new());
		let mut network = TestIo::new(&queue, None);
		on_demand.on_connect(0, Roles::FULL);

		on_demand.remote_call(RemoteCallRequest {
			block: Default::default(),
			header: dummy_header(),
			method: "test".into(),
			call_data: vec![],
			retry_count: Some(1),
		});

		on_demand.on_remote_read_response(&mut network, 0, message::RemoteReadResponse {
			id: 0,
			proof: vec![vec![2]],
		});
		assert!(network.to_disconnect.contains(&0));
		assert_eq!(on_demand.core.lock().pending_requests.len(), 1);
	}

	#[test]
	fn receives_remote_failure_after_retry_count_failures() {
		use parking_lot::{Condvar, Mutex};

		let retry_count = 2;
		let (_x, on_demand) = dummy(false);
		let queue = RwLock::new(VecDeque::new());
		let mut network = TestIo::new(&queue, None);
		for i in 0..retry_count+1 {
			on_demand.on_connect(i, Roles::FULL);
		}

		let sync = Arc::new((Mutex::new(0), Mutex::new(0), Condvar::new()));
		let thread_sync = sync.clone();

		let response = on_demand.remote_call(RemoteCallRequest {
			block: Default::default(),
			header: dummy_header(),
			method: "test".into(),
			call_data: vec![],
			retry_count: Some(retry_count)
		});
		let thread = ::std::thread::spawn(move || {
			let &(ref current, ref finished_at, ref finished) = &*thread_sync;
			let _ = response.wait().unwrap_err();
			*finished_at.lock() = *current.lock();
			finished.notify_one();
		});

		let &(ref current, ref finished_at, ref finished) = &*sync;
		for i in 0..retry_count+1 {
			let mut current = current.lock();
			*current = *current + 1;
			receive_call_response(&*on_demand, &mut network, i, i as u64);
		}

		let mut finished_at = finished_at.lock();
		assert!(!finished.wait_for(&mut finished_at, ::std::time::Duration::from_millis(1000)).timed_out());
		assert_eq!(*finished_at, retry_count + 1);

		thread.join().unwrap();
	}

	#[test]
	fn receives_remote_call_response() {
		let (_x, on_demand) = dummy(true);
		let queue = RwLock::new(VecDeque::new());
		let mut network = TestIo::new(&queue, None);
		on_demand.on_connect(0, Roles::FULL);

		let response = on_demand.remote_call(RemoteCallRequest {
			block: Default::default(),
			header: dummy_header(),
			method: "test".into(),
			call_data: vec![],
			retry_count: None,
		});
		let thread = ::std::thread::spawn(move || {
			let result = response.wait().unwrap();
			assert_eq!(result.return_data, vec![42]);
		});

		receive_call_response(&*on_demand, &mut network, 0, 0);
		thread.join().unwrap();
	}

	#[test]
	fn receives_remote_read_response() {
		let (_x, on_demand) = dummy(true);
		let queue = RwLock::new(VecDeque::new());
		let mut network = TestIo::new(&queue, None);
		on_demand.on_connect(0, Roles::FULL);

		let response = on_demand.remote_read(RemoteReadRequest {
			header: dummy_header(),
			block: Default::default(),
			key: b":key".to_vec(),
			retry_count: None,
		});
		let thread = ::std::thread::spawn(move || {
			let result = response.wait().unwrap();
			assert_eq!(result, Some(vec![42]));
		});

		on_demand.on_remote_read_response(&mut network, 0, message::RemoteReadResponse {
			id: 0,
			proof: vec![vec![2]],
		});
		thread.join().unwrap();
	}

	#[test]
	fn receives_remote_header_response() {
		let (_x, on_demand) = dummy(true);
		let queue = RwLock::new(VecDeque::new());
		let mut network = TestIo::new(&queue, None);
		on_demand.on_connect(0, Roles::FULL);

		let response = on_demand.remote_header(RemoteHeaderRequest {
			cht_root: Default::default(),
			block: 1,
			retry_count: None,
		});
		let thread = ::std::thread::spawn(move || {
			let result = response.wait().unwrap();
			assert_eq!(result.hash(), "80729accb7bb10ff9c637a10e8bb59f21c52571aa7b46544c5885ca89ed190f4".into());
		});

		on_demand.on_remote_header_response(&mut network, 0, message::RemoteHeaderResponse {
			id: 0,
			header: Some(Header {
				parent_hash: Default::default(),
				number: 1,
				state_root: Default::default(),
				extrinsics_root: Default::default(),
				digest: Default::default(),
			}),
			proof: vec![vec![2]],
		});
		thread.join().unwrap();
	}
}