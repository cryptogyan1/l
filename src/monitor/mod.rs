use crate::client::PolymarketClient;
use crate::domain::*;
use crate::execution::orderbook::fetch_orderbook;
use anyhow::Result;
use log::{info, warn};
use rust_decimal::Decimal;
use std::sync::Arc;
use tokio::time::{sleep, Duration};

pub struct MarketMonitor {
    api: Arc<PolymarketClient>,
    eth_market: Market,
    btc_market: Market,
    check_interval: Duration,
}

#[derive(Debug, Clone)]
pub struct MarketSnapshot {
    pub eth_market: MarketData,
    pub btc_market: MarketData,
    pub timestamp: std::time::Instant,
}

impl MarketMonitor {
    pub fn new(
        api: Arc<PolymarketClient>,
        eth_market: Market,
        btc_market: Market,
        check_interval_ms: u64,
    ) -> Self {
        Self {
            api,
            eth_market,
            btc_market,
            check_interval: Duration::from_millis(check_interval_ms),
        }
    }

    pub async fn start_monitoring<F, Fut>(&self, on_snapshot: F)
    where
        F: Fn(MarketSnapshot) -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = ()> + Send + 'static,
    {
        info!("🎬 Monitor starting...");

        loop {
            match self.fetch_snapshot().await {
                Ok(snapshot) => on_snapshot(snapshot).await,
                Err(e) => warn!("📊 Snapshot error: {}", e),
            }

            sleep(self.check_interval).await;
        }
    }

    async fn fetch_snapshot(&self) -> Result<MarketSnapshot> {
        Ok(MarketSnapshot {
            eth_market: self.build_market("ETH", &self.eth_market).await?,
            btc_market: self.build_market("BTC", &self.btc_market).await?,
            timestamp: std::time::Instant::now(),
        })
    }

    async fn build_market(&self, name: &str, market: &Market) -> Result<MarketData> {
        // ===============================
        // CRITICAL FIX: Use clob_token_ids instead of tokens field
        // The tokens field is often None for 15-minute markets
        // ===============================

        let token_ids_str = market
            .clob_token_ids
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("{} market missing clob_token_ids", name))?;

        let token_ids: Vec<String> = serde_json::from_str(token_ids_str)
            .map_err(|e| anyhow::anyhow!("Failed to parse {} token IDs: {}", name, e))?;

        if token_ids.len() < 2 {
            return Err(anyhow::anyhow!("{} market has less than 2 tokens", name));
        }

        // First token is typically UP (Yes/1), second is DOWN (No/0)
        let up_token_id = &token_ids[0];
        let down_token_id = &token_ids[1];

        // Fetch prices for UP token
        let (up_bid, up_ask) = match fetch_orderbook(&self.api, up_token_id).await {
            Ok(book) => {
                let best_bid = book
                    .best_bid()
                    .map(|(price, _size)| Decimal::from_f64_retain(price).unwrap_or(Decimal::ZERO));
                let best_ask = book
                    .best_ask()
                    .map(|(price, _size)| Decimal::from_f64_retain(price).unwrap_or(Decimal::ZERO));

                if let (Some(b), Some(a)) = (best_bid, best_ask) {
                    info!("📊 {} UP   | bid: {} | ask: {}", name, b, a);
                }

                (best_bid, best_ask)
            }
            Err(e) => {
                warn!("⚠️  Failed to fetch {} UP prices: {}", name, e);
                (None, None)
            }
        };

        // Fetch prices for DOWN token
        let (down_bid, down_ask) = match fetch_orderbook(&self.api, down_token_id).await {
            Ok(book) => {
                let best_bid = book
                    .best_bid()
                    .map(|(price, _size)| Decimal::from_f64_retain(price).unwrap_or(Decimal::ZERO));
                let best_ask = book
                    .best_ask()
                    .map(|(price, _size)| Decimal::from_f64_retain(price).unwrap_or(Decimal::ZERO));

                if let (Some(b), Some(a)) = (best_bid, best_ask) {
                    info!("📊 {} DOWN | bid: {} | ask: {}", name, b, a);
                }

                (best_bid, best_ask)
            }
            Err(e) => {
                warn!("⚠️  Failed to fetch {} DOWN prices: {}", name, e);
                (None, None)
            }
        };

        Ok(MarketData {
            condition_id: market.condition_id.clone(),
            market_name: name.to_string(),
            up_token: Some(TokenPrice {
                token_id: up_token_id.clone(),
                bid: up_bid,
                ask: up_ask,
            }),
            down_token: Some(TokenPrice {
                token_id: down_token_id.clone(),
                bid: down_bid,
                ask: down_ask,
            }),
        })
    }
}
