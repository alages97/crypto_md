# Binance WebSocket Market Data Client

A simple Rust application that establishes a WebSocket connection to Binance and fetches real-time market data for a given list of symbols.

## Features

- Connects to Binance WebSocket API
- Subscribes to 24hr ticker data for specified cryptocurrency symbols
- Displays real-time updates of price, change percentage, high, and low values
- Updates the console display with the latest data

## Prerequisites

- Rust and Cargo installed on your system
- Internet connection to access Binance API

## Usage

### Build the application:

```bash
cargo build --release
```

### Run with default symbols (BTCUSDT and ETHUSDT):

```bash
cargo run --release
```

### Run with custom symbols:

```bash
cargo run --release -- btcusdt ethusdt solusdt bnbusdt
```

You can specify any number of trading pairs as command-line arguments. The symbols are case-insensitive.

## Output

The application displays a table with the following information for each symbol:
- Symbol name
- Last price
- 24h price change percentage
- 24h high price
- 24h low price

The display is updated in real-time as new data arrives from Binance.

## Notes

- This application does not require any API keys as it only accesses public market data
- Press Ctrl+C to exit the application 