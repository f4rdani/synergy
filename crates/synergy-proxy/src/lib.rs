use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::RwLock;
use std::time::Duration;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProxyConfig {
    pub address: String,
    pub label: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProxyState {
    pub address: String,
    pub label: Option<String>,
    pub healthy: bool,
}

pub struct ProxyManager {
    proxies: Arc<RwLock<Vec<ProxyState>>>,
}

impl ProxyManager {
    pub fn new(configs: Vec<ProxyConfig>) -> Self {
        let states = configs
            .into_iter()
            .map(|c| ProxyState {
                address: c.address,
                label: c.label,
                healthy: true,
            })
            .collect();
        ProxyManager {
            proxies: Arc::new(RwLock::new(states)),
        }
    }

    pub async fn get_proxy_for_worker(&self, worker_id: usize) -> Option<String> {
        let list = self.proxies.read().await;
        if list.is_empty() {
            return None;
        }
        let healthy_proxies: Vec<&ProxyState> = list.iter().filter(|p| p.healthy).collect();
        if healthy_proxies.is_empty() {
            return None;
        }
        let idx = worker_id % healthy_proxies.len();
        Some(healthy_proxies[idx].address.clone())
    }

    pub async fn check_health(&self) {
        let mut list = self.proxies.write().await;
        for proxy in list.iter_mut() {
            let client_builder = reqwest::Client::builder().timeout(Duration::from_secs(5));
            let client = match reqwest::Proxy::all(&proxy.address) {
                Ok(reqwest_proxy) => client_builder.proxy(reqwest_proxy).build(),
                Err(_) => {
                    proxy.healthy = false;
                    continue;
                }
            };

            let client = match client {
                Ok(c) => c,
                Err(_) => {
                    proxy.healthy = false;
                    continue;
                }
            };

            let res = client.get("https://httpbin.org/ip").send().await;
            proxy.healthy = res.is_ok();
        }
    }

    pub async fn get_all(&self) -> Vec<ProxyState> {
        self.proxies.read().await.clone()
    }
}
