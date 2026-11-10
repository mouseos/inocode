//! Transport boundary — port of `goofish_client/transport.py`.
//! A Transport turns a RequestPlan into (HTTP status, decoded envelope).
//! Only the offline MockTransport lives here; the live HTTP path is the
//! async `execute_live` in the client (lib.rs).

use anyhow::{anyhow, Result};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Mutex;

use crate::requests::RequestPlan;

pub trait Transport: Send + Sync {
    fn execute(&self, plan: &RequestPlan) -> Result<(u16, Value)>;
}

/// Offline transport backed by registered synthetic envelopes.
/// `responses` maps (api_name, version) to a queue of envelope objects
/// consumed in order. Every executed plan is recorded.
pub struct MockTransport {
    queues: Mutex<HashMap<(String, String), Vec<Value>>>,
    pub requests: Mutex<Vec<RequestPlan>>,
}

impl MockTransport {
    pub fn new(responses: HashMap<(String, String), Vec<Value>>) -> Self {
        Self {
            queues: Mutex::new(responses),
            requests: Mutex::new(Vec::new()),
        }
    }

    pub fn request_count(&self) -> usize {
        self.requests.lock().unwrap().len()
    }
}

impl Transport for MockTransport {
    fn execute(&self, plan: &RequestPlan) -> Result<(u16, Value)> {
        self.requests.lock().unwrap().push(plan.clone());
        let key = (plan.api_name.clone(), plan.version.clone());
        let mut queues = self.queues.lock().unwrap();
        let queue = queues
            .get_mut(&key)
            .ok_or_else(|| anyhow!("MockTransport: no queued response for {}/{}", key.0, key.1))?;
        if queue.is_empty() {
            return Err(anyhow!(
                "MockTransport: no queued response for {}/{}",
                key.0,
                key.1
            ));
        }
        Ok((200, queue.remove(0)))
    }
}

/// A queued MockTransport response with an explicit HTTP status, for
/// classification / halt tests.
pub struct StatusMockTransport {
    responses: Mutex<Vec<(u16, Value)>>,
    pub requests: Mutex<Vec<RequestPlan>>,
}

impl StatusMockTransport {
    pub fn new(responses: Vec<(u16, Value)>) -> Self {
        Self {
            responses: Mutex::new(responses),
            requests: Mutex::new(Vec::new()),
        }
    }
}

impl Transport for StatusMockTransport {
    fn execute(&self, plan: &RequestPlan) -> Result<(u16, Value)> {
        self.requests.lock().unwrap().push(plan.clone());
        let mut q = self.responses.lock().unwrap();
        if q.is_empty() {
            return Err(anyhow!("StatusMockTransport: queue exhausted"));
        }
        Ok(q.remove(0))
    }
}
