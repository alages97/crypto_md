use std::collections::BTreeMap;
use std::env;
use std::time::{SystemTime, UNIX_EPOCH};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::mpsc;
use tokio_tungstenite::{connect_async, tungstenite::protocol::Message};
use url::Url;
use colored::*;

// Constants
const BINANCE_WS_URL: &str = "wss://stream.binance.com:9443/ws";  // Main Binance WebSocket endpoint
const CHANNEL_CAPACITY: usize = 1024 * 16; // Larger buffer for high throughput
const BOOK_CAPACITY: usize = 1000; // Pre-allocated capacity for order book levels
const LATENCY_BUFFER_SIZE: usize = 1000;

// Precise timestamp tracking
#[derive(Debug, Copy, Clone)]
#[repr(C, align(8))] // Memory alignment for better performance
struct Timestamp {
    secs: i64,
    nanos: u32,
}

impl Timestamp {
    #[inline(always)]
    fn now() -> Self {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap();
        Self {
            secs: now.as_secs() as i64,
            nanos: now.subsec_nanos(),
        }
    }
}

// L2 Market Data Structures
#[derive(Debug, Clone)]
#[repr(C, align(8))]
struct PriceLevel {
    price: f64,
    quantity: f64,
    timestamp: Timestamp,
}

#[derive(Debug, Clone)]
struct OrderBook {
    symbol: String,
    bids: BTreeMap<u64, PriceLevel>,
    asks: BTreeMap<u64, PriceLevel>,
    last_update_id: u64,
    timestamp: Timestamp,
    capacity: usize,
}

impl OrderBook {
    fn with_capacity(symbol: String, capacity: usize) -> Self {
        Self {
            symbol,
            bids: BTreeMap::new(),
            asks: BTreeMap::new(),
            last_update_id: 0,
            timestamp: Timestamp::now(),
            capacity,
        }
    }

    #[inline]
    fn update_bid(&mut self, price_raw: u64, price: f64, quantity: f64, timestamp: Timestamp) {
        if quantity > 0.0 {
            if self.bids.len() < self.capacity {
                self.bids.insert(price_raw, PriceLevel {
                    price,
                    quantity,
                    timestamp,
                });
            }
        } else {
            self.bids.remove(&price_raw);
        }
    }

    #[inline]
    fn update_ask(&mut self, price_raw: u64, price: f64, quantity: f64, timestamp: Timestamp) {
        if quantity > 0.0 {
            if self.asks.len() < self.capacity {
                self.asks.insert(price_raw, PriceLevel {
                    price,
                    quantity,
                    timestamp,
                });
            }
        } else {
            self.asks.remove(&price_raw);
        }
    }
}

// Ring buffer for latency measurements
struct LatencyStats {
    buffer: Vec<i64>,
    position: usize,
    count: usize,
}

impl LatencyStats {
    fn new(capacity: usize) -> Self {
        Self {
            buffer: vec![0; capacity],
            position: 0,
            count: 0,
        }
    }

    #[inline]
    fn add(&mut self, latency: i64) {
        self.buffer[self.position] = latency;
        self.position = (self.position + 1) % self.buffer.len();
        self.count = self.count.saturating_add(1);
    }

    fn stats(&self) -> (i64, i64, i64) { // min, max, avg
        if self.count == 0 {
            return (0, 0, 0);
        }
        let samples = &self.buffer[..self.count.min(self.buffer.len())];
        let mut min = i64::MAX;
        let mut max = i64::MIN;
        let mut sum = 0i64;
        
        for &latency in samples {
            min = min.min(latency);
            max = max.max(latency);
            sum += latency;
        }
        
        (min, max, sum / samples.len() as i64)
    }
}

// Binance specific structures
#[derive(Deserialize, Debug)]
struct OrderBookUpdate {
    #[serde(rename = "s")]
    symbol: String,
    #[serde(rename = "U")]
    first_update_id: u64,
    #[serde(rename = "u")]
    last_update_id: u64,
    #[serde(rename = "b")]
    bids: Vec<[String; 2]>,
    #[serde(rename = "a")]
    asks: Vec<[String; 2]>,
    #[serde(rename = "E")]
    event_time: u64,  // Exchange timestamp in milliseconds
}

#[derive(Serialize)]
struct SubscriptionMessage {
    method: String,
    params: Vec<String>,
    id: u64,
}

// Market data processor
struct MarketDataProcessor {
    order_books: BTreeMap<String, OrderBook>,
    updates_processed: u64,
    latency_stats: LatencyStats,
    last_stats_time: SystemTime,
}

impl MarketDataProcessor {
    fn new() -> Self {
        Self {
            order_books: BTreeMap::new(),
            updates_processed: 0,
            latency_stats: LatencyStats::new(LATENCY_BUFFER_SIZE),
            last_stats_time: SystemTime::now(),
        }
    }

    #[inline]
    fn process_update(&mut self, update: OrderBookUpdate, receive_timestamp: Timestamp) {
        let symbol = update.symbol.clone();
        let book = self.order_books.entry(symbol.clone()).or_insert_with(|| {
            OrderBook::with_capacity(symbol.clone(), BOOK_CAPACITY)
        });

        // Update book
        for bid in update.bids {
            let price = (bid[0].parse::<f64>().unwrap() * 10000.0) as u64;
            let quantity = bid[1].parse::<f64>().unwrap();
            book.update_bid(price, price as f64 / 10000.0, quantity, receive_timestamp);
        }

        for ask in update.asks {
            let price = (ask[0].parse::<f64>().unwrap() * 10000.0) as u64;
            let quantity = ask[1].parse::<f64>().unwrap();
            book.update_ask(price, price as f64 / 10000.0, quantity, receive_timestamp);
        }

        book.last_update_id = update.last_update_id;
        book.timestamp = receive_timestamp;
        
        self.updates_processed += 1;
        
        // Calculate latencies
        let now = Timestamp::now();
        let processing_latency = if now.secs > receive_timestamp.secs {
            (now.secs - receive_timestamp.secs) * 1_000_000_000 + 
            (now.nanos as i64 - receive_timestamp.nanos as i64)
        } else if now.secs == receive_timestamp.secs && now.nanos >= receive_timestamp.nanos {
            now.nanos as i64 - receive_timestamp.nanos as i64
        } else {
            0
        };
        
        // Get timestamps in milliseconds
        let receive_time_ms = receive_timestamp.secs as u64 * 1000 + 
                            (receive_timestamp.nanos as u64 / 1_000_000);
        let current_time_ms = (now.secs * 1000) as u64 + 
                            (now.nanos as u64 / 1_000_000);

        // Network latency is the difference between event time and receive time
        let network_latency_ms = if update.event_time > receive_time_ms {
            update.event_time - receive_time_ms  // Event is in the future relative to receive time
        } else {
            receive_time_ms - update.event_time  // Event is in the past relative to receive time
        };

        self.latency_stats.add(processing_latency);

        // Print stats every second
        if now.secs > self.last_stats_time.elapsed().unwrap().as_secs() as i64 {
            let (min, max, avg) = self.latency_stats.stats();
            println!("\nTiming Analysis:");
            println!("Current time (ms):  {}", current_time_ms);
            println!("Event time (ms):    {}", update.event_time);
            println!("Receive time (ms):  {}", receive_time_ms);
            println!("Network latency (ms): {}", network_latency_ms);
            self.last_stats_time = SystemTime::now();
        }
        
        if let (Some((&best_bid_price, best_bid)), Some((&best_ask_price, best_ask))) = 
            (book.bids.iter().next_back(), book.asks.iter().next()) {
            println!(
                "{} | {} {:>10.8} @ {:>10.6} | {} {:>10.8} @ {:>10.6} | {} {:>10.8} | {} {:>10} | {} {:>4}µs | {} {:>3}ms",
                symbol.bright_white().on_blue(),
                "Bid:".bright_green(),
                best_bid.price,
                best_bid.quantity,
                "Ask:".bright_red(),
                best_ask.price,
                best_ask.quantity,
                "Spread:".yellow(),
                best_ask.price - best_bid.price,
                "ID:".cyan(),
                update.last_update_id,
                "Latency:".purple(),
                processing_latency / 1000,
                "Net:".blue(),
                network_latency_ms
            );
        }
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let symbols = if env::args().len() > 1 {
        env::args().skip(1).collect::<Vec<String>>()
    } else {
        vec!["btcusdt".to_string()]
    };
    
    println!("Starting L2 market data collection for: {:?}", symbols);

    // Create channels
    let (tx, mut rx) = mpsc::channel::<(OrderBookUpdate, Timestamp)>(CHANNEL_CAPACITY);

    // Connect to WebSocket
    let url = Url::parse(BINANCE_WS_URL)?;
    let (ws_stream, _) = connect_async(url).await?;
    let (mut write, mut read) = ws_stream.split();

    // Subscribe to depth streams
    let streams: Vec<String> = symbols
        .iter()
        .map(|s| format!("{}@depth@100ms", s.to_lowercase()))
        .collect();

    let subscription = SubscriptionMessage {
        method: "SUBSCRIBE".to_string(),
        params: streams,
        id: 1,
    };

    write.send(Message::Text(serde_json::to_string(&subscription)?)).await?;
    println!("Subscribed to order book streams");

    // Spawn message handler
    let handle_messages = tokio::spawn(async move {
        while let Some(msg) = read.next().await {
            match msg {
                Ok(Message::Text(text)) => {
                    let current_time = SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .unwrap()
                        .as_millis();
                    //println!("\nRaw message: {}", text);
                    let receive_time = Timestamp::now();
                    if let Ok(update) = serde_json::from_str::<OrderBookUpdate>(&text) {
                        if let Err(_) = tx.send((update, receive_time)).await {
                            break;
                        }
                    }
                }
                Ok(Message::Ping(_)) => {
                    println!("Received ping");
                }
                Err(e) => {
                    eprintln!("Error receiving message: {:?}", e);
                    break;
                }
                _ => {}
            }
        }
    });

    // Process market data
    let mut processor = MarketDataProcessor::new();
    
    while let Some((update, timestamp)) = rx.recv().await {
        processor.process_update(update, timestamp);
    }

    handle_messages.await?;
    Ok(())
}
