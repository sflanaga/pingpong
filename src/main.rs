use clap::{Parser, Subcommand};
use crossbeam_channel::{unbounded, Sender};
use std::net::{UdpSocket, SocketAddr};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Logic for the high-precision logging system
struct LogEvent {
    timestamp_ns: u64,
    thread_id: thread::ThreadId,
    level: &'static str,
    data: LogData,
}

enum LogData {
    Info(String),
    Response { seq: u64, delay_ns: u64 },
    Timeout { seq: u64 },
    Error(String),
    DataMismatch { seq: u64, expected: Vec<u8>, actual: Vec<u8> },
}

struct Stats {
    highest_seq: AtomicU64,
    count_total: AtomicU64, // Responses in send, packets in echo
    timeouts_total: AtomicU64,
    sum_delay_ns: AtomicU64,
    min_delay_ns: AtomicU64,
    max_delay_ns: AtomicU64,
}

impl Stats {
    fn new() -> Self {
        Self {
            highest_seq: AtomicU64::new(0),
            count_total: AtomicU64::new(0),
            timeouts_total: AtomicU64::new(0),
            sum_delay_ns: AtomicU64::new(0),
            min_delay_ns: AtomicU64::new(u64::MAX),
            max_delay_ns: AtomicU64::new(0),
        }
    }
}

#[derive(Parser)]
#[command(name = "pingpong")]
struct Cli {
    #[command(subcommand)]
    mode: Mode,
    #[arg(short, long)]
    verbose: bool,
    #[arg(short, long, default_value_t = 1000)]
    ticker_ms: u64,
}

#[derive(Subcommand, Clone)]
enum Mode {
    Send {
        target: String,
        #[arg(long, default_value_t = 6000)]
        timeout: u64,
        #[arg(long, default_value_t = 0)]
        delay: u64,
    },
    Echo {
        #[arg(short, long)]
        port: u16,
    },
}

fn main() {
    let cli = Cli::parse();
    let (log_tx, log_rx) = unbounded::<LogEvent>();
    let stats = Arc::new(Stats::new());

    // --- Logging Thread ---
    thread::spawn(move || {
        while let Ok(event) = log_rx.recv() {
            let datetime = chrono::Utc::now();
            let time_str = datetime.format("%Y-%m-%d %H:%M:%S%.6f").to_string();
            
            let message = match event.data {
                LogData::Info(s) => s,
                LogData::Error(s) => format!("ERROR: {}", s),
                LogData::Timeout { seq } => format!("Timeout for sequence {}", seq),
                LogData::DataMismatch { seq, .. } => format!("DATA MISMATCH on sequence {}", seq),
                LogData::Response { seq, delay_ns } => {
                    let secs = delay_ns as f64 / 1_000_000_000.0;
                    format!("Seq {} Delay: {:.9}", seq, secs)
                }
            };
            println!("[{}] [{:?}] [{}] {}", time_str, event.thread_id, event.level, message);
        }
    });

    // --- Ticker Thread ---
    let ticker_stats = Arc::clone(&stats);
    let ticker_interval = cli.ticker_ms;
    let mode_clone = cli.mode.clone();
    thread::spawn(move || {
        let mut last_count = 0;
        loop {
            thread::sleep(Duration::from_millis(ticker_interval));
            let total = ticker_stats.count_total.load(Ordering::Relaxed);
            let highest = ticker_stats.highest_seq.load(Ordering::Relaxed);
            let delta = total - last_count;
            let rate = delta as f64 / (ticker_interval as f64 / 1000.0);

            match mode_clone {
                Mode::Echo { .. } => {
                    println!("ECHO Tick: Latest Seq: {} | Packets Recv: {} | Rate: {:.2}/s", 
                        highest, total, rate);
                },
                Mode::Send { .. } => {
                    let timeouts = ticker_stats.timeouts_total.load(Ordering::Relaxed);
                    let min = ticker_stats.min_delay_ns.swap(u64::MAX, Ordering::SeqCst);
                    let max = ticker_stats.max_delay_ns.swap(0, Ordering::SeqCst);
                    let sum = ticker_stats.sum_delay_ns.swap(0, Ordering::SeqCst);

                    let (min_s, max_s, avg_s) = if delta > 0 {
                        (format!("{:.9}s", min as f64 / 1e9), format!("{:.9}s", max as f64 / 1e9), format!("{:.9}s", (sum / delta) as f64 / 1e9))
                    } else {
                        ("na".into(), "na".into(), "na".into())
                    };

                    println!("SEND Tick: Seq: {} | echos: {} ({:.2}/s) | Timeouts: {} | Min/Max/Avg: {}/{}/{}",
                        highest, total, rate, timeouts, min_s, max_s, avg_s);
                }
            }
            last_count = total;
        }
    });

    match cli.mode {
        Mode::Echo { port } => run_echo(port, log_tx, stats),
        Mode::Send { target, timeout, delay } => run_send(target, timeout, delay, log_tx, stats, cli.verbose),
    }
}

fn now_ns() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos() as u64
}

fn send_log(tx: &Sender<LogEvent>, level: &'static str, data: LogData) {
    let _ = tx.send(LogEvent { timestamp_ns: now_ns(), thread_id: thread::current().id(), level, data });
}

fn run_echo(port: u16, log_tx: Sender<LogEvent>, stats: Arc<Stats>) {
    let socket = UdpSocket::bind(format!("0.0.0.0:{}", port)).expect("Failed to bind");
    let mut buf = [0u8; 1024];
    loop {
        if let Ok((amt, src)) = socket.recv_from(&mut buf) {
            if amt >= 8 {
                let seq = u64::from_be_bytes(buf[0..8].try_into().unwrap());
                stats.highest_seq.fetch_max(seq, Ordering::Relaxed);
                stats.count_total.fetch_add(1, Ordering::Relaxed);
            }
            let _ = socket.send_to(&buf[..amt], &src);
        }
    }
}

fn run_send(target: String, timeout_ms: u64, delay_ms: u64, log_tx: Sender<LogEvent>, stats: Arc<Stats>, verbose: bool) {
    let socket = UdpSocket::bind("0.0.0.0:0").expect("Failed to bind");
    socket.set_read_timeout(Some(Duration::from_millis(timeout_ms))).unwrap();
    let mut seq = 0u64;
    let mut buf = [0u8; 1024];

    loop {
        seq += 1;
        let start_ns = now_ns();
        let mut packet = [0u8; 16];
        packet[0..8].copy_from_slice(&seq.to_be_bytes());
        packet[8..16].copy_from_slice(&start_ns.to_be_bytes());

        if let Err(e) = socket.send_to(&packet, &target) {
            send_log(&log_tx, "ERROR", LogData::Error(e.to_string()));
        } else {
            match socket.recv_from(&mut buf) {
                Ok((amt, _)) => {
                    let end_ns = now_ns();
                    let received_payload = &buf[..amt];
                    
                    // Cross-check: Verify payload matches what we sent
                    if received_payload != packet {
                        send_log(&log_tx, "ERROR", LogData::DataMismatch { 
                            seq, 
                            expected: packet.to_vec(), 
                            actual: received_payload.to_vec() 
                        });
                    } else {
                        let delay = end_ns - start_ns;
                        stats.count_total.fetch_add(1, Ordering::Relaxed);
                        stats.highest_seq.store(seq, Ordering::Relaxed);
                        stats.sum_delay_ns.fetch_add(delay, Ordering::Relaxed);
                        stats.min_delay_ns.fetch_min(delay, Ordering::SeqCst);
                        stats.max_delay_ns.fetch_max(delay, Ordering::SeqCst);

                        if verbose {
                            send_log(&log_tx, "DEBUG", LogData::Response { seq, delay_ns: delay });
                        }
                    }
                }
                Err(_) => {
                    stats.timeouts_total.fetch_add(1, Ordering::Relaxed);
                    if verbose { send_log(&log_tx, "WARN", LogData::Timeout { seq }); }
                }
            }
        }
        if delay_ms > 0 { thread::sleep(Duration::from_millis(delay_ms)); }
    }
}
