use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;

use crate::common::{
    AnnouncePeerResponse, Id, Node, ResponseDone, ResponseMessage, ResponseSender, ResponseValue,
};
use crate::messages::RequestSpecific;
use crate::routing_table::{RoutingTable, MAX_BUCKET_SIZE_K};
use crate::socket::KrpcSocket;

/// A query is an iterative process of concurrently sending a request to the closest known nodes to
/// the target, updating the routing table with closer nodes discovered in the responses, and
/// repeating this process until no closer nodes (that aren't already queried) are found.
#[derive(Debug)]
pub struct Query {
    target: Id,
    request: RequestSpecific,
    candidates: RoutingTable,
    with_token: RoutingTable,
    inflight_requests: Vec<u16>,
    visited: HashSet<SocketAddr>,
    senders: Vec<ResponseSender>,
    responses: Vec<ResponseValue>,
}

impl Query {
    pub fn new(target: Id, request: RequestSpecific) -> Self {
        let candidates = RoutingTable::new().with_id(target);
        let with_token = RoutingTable::new().with_id(target);

        Self {
            target,
            request,
            candidates,
            // TODO: cache closest in the RPC?
            with_token,
            inflight_requests: Vec::new(),
            visited: HashSet::new(),
            senders: Vec::new(),
            responses: Vec::new(),
        }
    }

    // === Getters ===
    /// No more inflight_requests and visited addresses were reset.
    pub fn is_done(&self) -> bool {
        self.inflight_requests.is_empty() && self.visited.is_empty()
    }

    // === Public Methods ===

    /// Add a sender to the query and send all replies we found so far to it.
    pub fn add_sender(&mut self, sender: Option<ResponseSender>) {
        if let Some(sender) = sender {
            self.senders.push(sender);
            let sender = self.senders.last().unwrap();

            for response in &self.responses {
                self.send_value(sender, response.clone())
            }
        };
    }

    /// Force start query traversal by visiting closest nodes.
    pub fn start(&mut self, socket: &mut KrpcSocket) {
        self.visit_closest(socket);
    }

    /// Add a node to the routing table.
    pub fn add_candidate(&mut self, node: Node) {
        // ready for a ipv6 routing table?
        self.candidates.add(node);
    }

    /// Visit explicitly given addresses, and add them to the visited set.
    pub fn visit(&mut self, socket: &mut KrpcSocket, address: SocketAddr) {
        if self.visited.contains(&address) || address.is_ipv6() {
            // TODO: Add support for IPV6.
            return;
        }

        let tid = socket.request(address, self.request.clone());
        self.inflight_requests.push(tid);
        self.visited.insert(address);
    }

    /// Remove an inflight_request and return true if it existed.
    pub fn remove_inflight_request(&mut self, tid: u16) -> bool {
        if let Some(index) = self.inflight_requests.iter().position(|&x| x == tid) {
            self.inflight_requests.remove(index);

            return true;
        };

        false
    }

    pub fn add_responding_node(&mut self, node: Node) {
        self.with_token.add(node.clone());
    }

    /// Add reveived response
    pub fn response(&mut self, response: ResponseValue) {
        self.responses.push(response.clone());

        for sender in &self.senders {
            self.send_value(sender, response.clone())
        }
    }

    /// Query closest nodes for this query's target and message.
    pub fn tick(&mut self, socket: &mut KrpcSocket) {
        if self.is_done() {
            return;
        }

        // If there are no more inflight requests, and visited is empty, then
        // last tick we didn't add any closer nodes, so we are done traversing.
        self.visit_closest(socket);

        // First we clear timedout requests.
        // If no requests remain, then visit_closest didn't add any closer nodes,
        //  so we remove all visited addresses to set the query to "done" again.
        // If any senders are still waiting for response, send None to end the iterator,
        //  then clear them too.
        self.after_tick(socket);
    }

    // === Private Methods ===

    fn send_value(&self, sender: &ResponseSender, value: ResponseValue) {
        match sender {
            ResponseSender::GetPeer(sender) => {
                let ResponseValue::GetPeer(peer) = value.clone();
                let _ = sender.send(ResponseMessage::ResponseValue(peer));
            }
            _ => {}
        };
    }

    fn send_done(&self, sender: &ResponseSender) {
        let done = ResponseDone {
            visited: self.visited.len(),
            // Basically, we were using the routing table as a temporary
            closest_nodes: self.with_token.closest(&self.target),
        };

        match sender {
            ResponseSender::GetPeer(sender) => {
                ///
                sender.send(ResponseMessage::ResponseDone(done));
            }
            _ => {}
        };
    }

    fn visit_closest(&mut self, socket: &mut KrpcSocket) {
        let mut to_visit = self.candidates.closest(&self.target);
        to_visit.retain(|node| !self.visited.contains(&node.address));

        for node in to_visit {
            self.visit(socket, node.address);
        }
    }

    fn after_tick(&mut self, socket: &mut KrpcSocket) {
        self.inflight_requests
            .retain(|&tid| socket.inflight_requests.contains_key(&tid));

        if self.inflight_requests.is_empty() {
            // Send Done to all receivers to end iterators
            for sender in &self.senders {
                self.send_done(sender);
            }

            // No more closer nodes to visit, and no inflight requests to wait for
            // reset the visited set.
            //
            // Effectively this sets the query to "done" again.
            // This query will then be deleted from the rpc.queries map in the next tick.
            self.visited.clear();
        }
    }
}

#[derive(Debug)]
pub struct StoreQuery {
    /// Nodes queried
    queried: Vec<Node>,
    /// Nodes that confirmed success
    success: Vec<Id>,
    inflight_requests: Vec<u16>,
    sender: ResponseSender,
}

impl StoreQuery {
    pub fn new(sender: ResponseSender) -> Self {
        Self {
            queried: Vec::new(),
            success: Vec::new(),
            inflight_requests: Vec::new(),
            sender,
        }
    }

    pub fn request(&mut self, node: Node, request: RequestSpecific, socket: &mut KrpcSocket) {
        let tid = socket.request(node.address, request);

        self.queried.push(node);
        self.inflight_requests.push(tid);
    }

    pub fn is_done(&self) -> bool {
        self.inflight_requests.is_empty()
    }

    pub fn remove_inflight_request(&mut self, tid: u16) -> bool {
        if let Some(index) = self.inflight_requests.iter().position(|&x| x == tid) {
            self.inflight_requests.remove(index);

            return true;
        };
        false
    }

    pub fn success(&mut self, id: Id) {
        self.success.push(id);
    }

    /// remove timed out requests
    pub fn tick(&mut self, socket: &mut KrpcSocket) {
        self.inflight_requests
            .retain(|&tid| socket.inflight_requests.contains_key(&tid));

        if self.is_done() {
            match &self.sender {
                ResponseSender::AnnouncePeer(sender) => {
                    let _ = sender.send(AnnouncePeerResponse {
                        closest_nodes: self.queried.clone(),
                        success: self.success.clone(),
                    });
                }
                _ => {}
            }
        }
    }
}
