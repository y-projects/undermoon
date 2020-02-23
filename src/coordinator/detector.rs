use super::broker::MetaDataBroker;
use super::core::{CoordinateError, FailureChecker, FailureReporter, ProxiesRetriever};
use crate::protocol::{RedisClient, RedisClientFactory};
use futures::{Future, Stream, TryFutureExt, TryStreamExt};
use std::pin::Pin;
use std::sync::Arc;

pub struct BrokerProxiesRetriever<B: MetaDataBroker> {
    meta_data_broker: Arc<B>,
}

impl<B: MetaDataBroker> BrokerProxiesRetriever<B> {
    pub fn new(meta_data_broker: Arc<B>) -> Self {
        Self { meta_data_broker }
    }
}

impl<B: MetaDataBroker> ProxiesRetriever for BrokerProxiesRetriever<B> {
    fn retrieve_proxies<'s>(
        &'s self,
    ) -> Pin<Box<dyn Stream<Item = Result<String, CoordinateError>> + Send + 's>> {
        Box::pin(
            self.meta_data_broker
                .get_host_addresses()
                .map_err(CoordinateError::MetaData),
        )
    }
}

pub struct PingFailureDetector<F: RedisClientFactory> {
    client_factory: Arc<F>,
}

impl<F: RedisClientFactory> PingFailureDetector<F> {
    pub fn new(client_factory: Arc<F>) -> Self {
        Self { client_factory }
    }

    async fn ping(&self, address: String) -> Result<Option<String>, CoordinateError> {
        let mut client = match self.client_factory.create_client(address.clone()).await {
            Ok(client) => client,
            Err(err) => {
                error!("PingFailureDetector::check failed to connect: {:?}", err);
                return Ok(Some(address));
            }
        };

        // The connection pool might get a stale connection.
        // Return err instead for retry.
        let ping_command = vec!["PING".to_string().into_bytes()];
        match client.execute_single(ping_command).await {
            Ok(_) => Ok(None),
            Err(err) => {
                error!("PingFailureDetector::check failed to send PING: {:?}", err);
                Err(CoordinateError::Redis(err))
            }
        }
    }

    async fn check_impl(&self, address: String) -> Result<Option<String>, CoordinateError> {
        const RETRY: usize = 3;
        for i in 1..=RETRY {
            match self.ping(address.clone()).await {
                Ok(None) => return Ok(None),
                _ if i == RETRY => return Ok(Some(address)),
                _ => continue,
            }
        }
        Ok(Some(address))
    }
}

impl<F: RedisClientFactory> FailureChecker for PingFailureDetector<F> {
    fn check<'s>(
        &'s self,
        address: String,
    ) -> Pin<Box<dyn Future<Output = Result<Option<String>, CoordinateError>> + Send + 's>> {
        Box::pin(self.check_impl(address))
    }
}

pub struct BrokerFailureReporter<B: MetaDataBroker> {
    reporter_id: String,
    meta_data_broker: Arc<B>,
}

impl<B: MetaDataBroker> BrokerFailureReporter<B> {
    pub fn new(reporter_id: String, meta_data_broker: Arc<B>) -> Self {
        Self {
            reporter_id,
            meta_data_broker,
        }
    }
}

impl<B: MetaDataBroker> FailureReporter for BrokerFailureReporter<B> {
    fn report<'s>(
        &'s self,
        address: String,
    ) -> Pin<Box<dyn Future<Output = Result<(), CoordinateError>> + Send + 's>> {
        Box::pin(
            self.meta_data_broker
                .add_failure(address, self.reporter_id.clone())
                .map_err(CoordinateError::MetaData),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::super::broker::{MetaDataBroker, MetaDataBrokerError};
    use super::super::core::{FailureDetector, SeqFailureDetector};
    use super::*;
    use crate::common::cluster::{Cluster, Proxy};
    use crate::protocol::{
        Array, BinSafeStr, OptionalMulti, RedisClient, RedisClientError, Resp, RespVec,
    };
    use futures::{future, stream};
    use std::pin::Pin;
    use std::sync::{Arc, Mutex};
    use tokio;

    const NODE1: &'static str = "127.0.0.1:7000";
    const NODE2: &'static str = "127.0.0.1:7001";

    #[derive(Debug)]
    struct DummyClient {
        address: String,
    }

    impl RedisClient for DummyClient {
        fn execute<'s>(
            &'s mut self,
            _command: OptionalMulti<Vec<BinSafeStr>>,
        ) -> Pin<
            Box<dyn Future<Output = Result<OptionalMulti<RespVec>, RedisClientError>> + Send + 's>,
        > {
            if self.address == NODE1 {
                // only works for single command
                Box::pin(future::ok(OptionalMulti::Single(
                    Resp::Arr(Array::Nil).into(),
                )))
            } else {
                Box::pin(future::err(RedisClientError::InvalidReply))
            }
        }
    }

    struct DummyClientFactory;

    impl RedisClientFactory for DummyClientFactory {
        type Client = DummyClient;

        fn create_client(
            &self,
            address: String,
        ) -> Pin<Box<dyn Future<Output = Result<Self::Client, RedisClientError>> + Send>> {
            Box::pin(future::ok(DummyClient { address }))
        }
    }

    #[derive(Clone)]
    struct DummyMetaBroker {
        reported_failures: Arc<Mutex<Vec<String>>>,
    }

    impl DummyMetaBroker {
        fn new() -> Self {
            Self {
                reported_failures: Arc::new(Mutex::new(vec![])),
            }
        }
    }

    impl MetaDataBroker for DummyMetaBroker {
        fn get_cluster_names<'s>(
            &'s self,
        ) -> Pin<Box<dyn Stream<Item = Result<String, MetaDataBrokerError>> + Send + 's>> {
            Box::pin(stream::iter(vec![]))
        }
        fn get_cluster<'s>(
            &'s self,
            _name: String,
        ) -> Pin<Box<dyn Future<Output = Result<Option<Cluster>, MetaDataBrokerError>> + Send + 's>>
        {
            Box::pin(future::ok(None))
        }
        fn get_host_addresses<'s>(
            &'s self,
        ) -> Pin<Box<dyn Stream<Item = Result<String, MetaDataBrokerError>> + Send + 's>> {
            Box::pin(stream::iter(vec![
                Ok(NODE1.to_string()),
                Ok(NODE2.to_string()),
            ]))
        }
        fn get_host<'s>(
            &'s self,
            _address: String,
        ) -> Pin<Box<dyn Future<Output = Result<Option<Proxy>, MetaDataBrokerError>> + Send + 's>>
        {
            Box::pin(future::ok(None))
        }
        fn add_failure<'s>(
            &'s self,
            address: String,
            _reporter_id: String,
        ) -> Pin<Box<dyn Future<Output = Result<(), MetaDataBrokerError>> + Send + 's>> {
            self.reported_failures
                .lock()
                .expect("dummy_add_failure")
                .push(address);
            Box::pin(future::ok(()))
        }
        fn get_failures<'s>(
            &'s self,
        ) -> Pin<Box<dyn Stream<Item = Result<String, MetaDataBrokerError>> + Send + 's>> {
            Box::pin(stream::iter(vec![]))
        }
    }

    #[tokio::test]
    async fn test_detector() {
        let broker = Arc::new(DummyMetaBroker::new());
        let retriever = BrokerProxiesRetriever::new(broker.clone());
        let checker = PingFailureDetector::new(Arc::new(DummyClientFactory {}));
        let reporter = BrokerFailureReporter::new("test_id".to_string(), broker.clone());
        let detector = SeqFailureDetector::new(retriever, checker, reporter);

        let res = detector.run().into_future().await;
        assert!(res.is_ok());
        let failed_nodes = broker
            .reported_failures
            .lock()
            .expect("test_detector")
            .clone();
        assert_eq!(1, failed_nodes.len());
        assert_eq!(NODE2, failed_nodes[0]);
    }
}
