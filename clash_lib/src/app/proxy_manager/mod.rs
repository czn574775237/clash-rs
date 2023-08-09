use std::{
    collections::{HashMap, VecDeque},
    sync::Arc,
    time::{Duration, Instant, SystemTime},
};

use boring::ssl::{SslConnector, SslMethod};

use http::Request;
use hyper_boring::HttpsConnector;
use tokio::sync::Mutex;
use tracing::error;

use crate::{
    common::errors::{map_io_error, new_io_error},
    proxy::AnyOutboundHandler,
};

use self::http_client::LocalConnector;

use super::ThreadSafeDNSResolver;

pub mod healthcheck;
mod http_client;
pub mod providers;

#[derive(Clone)]
pub struct DelayHistory {
    time: SystemTime,
    delay: u16,
    mean_delay: u16,
}

#[derive(Default)]
struct ProxyState {
    alive: bool,
    delay_history: VecDeque<DelayHistory>,
}

/// ProxyManager is only the latency registry.
/// TODO: move all proxies here, too, maybe.
#[derive(Clone)]
pub struct ProxyManager {
    proxy_state: Arc<Mutex<HashMap<String, ProxyState>>>,
    dns_resolver: ThreadSafeDNSResolver,
}

pub type ThreadSafeProxyManager = std::sync::Arc<tokio::sync::Mutex<ProxyManager>>;

impl ProxyManager {
    pub fn new(dns_resolver: ThreadSafeDNSResolver) -> Self {
        Self {
            dns_resolver,
            proxy_state: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub async fn check(
        &mut self,
        proxies: &Vec<AnyOutboundHandler>,
        url: &str,
        timeout: Option<Duration>,
    ) {
        let mut futures = vec![];
        for proxy in proxies {
            let proxy = proxy.clone();
            let url = url.to_owned();
            let timeout = timeout.clone();
            let mut manager = self.clone();
            futures.push(async move {
                manager
                    .url_test(proxy, url.as_str(), timeout)
                    .await
                    .map_err(|e| error!("healthcheck failed: {}", e))
            });
        }
        futures::future::join_all(futures).await;
    }

    pub async fn alive(&self, name: &str) -> bool {
        self.proxy_state
            .lock()
            .await
            .get(name)
            .map(|x| x.alive)
            .unwrap_or(false)
    }

    pub async fn report_alive(&mut self, name: &str, alive: bool) {
        let mut state = self.proxy_state.lock().await;
        let mut state = state.entry(name.to_owned()).or_default();
        state.alive = alive;
    }

    pub async fn delay_history(&self, name: &str) -> Vec<DelayHistory> {
        self.proxy_state
            .lock()
            .await
            .get(name)
            .map(|x| x.delay_history.clone())
            .unwrap_or_default()
            .into()
    }
    pub async fn last_delay(&self, name: &str) -> u16 {
        let max = u16::MAX;
        if !self.alive(name).await {
            return max;
        }
        self.delay_history(name)
            .await
            .last()
            .map(|x| x.delay)
            .unwrap_or(max)
    }
    pub async fn url_test(
        &mut self,
        proxy: AnyOutboundHandler,
        url: &str,
        timeout: Option<Duration>,
    ) -> std::io::Result<(u16, u16)> {
        let name = proxy.name().to_owned();
        let default_timeout = Duration::from_secs(30);

        let dns_resolver = self.dns_resolver.clone();
        let tester = async move {
            let connector = LocalConnector(proxy.clone(), dns_resolver);

            let mut ssl = SslConnector::builder(SslMethod::tls()).map_err(map_io_error)?;
            ssl.set_alpn_protos(b"\x02h2\x08http/1.1")
                .map_err(map_io_error)?;

            let connector = HttpsConnector::with_connector(connector, ssl).map_err(map_io_error)?;
            let client = hyper::Client::builder().build::<_, hyper::Body>(connector);

            let now = Instant::now();
            let req = Request::get(url).body(hyper::Body::empty()).unwrap();
            let resp = client.request(req);

            let delay: u16 =
                match tokio::time::timeout(timeout.unwrap_or(default_timeout), resp).await {
                    Ok(_) => Ok(now
                        .elapsed()
                        .as_millis()
                        .try_into()
                        .expect("delay is too large")),
                    Err(_) => Err(new_io_error(format!("timeout for {}", url).as_str())),
                }?;

            let req2 = Request::get(url).body(hyper::Body::empty()).unwrap();
            let resp2 = client.request(req2);
            let mean_delay: u16 =
                match tokio::time::timeout(timeout.unwrap_or(default_timeout), resp2).await {
                    Ok(_) => now
                        .elapsed()
                        .as_millis()
                        .try_into()
                        .expect("delay is too large"),
                    Err(_) => 0,
                };

            Ok((delay, mean_delay))
        };

        let result = tester.await;
        self.report_alive(&name, result.is_ok()).await;
        let ins = DelayHistory {
            time: SystemTime::now(),
            delay: result.as_ref().map(|x| x.0).unwrap_or(0),
            mean_delay: result.as_ref().map(|x| x.1).unwrap_or(0),
        };
        let mut state = self.proxy_state.lock().await;
        let state = state.entry(name.to_owned()).or_default();

        state.delay_history.push_back(ins);
        if state.delay_history.len() > 10 {
            state.delay_history.pop_front();
        }

        result
    }
}

#[cfg(test)]
mod tests {
    use std::{net::Ipv4Addr, sync::Arc, time::Duration};

    use futures::TryFutureExt;

    use crate::{
        app::dns::resolver::MockClashResolver, config::internal::proxy::PROXY_DIRECT,
        proxy::MockOutboundHandler,
    };

    #[tokio::test]
    async fn test_proxy_manager_alive() {
        let mut mock_resolver = MockClashResolver::new();
        mock_resolver
            .expect_resolve()
            .returning(|_| Ok(Some(std::net::IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)))));

        let mut manager = super::ProxyManager::new(Arc::new(mock_resolver));

        let mut mock_handler = MockOutboundHandler::new();
        mock_handler
            .expect_name()
            .return_const(PROXY_DIRECT.to_owned());
        mock_handler.expect_connect_stream().returning(|_, _| {
            Ok(Box::new(
                tokio_test::io::Builder::new()
                    .wait(Duration::from_millis(50))
                    .build(),
            ))
        });

        let mock_handler = Arc::new(mock_handler);

        manager
            .url_test(
                mock_handler.clone(),
                "http://www.google.com/generate_204",
                None,
            )
            .await
            .expect("test failed");

        assert!(manager.alive(PROXY_DIRECT).await);
        assert!(manager.last_delay(PROXY_DIRECT).await > 0);
        assert!(manager.delay_history(PROXY_DIRECT).await.len() > 0);

        manager.report_alive(PROXY_DIRECT, false).await;
        assert!(!manager.alive(PROXY_DIRECT).await);

        for _ in 0..10 {
            manager
                .url_test(
                    mock_handler.clone(),
                    "http://www.google.com/generate_204",
                    None,
                )
                .await
                .expect("test failed");
        }

        assert!(manager.alive(PROXY_DIRECT).await);
        assert!(manager.last_delay(PROXY_DIRECT).await > 0);
        assert!(manager.delay_history(PROXY_DIRECT).await.len() == 10);
    }

    #[tokio::test]
    async fn test_proxy_manager_timeout() {
        let mut mock_resolver = MockClashResolver::new();
        mock_resolver
            .expect_resolve()
            .returning(|_| Ok(Some(std::net::IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)))));

        let mut manager = super::ProxyManager::new(Arc::new(mock_resolver));

        let mut mock_handler = MockOutboundHandler::new();
        mock_handler
            .expect_name()
            .return_const(PROXY_DIRECT.to_owned());
        mock_handler.expect_connect_stream().returning(|_, _| {
            Ok(Box::new(
                tokio_test::io::Builder::new()
                    .wait(Duration::from_secs(10))
                    .build(),
            ))
        });

        let mock_handler = Arc::new(mock_handler);

        let result = manager
            .url_test(
                mock_handler.clone(),
                "http://www.google.com/generate_204",
                Some(Duration::from_secs(3)),
            )
            .map_err(|x| assert!(x.to_string().contains("timeout")))
            .await;

        assert!(result.is_err());
        assert!(!manager.alive(PROXY_DIRECT).await);
        assert!(manager.last_delay(PROXY_DIRECT).await == u16::MAX);
        assert!(manager.delay_history(PROXY_DIRECT).await.len() == 1);
    }
}