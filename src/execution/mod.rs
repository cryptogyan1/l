pub mod clob_client;
use crate::client::PolymarketClient;
use crate::config::{PositionSizing, TradeMode, TradingConfig, WalletConfig};
use crate::domain::*;
use crate::wallet::signer::{ClobOrder, WalletSigner};
use anyhow::Result;
use std::str::FromStr;
use ::rand::Rng;
use ethers::types::Address;
pub use clob_client::ClobClient;
use ethers::types::{H256, U256};
use ethers::utils::keccak256;
use log::{info, warn};
use rust_decimal::prelude::{FromPrimitive, ToPrimitive};
use rust_decimal::Decimal;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::Mutex;
pub mod errors;
pub mod orderbook;
pub mod trader;

// ==================================================
// Helpers
// ==================================================

fn str_to_h256(s: &str) -> H256 {
    H256::from_slice(&keccak256(s.as_bytes()))
}

fn to_u256_scaled(v: Decimal) -> U256 {
    let f = v.to_f64().unwrap_or(0.0);
    U256::from((f * 1_000_000.0) as u128)
}

fn now_ts() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

fn make_nonce() -> U256 {
    let t = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    U256::from(t)
}

// ==================================================
// Trader
// ==================================================

pub struct Trader {
    api: Arc<PolymarketClient>,
    clob: Arc<ClobClient>,
    config: TradingConfig,
    wallet: WalletConfig,
    signer: WalletSigner,
    sizing: PositionSizing,

    live_usdc_balance: Arc<Mutex<Decimal>>,
}

impl Trader {
    pub fn new(
        api: Arc<PolymarketClient>,
        clob: Arc<ClobClient>,
        config: TradingConfig,
        wallet: WalletConfig,
        signer: WalletSigner,
    ) -> Self {
        Self {
            api,
            clob,
            config,
            wallet,
            signer,
            sizing: PositionSizing::from_env(),
            live_usdc_balance: Arc::new(Mutex::new(Decimal::ZERO)),
        }
    }

    // ==================================================
    // BALANCE
    // ==================================================

    async fn refresh_balance(&self) -> Result<()> {
        let bal = self.api.get_usdc_balance().await?;
        *self.live_usdc_balance.lock().await = bal;
        info!("💰 USDC balance: {}", bal);
        Ok(())
    }

    // ==================================================
    // EXECUTION (REAL MONEY)
    // ==================================================

    pub async fn execute_arbitrage(&self, opportunity: &ArbitrageOpportunity) -> Result<()> {
        // 1️⃣ Refresh balance
        self.refresh_balance().await?;

        // 2️⃣ Calculate size
        let units = self.calculate_position_size(opportunity).await?;
        if units <= 0.0 {
            return Ok(());
        }

        let cost = opportunity.total_cost.to_f64().unwrap_or(0.0);
        let spend = units * cost;

        if spend < 1.0 {
            warn!("❌ Trade skipped (below $1 minimum)");
            return Ok(());
        }

        // 3️⃣ HARD GATE — balance + allowance + ERC1155
        self.clob
            .ensure_trading_ready((spend * 1_000_000.0) as u128)
            .await?;

        info!(
            "🚀 EXEC | units={} spend=${:.2} expected_profit={}",
            units, spend, opportunity.expected_profit
        );

        let size_dec = Decimal::from_f64(units).unwrap();

        // ================= ETH LEG =================
        // ================= ETH LEG =================
        self.place_leg(
            &opportunity.eth_up_token_id,
            0,
            opportunity.eth_up_price,
            size_dec,
        )
        .await?;

        // ================= BTC LEG =================
        self.place_leg(
            &opportunity.btc_down_token_id,
            0,
            opportunity.btc_down_price,
            size_dec,
        )
        .await?;

        Ok(())
    }

    async fn place_leg(
        &self,
        token_id: &str,
        side: u8, // 0 BUY, 1 SELL
        price: Decimal,
        size: Decimal,
    ) -> Result<()> {
        let price_u256 = to_u256_scaled(price);
        let size_u256 = to_u256_scaled(size);
        
        // Calculate maker/taker amounts
        let (maker_amount, taker_amount) = if side == 0 {
            // BUY: makerAmount = price × size, takerAmount = size
            (price_u256 * size_u256 / U256::from(1_000_000), size_u256)
        } else {
            // SELL: makerAmount = size, takerAmount = price × size
            (size_u256, price_u256 * size_u256 / U256::from(1_000_000))
        };
        
        let order = ClobOrder {
            salt: U256::from(::rand::random::<u64>()),
            maker: Address::from_str(&self.wallet.proxy_wallet)?,
            signer: self.signer.address(),
            taker: Address::zero(),
            token_id: str_to_h256(token_id),
            maker_amount,
            taker_amount,
            side,
            fee_rate_bps: U256::zero(),
            nonce: make_nonce(),
            expiration: U256::from(now_ts() + 300),
        };

        let sig = self.signer.sign_order(&order).await?;

        match self
            .clob
            .submit_order(order, sig, &self.wallet.proxy_wallet)
            .await
        {
            Ok(_) => info!("✅ Order submitted {}", token_id),
            Err(e) => warn!("❌ Order rejected {} → {}", token_id, e),
        }

        Ok(())
    }

    // ==================================================
    // POSITION SIZING
    // ==================================================

    async fn calculate_position_size(&self, opportunity: &ArbitrageOpportunity) -> Result<f64> {
        let bal = self.live_usdc_balance.lock().await;
        let balance = bal.to_f64().unwrap_or(0.0);
        let cost = opportunity.total_cost.to_f64().unwrap_or(1.0);

        let spend = match self.sizing.mode {
            TradeMode::Fixed => self.sizing.fixed_usdc.unwrap_or(0.0),
            TradeMode::Percentage => balance * (self.sizing.percentage.unwrap_or(10.0) / 100.0),
            TradeMode::Dynamic => {
                let edge = opportunity.expected_profit.to_f64().unwrap_or(0.0);
                (balance * 0.01 * (1.0 + edge)).min(balance * 0.25)
            }
            TradeMode::Free => balance,
        };

        Ok((spend / cost).floor())
    }
}
