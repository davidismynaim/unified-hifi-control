//! Skip retained MQTT publishes whose payload has not changed since we last sent it.
//!
//! `handle_bus_event` republishes a zone's full discovery config (every entity) plus its
//! state on each of several bus events per playback tick, so an unchanged discovery config
//! went out dozens of times a second and every position value went out four times. Retained
//! messages are idempotent, so resending identical bytes only costs broker, Home Assistant
//! and recorder work. Non-retained messages are never deduplicated: they are events.
//!
//! The cache must be cleared whenever the broker may have lost what we published (a fresh
//! connection, or Home Assistant announcing itself) - see [`MqttClient::reset_dedupe`].

use std::collections::HashMap;
use std::ops::Deref;
use std::sync::{Arc, Mutex};

use rumqttc::{AsyncClient, ClientError, QoS};

/// Pure decision logic, kept separate from the client so it can be unit tested.
#[derive(Debug, Default)]
pub struct PublishCache {
    last: HashMap<String, Vec<u8>>,
}

impl PublishCache {
    /// Whether `payload` should actually be sent to `topic`. Records it when it should.
    pub fn should_publish(&mut self, topic: &str, retain: bool, payload: &[u8]) -> bool {
        if !retain {
            return true;
        }
        if self.last.get(topic).is_some_and(|prev| prev == payload) {
            return false;
        }
        self.last.insert(topic.to_owned(), payload.to_vec());
        true
    }

    /// Drop the record for one topic (used when a send failed, so a retry is not suppressed).
    pub fn forget(&mut self, topic: &str) {
        self.last.remove(topic);
    }

    pub fn clear(&mut self) {
        self.last.clear();
    }
}

/// `AsyncClient` whose retained `publish` skips unchanged payloads. Everything else
/// (`subscribe`, ...) is the underlying client's, via `Deref`.
#[derive(Clone)]
pub struct MqttClient {
    inner: AsyncClient,
    cache: Arc<Mutex<PublishCache>>,
}

impl MqttClient {
    pub fn new(inner: AsyncClient) -> Self {
        Self {
            inner,
            cache: Arc::new(Mutex::new(PublishCache::default())),
        }
    }

    /// Forget everything published so far, so the next announce resends it all.
    pub fn reset_dedupe(&self) {
        if let Ok(mut cache) = self.cache.lock() {
            cache.clear();
        }
    }

    /// Same signature as [`AsyncClient::publish`]; shadows it so existing call sites are unchanged.
    pub async fn publish<S: Into<String>, V: Into<Vec<u8>>>(
        &self,
        topic: S,
        qos: QoS,
        retain: bool,
        payload: V,
    ) -> Result<(), ClientError> {
        let topic = topic.into();
        let payload = payload.into();
        // The guard is dropped before the await below.
        let send = self
            .cache
            .lock()
            .map(|mut cache| cache.should_publish(&topic, retain, &payload))
            .unwrap_or(true);
        if !send {
            return Ok(());
        }
        let result = self
            .inner
            .publish(topic.clone(), qos, retain, payload)
            .await;
        if result.is_err() {
            if let Ok(mut cache) = self.cache.lock() {
                cache.forget(&topic);
            }
        }
        result
    }
}

impl Deref for MqttClient {
    type Target = AsyncClient;
    fn deref(&self) -> &AsyncClient {
        &self.inner
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identical_retained_payload_is_sent_once() {
        let mut c = PublishCache::default();
        assert!(c.should_publish("t", true, b"a"));
        assert!(!c.should_publish("t", true, b"a"));
        assert!(!c.should_publish("t", true, b"a"));
    }

    #[test]
    fn a_changed_payload_is_sent_and_becomes_the_new_baseline() {
        let mut c = PublishCache::default();
        assert!(c.should_publish("t", true, b"a"));
        assert!(c.should_publish("t", true, b"b"));
        assert!(!c.should_publish("t", true, b"b"));
        assert!(
            c.should_publish("t", true, b"a"),
            "flipping back is a change"
        );
    }

    #[test]
    fn topics_are_independent() {
        let mut c = PublishCache::default();
        assert!(c.should_publish("t1", true, b"a"));
        assert!(c.should_publish("t2", true, b"a"));
    }

    #[test]
    fn non_retained_messages_are_never_deduplicated() {
        let mut c = PublishCache::default();
        assert!(c.should_publish("cmd", false, b"PRESS"));
        assert!(c.should_publish("cmd", false, b"PRESS"));
    }

    #[test]
    fn clearing_resends_everything_after_a_reconnect() {
        let mut c = PublishCache::default();
        assert!(c.should_publish("t", true, b"a"));
        c.clear();
        assert!(c.should_publish("t", true, b"a"));
    }

    #[test]
    fn a_failed_send_does_not_suppress_the_retry() {
        let mut c = PublishCache::default();
        assert!(c.should_publish("t", true, b"a"));
        c.forget("t");
        assert!(c.should_publish("t", true, b"a"));
    }

    #[test]
    fn retract_then_reannounce_is_sent_both_times() {
        // Retracting a zone publishes an empty retained payload; bringing it back must resend.
        let mut c = PublishCache::default();
        assert!(c.should_publish("cfg", true, b"{\"name\":\"x\"}"));
        assert!(c.should_publish("cfg", true, b""));
        assert!(c.should_publish("cfg", true, b"{\"name\":\"x\"}"));
    }
}
