use atomic_option::AtomicOption;
use futures::sync::oneshot;
use futures::{future, stream, Future, Stream};
use futures_timer::Delay;
use protocol::{RedisClient, RedisClientError, RedisClientFactory, Resp};
use std::iter;
use std::str;
use std::sync::atomic;
use std::sync::Arc;
use std::time::Duration;

pub fn keep_retrying_and_sending<F: RedisClientFactory>(
    client_factory: Arc<F>,
    address: String,
    cmd: Vec<String>,
    interval: Duration,
) -> impl Future<Item = (), Error = RedisClientError> {
    keep_connecting_and_sending(client_factory, address, cmd, interval, retry_handle_func)
}

pub fn keep_connecting_and_sending<F: RedisClientFactory, Func>(
    client_factory: Arc<F>,
    address: String,
    cmd: Vec<String>,
    interval: Duration,
    handle_result: Func,
) -> impl Future<Item = (), Error = RedisClientError>
where
    Func: Clone + Fn(Resp) -> Result<(), RedisClientError>,
{
    let infinite_stream = stream::iter_ok(iter::repeat(()));
    infinite_stream.for_each(move |()| {
        let client_fut = client_factory.create_client(address.clone());
        let cmd_clone = cmd.clone();
        let cmd_clone2 = cmd.clone();
        let interval_clone = interval;
        let handle_result_clone = handle_result.clone();
        client_fut
            .and_then(move |client| {
                keep_sending_cmd(client, cmd_clone, interval_clone, handle_result_clone)
            })
            .then(move |result| match result {
                Ok(()) => future::ok(()),
                Err(RedisClientError::Done) => {
                    info!("stop keep sending commands {:?}", cmd_clone2);
                    future::err(RedisClientError::Done)
                }
                Err(e) => {
                    error!(
                        "failed to send commands {:?} {:?}. Try again.",
                        e, cmd_clone2
                    );
                    future::ok(())
                }
            })
    })
}

pub fn keep_sending_cmd<C: RedisClient, Func>(
    client: C,
    cmd: Vec<String>,
    interval: Duration,
    handle_result: Func,
) -> impl Future<Item = (), Error = RedisClientError>
where
    Func: Clone + Fn(Resp) -> Result<(), RedisClientError>,
{
    let infinite_stream = stream::iter_ok(iter::repeat(()));
    infinite_stream
        .fold(client, move |client, ()| {
            let byte_cmd = cmd.iter().map(|s| s.clone().into_bytes()).collect();
            // debug!("sending cmd {:?}", cmd);
            let handle_result_clone = handle_result.clone();
            let exec_fut = client
                .execute(byte_cmd)
                .map_err(|e| {
                    error!("failed to send: {}", e);
                    e
                })
                .and_then(move |(client, response)| {
                    future::result(handle_result_clone(response).map(|()| client))
                });
            let delay = Delay::new(interval).map_err(RedisClientError::Io);
            exec_fut.join(delay).map(move |(client, ())| client)
        })
        .map(|_| ())
}

pub fn retry_handle_func(response: Resp) -> Result<(), RedisClientError> {
    if let Resp::Error(err) = response {
        let err_str = str::from_utf8(&err)
            .map(ToString::to_string)
            .unwrap_or_else(|_| format!("{:?}", err));
        error!("error reply: {}", err_str);
    }
    Ok(())
}

pub struct I64Retriever {
    data: Arc<atomic::AtomicI64>,
    stop_signal: AtomicOption<oneshot::Sender<()>>,
}

impl I64Retriever {
    pub fn new<F: RedisClientFactory, Func>(
        init_data: i64,
        client_factory: Arc<F>,
        address: String,
        cmd: Vec<String>,
        interval: Duration,
        handle_func: Func,
    ) -> (Self, impl Future<Item = (), Error = RedisClientError>)
    where
        Func: Clone + Fn(Resp, &Arc<atomic::AtomicI64>) -> Result<(), RedisClientError>,
    {
        let (sender, receiver) = oneshot::channel();
        let data = Arc::new(atomic::AtomicI64::new(init_data));
        let data_clone = data.clone();

        let handle_result =
            move |resp: Resp| -> Result<(), RedisClientError> { handle_func(resp, &data_clone) };

        let sending =
            keep_connecting_and_sending(client_factory, address, cmd, interval, handle_result);
        let fut = receiver
            .map_err(|_| RedisClientError::Done)
            .select(sending)
            .map(|_| ())
            .map_err(|_| RedisClientError::Done);

        let stop_signal = AtomicOption::new(Box::new(sender));
        let retriever = Self { data, stop_signal };
        (retriever, fut)
    }

    pub fn get_data(&self) -> i64 {
        self.data.load(atomic::Ordering::SeqCst)
    }

    pub fn stop(&self) {
        if !self.try_stop() {
            warn!("Failed to stop I64Retriever. Maybe it has been already stopped.");
        }
    }

    pub fn try_stop(&self) -> bool {
        match self.stop_signal.take(atomic::Ordering::SeqCst) {
            Some(sender) => sender.send(()).is_ok(),
            None => false,
        }
    }
}

impl Drop for I64Retriever {
    fn drop(&mut self) {
        self.stop()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use ::common::utils::ThreadSafe;
    use ::protocol::BinSafeStr;
    use futures::future;

    #[derive(Debug)]
    struct Counter {
        pub count: AtomicUsize,
        pub max_count: usize,
    }

    impl Counter {
        fn new(max_count: usize) -> Self {
            Self {
                max_count,
                count: AtomicUsize::new(0),
            }
        }
    }

    #[derive(Debug)]
    struct DummyRedisClient {
        counter: Arc<Counter>,
    }

    impl DummyRedisClient {
        fn new(counter: Arc<Counter>) -> Self {
            Self { counter }
        }
    }

    impl ThreadSafe for DummyRedisClient {}

    impl RedisClient for DummyRedisClient {
        fn execute(
            self,
            _command: Vec<BinSafeStr>,
        ) -> Box<dyn Future<Item = (Self, Resp), Error = RedisClientError> + Send + 'static>
        {
            let client = self;
            if client.counter.count.load(Ordering::SeqCst) < client.counter.max_count {
                client.counter.count.fetch_add(1, Ordering::SeqCst);
                Box::new(future::ok((
                    client,
                    Resp::Simple("OK".to_string().into_bytes()),
                )))
            } else {
                Box::new(future::err(RedisClientError::Closed))
            }
        }
    }

    struct DummyClientFactory {
        counter: Arc<Counter>,
    }

    impl DummyClientFactory {
        fn new(counter: Arc<Counter>) -> Self {
            Self { counter }
        }
    }

    impl ThreadSafe for DummyClientFactory {}

    impl RedisClientFactory for DummyClientFactory {
        type Client = DummyRedisClient;

        fn create_client(
            &self,
            _address: String,
        ) -> Box<dyn Future<Item = Self::Client, Error = RedisClientError> + Send + 'static>
        {
            Box::new(future::ok(DummyRedisClient::new(self.counter.clone())))
        }
    }

    #[test]
    fn test_keep_sending_cmd() {
        let interval = Duration::new(0, 0);
        let counter = Arc::new(Counter::new(3));
        let res = keep_sending_cmd(
            DummyRedisClient::new(counter.clone()),
            vec![],
            interval,
            retry_handle_func,
        )
        .wait();
        assert!(res.is_err());
        assert_eq!(counter.count.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn test_keep_connecting_and_sending() {
        let interval = Duration::new(0, 0);
        let counter = Arc::new(Counter::new(3));
        let retry_counter = Arc::new(Counter::new(2));
        let retry_counter_clone = retry_counter.clone();
        let handler = move |_result| {
            if retry_counter.count.load(Ordering::SeqCst) < retry_counter.max_count {
                retry_counter.count.fetch_add(1, Ordering::SeqCst);
                Ok(())
            } else {
                Err(RedisClientError::Done)
            }
        };
        let factory = Arc::new(DummyClientFactory::new(counter.clone()));
        let res = keep_connecting_and_sending(
            factory,
            "host:port".to_string(),
            vec![],
            interval,
            handler,
        )
        .wait();
        assert!(res.is_err());
        assert_eq!(counter.count.load(Ordering::SeqCst), 3);
        assert_eq!(retry_counter_clone.count.load(Ordering::SeqCst), 2);
    }
}
