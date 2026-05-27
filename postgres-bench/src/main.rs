use std::error::Error;
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::{Duration, Instant};

use clap::Parser;
use hdrhistogram::Histogram;
use native_tls::TlsConnector;
use postgres::Client;
use postgres_native_tls::MakeTlsConnector;

type DynError = Box<dyn Error + Send + Sync + 'static>;
type LatencyHistogram = Histogram<u64>;
type PostgresTls = MakeTlsConnector;

#[derive(Debug, Parser)]
#[command(author, version, about)]
struct Args {
    /// Postgres connection string.
    #[arg(
        long,
        env = "DATABASE_URL",
        default_value = "postgresql://postgres:postgres@localhost:5432/postgres"
    )]
    dsn: String,

    /// Number of writer threads.
    #[arg(long, default_value_t = 20)]
    threads: usize,

    /// Size of the bytea payload each thread writes, in bytes.
    #[arg(long, default_value_t = 4096)]
    value_size: usize,

    /// Benchmark duration in seconds. Use 0 to run until Ctrl-C.
    #[arg(long, default_value_t = 10)]
    duration_secs: u64,
}

fn main() -> Result<(), DynError> {
    let args = Args::parse();
    args.validate()?;

    let running = Arc::new(AtomicBool::new(true));
    install_ctrlc_handler(Arc::clone(&running))?;

    setup_table(&args.dsn, args.threads, args.value_size)?;
    let clients = connect_workers(&args.dsn, args.threads)?;

    println!(
        "starting benchmark: threads={}, value_size={} bytes, duration={} seconds",
        args.threads, args.value_size, args.duration_secs
    );

    let barrier = Arc::new(Barrier::new(args.threads + 1));
    let mut handles = Vec::with_capacity(args.threads);

    for (worker_id, client) in clients.into_iter().enumerate() {
        let barrier = Arc::clone(&barrier);
        let running = Arc::clone(&running);
        let value_size = args.value_size;

        handles.push(thread::spawn(move || {
            run_worker(worker_id, client, value_size, barrier, running)
        }));
    }

    barrier.wait();
    let started_at = Instant::now();

    if args.duration_secs == 0 {
        while running.load(Ordering::Relaxed) {
            thread::sleep(Duration::from_millis(100));
        }
    } else {
        thread::sleep(Duration::from_secs(args.duration_secs));
        running.store(false, Ordering::Relaxed);
    }

    let mut total_stats = WorkerStats::new()?;
    for handle in handles {
        let stats = handle
            .join()
            .map_err(|_| io::Error::other("worker thread panicked"))??;
        total_stats.merge(stats)?;
    }

    print_summary(&total_stats, args.value_size, started_at.elapsed());
    Ok(())
}

struct WorkerStats {
    writes: u64,
    latencies: LatencyHistogram,
}

impl WorkerStats {
    fn new() -> Result<Self, DynError> {
        Ok(Self {
            writes: 0,
            latencies: Histogram::new(3)?,
        })
    }

    fn record_write(&mut self, latency: Duration) -> Result<(), DynError> {
        let micros = latency.as_micros().try_into().unwrap_or(u64::MAX);
        self.latencies.record(micros)?;
        self.writes += 1;

        Ok(())
    }

    fn merge(&mut self, other: Self) -> Result<(), DynError> {
        self.writes += other.writes;
        self.latencies.add(&other.latencies)?;

        Ok(())
    }
}

impl Args {
    fn validate(&self) -> Result<(), DynError> {
        if self.threads == 0 {
            return Err("threads must be greater than 0".into());
        }

        if self.value_size == 0 {
            return Err("value-size must be greater than 0".into());
        }

        Ok(())
    }
}

fn install_ctrlc_handler(running: Arc<AtomicBool>) -> Result<(), DynError> {
    ctrlc::set_handler(move || {
        running.store(false, Ordering::Relaxed);
    })?;

    Ok(())
}

fn setup_table(dsn: &str, threads: usize, value_size: usize) -> Result<(), DynError> {
    let mut client = Client::connect(dsn, tls_connector()?)?;

    client.batch_execute(
        r#"
        CREATE TABLE IF NOT EXISTS kv (
            "key" text PRIMARY KEY,
            value bytea NOT NULL
        );

        TRUNCATE TABLE kv;
        "#,
    )?;

    let insert = client.prepare(r#"INSERT INTO kv ("key", value) VALUES ($1, $2)"#)?;

    for worker_id in 0..threads {
        let key = worker_key(worker_id);
        let value = make_value(worker_id, value_size);
        client.execute(&insert, &[&key, &value])?;
    }

    Ok(())
}

fn connect_workers(dsn: &str, threads: usize) -> Result<Vec<Client>, DynError> {
    let mut clients = Vec::with_capacity(threads);

    for _ in 0..threads {
        clients.push(Client::connect(dsn, tls_connector()?)?);
    }

    Ok(clients)
}

fn tls_connector() -> Result<PostgresTls, DynError> {
    let connector = TlsConnector::builder().build()?;
    Ok(MakeTlsConnector::new(connector))
}

fn run_worker(
    worker_id: usize,
    mut client: Client,
    value_size: usize,
    barrier: Arc<Barrier>,
    running: Arc<AtomicBool>,
) -> Result<WorkerStats, DynError> {
    let key = worker_key(worker_id);
    let update = client.prepare(r#"UPDATE kv SET value = $1 WHERE "key" = $2"#)?;
    let mut value = make_value(worker_id, value_size);
    let mut stats = WorkerStats::new()?;

    barrier.wait();

    while running.load(Ordering::Relaxed) {
        write_counter(&mut value, stats.writes);
        let started_at = Instant::now();
        client.execute(&update, &[&value, &key])?;
        stats.record_write(started_at.elapsed())?;
    }

    Ok(stats)
}

fn worker_key(worker_id: usize) -> String {
    format!("worker-{worker_id}")
}

fn make_value(worker_id: usize, value_size: usize) -> Vec<u8> {
    let mut value = vec![0_u8; value_size];

    for (offset, byte) in value.iter_mut().enumerate() {
        *byte = worker_id.wrapping_add(offset).to_le_bytes()[0];
    }

    value
}

fn write_counter(value: &mut [u8], writes: u64) {
    let counter = writes.to_le_bytes();
    let len = value.len().min(counter.len());
    value[..len].copy_from_slice(&counter[..len]);
}

fn print_summary(stats: &WorkerStats, value_size: usize, elapsed: Duration) {
    let elapsed_secs = elapsed.as_secs_f64();
    let writes_per_sec = stats.writes as f64 / elapsed_secs;
    let mib_written = stats.writes as f64 * value_size as f64 / 1024.0 / 1024.0;
    let mib_per_sec = mib_written / elapsed_secs;

    println!("elapsed: {:.3} seconds", elapsed_secs);
    println!("writes: {}", stats.writes);
    println!("writes/sec: {:.2}", writes_per_sec);
    println!("MiB written: {:.2}", mib_written);
    println!("MiB/sec: {:.2}", mib_per_sec);
    println!(
        "latency p50: {}",
        format_latency_micros(stats.latencies.value_at_quantile(0.5))
    );
    println!(
        "latency p95: {}",
        format_latency_micros(stats.latencies.value_at_quantile(0.95))
    );
    println!(
        "latency p99: {}",
        format_latency_micros(stats.latencies.value_at_quantile(0.99))
    );
    println!(
        "latency p99.9: {}",
        format_latency_micros(stats.latencies.value_at_quantile(0.999))
    );
    println!(
        "latency max: {}",
        format_latency_micros(stats.latencies.max())
    );
}

fn format_latency_micros(micros: u64) -> String {
    if micros < 1_000 {
        format!("{micros} us")
    } else if micros < 1_000_000 {
        format!("{:.3} ms", micros as f64 / 1_000.0)
    } else {
        format!("{:.3} s", micros as f64 / 1_000_000.0)
    }
}
