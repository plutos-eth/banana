//! Typed JSON-RPC calls, all of them through the gate.
//!
//! There is no second path to the network in this workspace. Everything here takes a
//! [`Priority`] because the caller always knows whether it is the sniper or the indexer,
//! and getting that wrong is how a backfill starves a live entry.

use alloy_primitives::{Address, B256, Bytes, U256, hex};
use alloy_sol_types::{SolCall, SolValue};
use serde_json::{Value, json};

use crate::abi::IMulticall3;
use crate::addr;
use crate::gate::{Gate, Priority, RpcError};

/// A log as the endpoint returns it, before ABI decoding.
///
/// Note what is **not** here: a timestamp. The endpoint includes a `blockTimestamp` field
/// but it is always `0x0` (measured 2026-09-07), so it is schema rather than data. Times
/// come from the sampled anchor table instead (PLAN.md F2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawLog {
    pub address: Address,
    pub topics: Vec<B256>,
    pub data: Bytes,
    pub block_number: u64,
    pub tx_hash: B256,
    pub tx_index: u64,
    pub log_index: u64,
}

impl RawLog {
    /// The natural key for deduplication and idempotent indexing (spec §6.2).
    pub fn key(&self) -> (B256, u64) {
        (self.tx_hash, self.log_index)
    }

    pub fn topic0(&self) -> Option<B256> {
        self.topics.first().copied()
    }
}

/// A block range plus what to match in it.
#[derive(Debug, Clone, Default)]
pub struct LogFilter {
    pub from_block: u64,
    pub to_block: u64,
    /// Restrict to these contracts. Trade logs cannot use this: every curve is its own
    /// contract, so they are fetched by topic across all addresses and matched
    /// client-side against known curves.
    pub addresses: Vec<Address>,
    /// `topic0` alternatives, ORed. Fetching `CurveBuy`, `CurveSell` and
    /// `SnipeTaxCharged` in one query is what keeps the trade scan to one pass.
    pub topic0: Vec<B256>,
}

impl LogFilter {
    pub fn new(from_block: u64, to_block: u64) -> Self {
        Self {
            from_block,
            to_block,
            ..Default::default()
        }
    }

    pub fn address(mut self, a: Address) -> Self {
        self.addresses.push(a);
        self
    }

    pub fn topics(mut self, t: impl IntoIterator<Item = B256>) -> Self {
        self.topic0.extend(t);
        self
    }

    pub fn blocks(&self) -> u64 {
        self.to_block.saturating_sub(self.from_block) + 1
    }

    fn to_params(&self) -> Value {
        let mut o = serde_json::Map::new();
        o.insert(
            "fromBlock".into(),
            json!(format!("0x{:x}", self.from_block)),
        );
        o.insert("toBlock".into(), json!(format!("0x{:x}", self.to_block)));
        if !self.addresses.is_empty() {
            let list: Vec<String> = self.addresses.iter().map(|a| a.to_string()).collect();
            o.insert("address".into(), json!(list));
        }
        if !self.topic0.is_empty() {
            let list: Vec<String> = self.topic0.iter().map(|t| t.to_string()).collect();
            o.insert("topics".into(), json!([list]));
        }
        json!([Value::Object(o)])
    }
}

/// A transaction, reduced to what the indexer needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TxInfo {
    pub from: Address,
    pub to: Option<Address>,
    pub value: U256,
    /// The calldata. For a launch this carries the point-in-time metadata.
    pub input: Bytes,
    pub block_number: u64,
}

/// A mined transaction's outcome and the logs it emitted.
///
/// The sniper needs this twice: to read the launch transaction's own `CurveBuy` (the dev
/// buy, and the only point-in-time source for it), and to read back what its own buy
/// actually paid from the `CurveBuy` and `SnipeTaxCharged` the buy emitted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Receipt {
    pub tx_hash: B256,
    pub block_number: u64,
    /// `false` means the transaction reverted. Its logs will be empty.
    pub success: bool,
    pub gas_used: u64,
    /// The effective gas price actually charged, which on this chain is the base fee.
    pub effective_gas_price: u128,
    pub logs: Vec<RawLog>,
}

impl Receipt {
    /// Logs emitted by one contract, in order.
    pub fn logs_from(&self, who: Address) -> impl Iterator<Item = &RawLog> {
        self.logs.iter().filter(move |l| l.address == who)
    }

    /// What the transaction cost in wei.
    pub fn fee_wei(&self) -> U256 {
        U256::from(self.gas_used).saturating_mul(U256::from(self.effective_gas_price))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlockHeader {
    pub number: u64,
    pub timestamp: u64,
}

/// One result from a Multicall3 `aggregate3`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallResult {
    pub success: bool,
    pub data: Bytes,
}

impl CallResult {
    /// Decode a successful call, or `None` if it reverted.
    ///
    /// `allowFailure` is always on, so one reverting getter never hides the rest of a
    /// bundle (spec §7).
    pub fn decode<C: SolCall>(&self) -> Option<C::Return> {
        if !self.success {
            return None;
        }
        C::abi_decode_returns(&self.data).ok()
    }
}

/// Typed access to the chain. Cheap to clone; all clones share one gate.
#[derive(Debug, Clone)]
pub struct Client {
    gate: Gate,
}

impl Client {
    pub fn new(gate: Gate) -> Self {
        Self { gate }
    }

    pub fn gate(&self) -> &Gate {
        &self.gate
    }

    pub async fn chain_id(&self, p: Priority) -> Result<u64, RpcError> {
        let v = self.gate.call("eth_chainId", json!([]), p).await?;
        parse_u64(&v, "eth_chainId")
    }

    pub async fn block_number(&self, p: Priority) -> Result<u64, RpcError> {
        let v = self.gate.call("eth_blockNumber", json!([]), p).await?;
        parse_u64(&v, "eth_blockNumber")
    }

    /// `eth_call` against the latest block, decoded through the `sol!` binding.
    pub async fn call<C: SolCall>(
        &self,
        to: Address,
        call: &C,
        p: Priority,
    ) -> Result<C::Return, RpcError> {
        let data = hex::encode_prefixed(call.abi_encode());
        let v = self
            .gate
            .call(
                "eth_call",
                json!([{ "to": to.to_string(), "data": data }, "latest"]),
                p,
            )
            .await?;
        let raw = v.as_str().ok_or_else(|| RpcError::Malformed {
            label: "eth_call".into(),
            detail: "result is not a hex string".into(),
        })?;
        let bytes = hex::decode(raw).map_err(|e| RpcError::Malformed {
            label: "eth_call".into(),
            detail: e.to_string(),
        })?;
        C::abi_decode_returns(&bytes).map_err(|e| RpcError::Malformed {
            label: "eth_call".into(),
            detail: format!("cannot decode return: {e}"),
        })
    }

    /// Fold many reads into one `eth_call` through Multicall3.
    ///
    /// This is the only batching the public RPC accepts: a JSON-RPC batch array has every
    /// call inside it counted separately, but `aggregate3` is one request (spec §1).
    pub async fn multicall(
        &self,
        calls: Vec<(Address, Vec<u8>)>,
        p: Priority,
    ) -> Result<Vec<CallResult>, RpcError> {
        let call3: Vec<IMulticall3::Call3> = calls
            .into_iter()
            .map(|(target, data)| IMulticall3::Call3 {
                target,
                allowFailure: true,
                callData: data.into(),
            })
            .collect();
        let call = IMulticall3::aggregate3Call { calls: call3 };
        let results = self.call(addr::MULTICALL3, &call, p).await?;
        Ok(results
            .into_iter()
            .map(|r| CallResult {
                success: r.success,
                data: r.returnData,
            })
            .collect())
    }

    /// `eth_getLogs`.
    ///
    /// A [`RpcError::TooManyResults`] here is not a failure to retry: it means the range
    /// matched more than the endpoint will return and the caller must split it. The
    /// indexer does exactly that, recursively.
    pub async fn get_logs(&self, filter: &LogFilter, p: Priority) -> Result<Vec<RawLog>, RpcError> {
        let v = self.gate.call("eth_getLogs", filter.to_params(), p).await?;
        let arr = v.as_array().ok_or_else(|| RpcError::Malformed {
            label: "eth_getLogs".into(),
            detail: "result is not an array".into(),
        })?;
        arr.iter().map(parse_log).collect()
    }

    pub async fn get_transaction(
        &self,
        hash: B256,
        p: Priority,
    ) -> Result<Option<TxInfo>, RpcError> {
        let v = self
            .gate
            .call("eth_getTransactionByHash", json!([hash.to_string()]), p)
            .await?;
        if v.is_null() {
            return Ok(None);
        }
        Ok(Some(parse_tx(&v)?))
    }

    /// A mined transaction's receipt, or `None` while it is still pending.
    ///
    /// `None` is not an error: a receipt asked for too soon simply is not there yet, and
    /// the caller polls. An error means the endpoint could not answer.
    pub async fn get_transaction_receipt(
        &self,
        hash: B256,
        p: Priority,
    ) -> Result<Option<Receipt>, RpcError> {
        let v = self
            .gate
            .call("eth_getTransactionReceipt", json!([hash.to_string()]), p)
            .await?;
        if v.is_null() {
            return Ok(None);
        }
        Ok(Some(parse_receipt(&v)?))
    }

    /// A block header without its transactions.
    ///
    /// `false` for the second parameter matters: including full transaction bodies would
    /// multiply the response size of the timestamp-anchor scan for data it does not use.
    pub async fn get_block_header(
        &self,
        number: u64,
        p: Priority,
    ) -> Result<Option<BlockHeader>, RpcError> {
        let v = self
            .gate
            .call(
                "eth_getBlockByNumber",
                json!([format!("0x{number:x}"), false]),
                p,
            )
            .await?;
        if v.is_null() {
            return Ok(None);
        }
        Ok(Some(BlockHeader {
            number: parse_u64(v.get("number").unwrap_or(&Value::Null), "block.number")?,
            timestamp: parse_u64(
                v.get("timestamp").unwrap_or(&Value::Null),
                "block.timestamp",
            )?,
        }))
    }

    /// An account's balance in wei.
    pub async fn balance(&self, a: Address, p: Priority) -> Result<U256, RpcError> {
        let v = self
            .gate
            .call("eth_getBalance", json!([a.to_string(), "latest"]), p)
            .await?;
        parse_u256(&v, "eth_getBalance")
    }

    /// The next nonce, counting transactions this node has seen but not yet mined.
    ///
    /// `pending` rather than `latest`: with one transaction in flight at a time the two
    /// agree, but `latest` would reuse a nonce the moment that stops being true, and a
    /// replaced transaction is a very expensive way to find out.
    pub async fn next_nonce(&self, a: Address, p: Priority) -> Result<u64, RpcError> {
        let v = self
            .gate
            .call(
                "eth_getTransactionCount",
                json!([a.to_string(), "pending"]),
                p,
            )
            .await?;
        parse_u64(&v, "eth_getTransactionCount")
    }

    /// The current gas price.
    ///
    /// On this chain there is no priority-fee auction — no mempool to bid into (spec §2) —
    /// so this is the base fee and it is what a transaction actually pays.
    pub async fn gas_price(&self, p: Priority) -> Result<u128, RpcError> {
        let v = self.gate.call("eth_gasPrice", json!([]), p).await?;
        Ok(parse_u256(&v, "eth_gasPrice")?.saturating_to::<u128>())
    }

    /// Simulate a call and return the gas it would use.
    ///
    /// A revert arrives as [`RpcError::Rpc`] carrying the endpoint's own message — usually
    /// `execution reverted` and the contract's reason. That is the difference between "the
    /// order would fail" and "the endpoint is down", and the caller must not conflate them:
    /// the first is a refusal to report, the second is worth retrying.
    pub async fn estimate_gas(
        &self,
        from: Address,
        to: Address,
        value: U256,
        data: &[u8],
        p: Priority,
    ) -> Result<u64, RpcError> {
        let params = json!([{
            "from": from.to_string(),
            "to": to.to_string(),
            "value": format!("0x{value:x}"),
            "data": hex::encode_prefixed(data),
        }]);
        let v = self.gate.call("eth_estimateGas", params, p).await?;
        parse_u64(&v, "eth_estimateGas")
    }

    /// `eth_call`, returning raw bytes, so a caller can simulate an order before sending it.
    pub async fn call_raw(
        &self,
        from: Address,
        to: Address,
        value: U256,
        data: &[u8],
        p: Priority,
    ) -> Result<Bytes, RpcError> {
        let params = json!([{
            "from": from.to_string(),
            "to": to.to_string(),
            "value": format!("0x{value:x}"),
            "data": hex::encode_prefixed(data),
        }, "latest"]);
        let v = self.gate.call("eth_call", params, p).await?;
        let raw = v.as_str().unwrap_or("0x");
        Ok(hex::decode(raw)
            .map_err(|e| malformed("eth_call", e.to_string()))?
            .into())
    }

    /// Broadcast a signed transaction.
    ///
    /// The only method in this file that changes anything. Producing its argument requires
    /// a signature, and a signature requires the key, which exists in `quarrel-live` and
    /// nowhere else — so this being here does not widen the trust boundary of spec §3.8.
    pub async fn send_raw_transaction(&self, raw: &[u8], p: Priority) -> Result<B256, RpcError> {
        let v = self
            .gate
            .call(
                "eth_sendRawTransaction",
                json!([hex::encode_prefixed(raw)]),
                p,
            )
            .await?;
        parse_b256(&v, "eth_sendRawTransaction")
    }

    /// Whether an address has code. Used by `doctor` to verify every hardcoded address is
    /// still a contract.
    pub async fn has_code(&self, a: Address, p: Priority) -> Result<usize, RpcError> {
        let v = self
            .gate
            .call("eth_getCode", json!([a.to_string(), "latest"]), p)
            .await?;
        let s = v.as_str().unwrap_or("0x");
        Ok(s.len().saturating_sub(2) / 2)
    }
}

// --- parsing --------------------------------------------------------------------------

fn malformed(what: &str, detail: impl Into<String>) -> RpcError {
    RpcError::Malformed {
        label: what.to_owned(),
        detail: detail.into(),
    }
}

fn parse_u64(v: &Value, what: &str) -> Result<u64, RpcError> {
    let s = v
        .as_str()
        .ok_or_else(|| malformed(what, "expected a hex string"))?;
    u64::from_str_radix(s.trim_start_matches("0x"), 16)
        .map_err(|e| malformed(what, format!("{s}: {e}")))
}

fn parse_u256(v: &Value, what: &str) -> Result<U256, RpcError> {
    let s = v
        .as_str()
        .ok_or_else(|| malformed(what, "expected a hex string"))?;
    U256::from_str_radix(s.trim_start_matches("0x"), 16)
        .map_err(|e| malformed(what, format!("{s}: {e}")))
}

fn parse_addr(v: &Value, what: &str) -> Result<Address, RpcError> {
    v.as_str()
        .ok_or_else(|| malformed(what, "expected an address string"))?
        .parse()
        .map_err(|e| malformed(what, format!("{e:?}")))
}

fn parse_b256(v: &Value, what: &str) -> Result<B256, RpcError> {
    v.as_str()
        .ok_or_else(|| malformed(what, "expected a 32-byte hex string"))?
        .parse()
        .map_err(|e| malformed(what, format!("{e:?}")))
}

fn parse_log(v: &Value) -> Result<RawLog, RpcError> {
    let topics = v
        .get("topics")
        .and_then(|t| t.as_array())
        .ok_or_else(|| malformed("log", "no topics"))?
        .iter()
        .map(|t| parse_b256(t, "log.topic"))
        .collect::<Result<Vec<_>, _>>()?;
    let data_str = v.get("data").and_then(|d| d.as_str()).unwrap_or("0x");
    let data = hex::decode(data_str)
        .map_err(|e| malformed("log.data", e.to_string()))?
        .into();
    Ok(RawLog {
        address: parse_addr(v.get("address").unwrap_or(&Value::Null), "log.address")?,
        topics,
        data,
        block_number: parse_u64(
            v.get("blockNumber").unwrap_or(&Value::Null),
            "log.blockNumber",
        )?,
        tx_hash: parse_b256(
            v.get("transactionHash").unwrap_or(&Value::Null),
            "log.transactionHash",
        )?,
        tx_index: parse_u64(
            v.get("transactionIndex").unwrap_or(&Value::Null),
            "log.transactionIndex",
        )?,
        log_index: parse_u64(v.get("logIndex").unwrap_or(&Value::Null), "log.logIndex")?,
    })
}

fn parse_receipt(v: &Value) -> Result<Receipt, RpcError> {
    let logs = v
        .get("logs")
        .and_then(|l| l.as_array())
        .ok_or_else(|| malformed("receipt", "no logs array"))?
        .iter()
        .map(parse_log)
        .collect::<Result<Vec<_>, _>>()?;
    // `status` is 0x1 or 0x0. A receipt without one predates Byzantium and cannot occur
    // on this chain, so its absence is malformed rather than a case to guess at.
    let status = parse_u64(v.get("status").unwrap_or(&Value::Null), "receipt.status")?;
    Ok(Receipt {
        tx_hash: parse_b256(
            v.get("transactionHash").unwrap_or(&Value::Null),
            "receipt.transactionHash",
        )?,
        block_number: parse_u64(
            v.get("blockNumber").unwrap_or(&Value::Null),
            "receipt.blockNumber",
        )?,
        success: status == 1,
        gas_used: parse_u64(v.get("gasUsed").unwrap_or(&Value::Null), "receipt.gasUsed")?,
        effective_gas_price: parse_u64(
            v.get("effectiveGasPrice").unwrap_or(&Value::Null),
            "receipt.effectiveGasPrice",
        )? as u128,
        logs,
    })
}

fn parse_tx(v: &Value) -> Result<TxInfo, RpcError> {
    let to = match v.get("to") {
        Some(Value::String(s)) => Some(
            s.parse()
                .map_err(|e| malformed("tx.to", format!("{e:?}")))?,
        ),
        // Contract creation has a null `to`.
        _ => None,
    };
    let input_str = v.get("input").and_then(|i| i.as_str()).unwrap_or("0x");
    Ok(TxInfo {
        from: parse_addr(v.get("from").unwrap_or(&Value::Null), "tx.from")?,
        to,
        value: parse_u256(v.get("value").unwrap_or(&Value::Null), "tx.value")?,
        input: hex::decode(input_str)
            .map_err(|e| malformed("tx.input", e.to_string()))?
            .into(),
        block_number: parse_u64(
            v.get("blockNumber").unwrap_or(&Value::Null),
            "tx.blockNumber",
        )?,
    })
}

/// ABI-encode a call, for building a multicall bundle.
pub fn encode<C: SolCall>(call: &C) -> Vec<u8> {
    call.abi_encode()
}

/// Decode a Solidity value out of raw returndata.
pub fn decode_value<T: SolValue + From<<T::SolType as alloy_sol_types::SolType>::RustType>>(
    data: &[u8],
) -> Option<T> {
    T::abi_decode(data).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::abi::{IPonsCurve, IPonsFactory};
    use crate::gate::{Endpoint, GateConfig};
    use crate::transport::{HttpResponse, Transport, TransportError};
    use alloy_sol_types::SolEvent;
    use async_trait::async_trait;
    use std::sync::Mutex;

    /// Captures the request body so tests can assert on the wire format.
    #[derive(Debug)]
    struct Spy {
        body: Mutex<String>,
        reply: String,
    }

    #[async_trait]
    impl Transport for Spy {
        async fn post(&self, _url: &str, body: &str) -> Result<HttpResponse, TransportError> {
            *self.body.lock().unwrap() = body.to_owned();
            Ok(HttpResponse {
                status: 200,
                body: self.reply.clone(),
            })
        }
    }

    fn client_with(reply: &str) -> (Client, std::sync::Arc<Spy>) {
        let spy = std::sync::Arc::new(Spy {
            body: Mutex::new(String::new()),
            reply: reply.to_owned(),
        });
        #[derive(Debug)]
        struct Shared(std::sync::Arc<Spy>);
        #[async_trait]
        impl Transport for Shared {
            async fn post(&self, u: &str, b: &str) -> Result<HttpResponse, TransportError> {
                self.0.post(u, b).await
            }
        }
        let gate = Gate::new(
            Box::new(Shared(spy.clone())),
            vec![Endpoint::new("https://x", "x", true)],
            GateConfig {
                spacing: std::time::Duration::ZERO,
                logs_spacing: std::time::Duration::ZERO,
                ..Default::default()
            },
        );
        (Client::new(gate), spy)
    }

    #[tokio::test]
    async fn chain_id_parses_the_hex_the_endpoint_returned() {
        // 0x1237 is what the live endpoint actually answered on 2026-09-07.
        let (c, _) = client_with(r#"{"jsonrpc":"2.0","id":1,"result":"0x1237"}"#);
        assert_eq!(c.chain_id(Priority::Hot).await.unwrap(), 4663);
    }

    #[tokio::test]
    async fn a_log_filter_serialises_topics_as_an_or_group() {
        // topics: [[a, b]] means "topic0 is a OR b", which is what lets one scan cover
        // CurveBuy, CurveSell and SnipeTaxCharged together.
        let (c, spy) = client_with(r#"{"jsonrpc":"2.0","id":1,"result":[]}"#);
        let f = LogFilter::new(100, 200).topics([
            IPonsCurve::CurveBuy::SIGNATURE_HASH,
            IPonsCurve::CurveSell::SIGNATURE_HASH,
        ]);
        c.get_logs(&f, Priority::Bulk).await.unwrap();

        let body = spy.body.lock().unwrap().clone();
        let v: Value = serde_json::from_str(&body).unwrap();
        let params = &v["params"][0];
        assert_eq!(params["fromBlock"], "0x64");
        assert_eq!(params["toBlock"], "0xc8");
        let topics = params["topics"].as_array().unwrap();
        assert_eq!(topics.len(), 1, "one position");
        assert_eq!(topics[0].as_array().unwrap().len(), 2, "two alternatives");
        assert!(
            params.get("address").is_none(),
            "trade logs span all curves"
        );
    }

    #[tokio::test]
    async fn an_address_filtered_query_names_the_factory() {
        let (c, spy) = client_with(r#"{"jsonrpc":"2.0","id":1,"result":[]}"#);
        let f = LogFilter::new(1, 2)
            .address(addr::PONS_FACTORY)
            .topics([IPonsFactory::TokenLaunched::SIGNATURE_HASH]);
        c.get_logs(&f, Priority::Bulk).await.unwrap();

        let v: Value = serde_json::from_str(&spy.body.lock().unwrap()).unwrap();
        let addrs = v["params"][0]["address"].as_array().unwrap();
        assert_eq!(addrs.len(), 1);
    }

    #[tokio::test]
    async fn logs_parse_into_the_natural_key_used_for_idempotent_indexing() {
        // A real CurveSell log body, trimmed.
        let reply = r#"{"jsonrpc":"2.0","id":1,"result":[{
            "address":"0x750d0a9a4533af75f1cc62138b2ded19eb1b8dc0",
            "topics":["0x8113d738abdcb6b38357e9d53a54a7157861a09031b453651f0fe7fe151f59df",
                      "0x000000000000000000000000ed090594a014208a177bb23fbb921aa3997eb1f9",
                      "0x000000000000000000000000ed090594a014208a177bb23fbb921aa3997eb1f9"],
            "data":"0x00",
            "blockNumber":"0x3613001",
            "transactionHash":"0x3de20a599bd5cb137ff468b6529f8b93fafe742c3f46fc56e70df2d486ad9d19",
            "transactionIndex":"0x2",
            "logIndex":"0x7"
        }]}"#;
        let (c, _) = client_with(reply);
        let logs = c
            .get_logs(&LogFilter::new(1, 2), Priority::Bulk)
            .await
            .unwrap();
        assert_eq!(logs.len(), 1);
        let l = &logs[0];
        assert_eq!(l.block_number, 0x3613001);
        assert_eq!(l.log_index, 7);
        assert_eq!(l.topic0(), Some(IPonsCurve::CurveSell::SIGNATURE_HASH));
        assert_eq!(l.key().1, 7);
    }

    #[tokio::test]
    async fn a_missing_transaction_is_none_not_an_error() {
        let (c, _) = client_with(r#"{"jsonrpc":"2.0","id":1,"result":null}"#);
        assert_eq!(
            c.get_transaction(B256::ZERO, Priority::Bulk).await.unwrap(),
            None
        );
    }

    #[tokio::test]
    async fn a_block_header_request_does_not_ask_for_full_transactions() {
        let (c, spy) = client_with(
            r#"{"jsonrpc":"2.0","id":1,"result":{"number":"0x3555bd3","timestamp":"0x6a9c5ef6"}}"#,
        );
        let h = c
            .get_block_header(55_990_739, Priority::Bulk)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(h.number, 0x3555bd3);
        assert_eq!(h.timestamp, 0x6a9c5ef6);

        let v: Value = serde_json::from_str(&spy.body.lock().unwrap()).unwrap();
        assert_eq!(
            v["params"][1], false,
            "full transaction bodies would bloat the anchor scan for data it never reads"
        );
    }

    #[tokio::test]
    async fn a_contract_creation_transaction_has_no_to_address() {
        let reply = r#"{"jsonrpc":"2.0","id":1,"result":{
            "from":"0x192de377d718c13d3bb4e48dd2a7675b66521a47","to":null,
            "value":"0x0","input":"0x1234","blockNumber":"0x1"}}"#;
        let (c, _) = client_with(reply);
        let tx = c
            .get_transaction(B256::ZERO, Priority::Bulk)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(tx.to, None);
        assert_eq!(tx.input.as_ref(), &[0x12, 0x34]);
    }

    #[test]
    fn a_filter_reports_its_own_block_span() {
        assert_eq!(LogFilter::new(100, 100).blocks(), 1);
        assert_eq!(LogFilter::new(0, 3_999).blocks(), 4_000);
    }

    #[test]
    fn a_reverted_multicall_entry_decodes_to_none_rather_than_poisoning_the_bundle() {
        // allowFailure is always on: one reverting getter must not hide the rest.
        let failed = CallResult {
            success: false,
            data: Bytes::new(),
        };
        assert!(failed.decode::<IPonsCurve::feeBpsCall>().is_none());
    }
}
