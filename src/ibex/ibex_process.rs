use crate::error::BenchmarkError::OtherError;
use crate::error::BenchmarkResult;
use crate::process_monitor::ProcessMonitor;
use crate::utils::ibex_binary_path;
use crate::{
    prometheus_metrics, CPU_USAGE_GAUGE, IBEX_CPU_USAGE_GAUGE, IBEX_MEM_USAGE_GAUGE,
    IBEX_NODES_GAUGE, IBEX_RELATIONSHIPS_GAUGE, IBEX_RESTART_COUNTER, MEM_USAGE_GAUGE,
};
use prometheus::core::{AtomicU64, GenericCounter};
use serde::Deserialize;
use std::sync::{Arc, Mutex};
use sysinfo::{Pid, System};
use tokio::task::JoinHandle;
use tracing::info;

const IBEX_HOST: &str = "127.0.0.1";
// NOTE: deliberately not 8080 — the benchmark's own Prometheus endpoint
// (see prometheus_endpoint.rs) binds 0.0.0.0:8080, and that would collide
// with `ibexdb server`'s default port on the same host.
const IBEX_PORT: u16 = 8088;
pub(crate) const IBEX_DATA_DIR: &str = "./ibex-data";

pub fn ibex_endpoint() -> String {
    std::env::var("IBEX_ENDPOINT")
        .unwrap_or_else(|_| format!("http://{}:{}", IBEX_HOST, IBEX_PORT))
}

/// Mirror of `ibexdb_server::StatsResponse` — that struct is private to the
/// server crate, so the benchmark fetches `/api/stats` over HTTP and decodes
/// only the fields it needs.
#[derive(Debug, Deserialize)]
struct StatsResponse {
    num_nodes: u64,
    num_edges: u64,
}

pub(crate) async fn fetch_stats() -> BenchmarkResult<(u64, u64)> {
    let url = format!("{}/api/stats", ibex_endpoint());
    let response = reqwest::get(&url).await?;
    let stats: StatsResponse = response.json().await?;
    Ok((stats.num_nodes, stats.num_edges))
}

#[derive(Default)]
pub struct IbexProcess {
    shutdown_tx: Option<tokio::sync::oneshot::Sender<()>>,
    process_handle: Option<JoinHandle<()>>,
    prom_shutdown_tx: Option<tokio::sync::oneshot::Sender<()>>,
    prom_process_handle: Option<JoinHandle<()>>,
    dropped: bool,
}

impl IbexProcess {
    pub async fn new(database: &str) -> BenchmarkResult<Self> {
        let (prom_process_handle, prom_shutdown_tx) =
            prometheus_metrics::run_metrics_reporter(report_metrics);

        // IBEX_EXTERNAL: connect to an already-running `ibexdb server` (e.g. the
        // dockerized instance from docker-compose.yml) instead of spawning and
        // managing a local one. Metrics reporting still runs against it.
        if std::env::var("IBEX_EXTERNAL").is_ok() {
            info!("IBEX_EXTERNAL set — connecting to externally managed ibexdb server");
            return Ok(Self {
                shutdown_tx: None,
                process_handle: None,
                prom_shutdown_tx: Some(prom_shutdown_tx),
                prom_process_handle: Some(prom_process_handle),
                dropped: false,
            });
        }

        let command = ibex_binary_path()?;
        let args: Vec<String> = vec![
            "server".to_string(),
            "--database".to_string(),
            database.to_string(),
            "--host".to_string(),
            IBEX_HOST.to_string(),
            "--port".to_string(),
            IBEX_PORT.to_string(),
        ];

        let (mut process_monitor, shutdown_tx) = ProcessMonitor::new(
            command,
            args,
            Default::default(),
            std::time::Duration::from_secs(5),
        );
        let counter: GenericCounter<AtomicU64> = IBEX_RESTART_COUNTER.clone();
        let ibex_process_monitor = tokio::spawn(async move {
            let _ = process_monitor.run(counter).await;
        });
        let process_handle = Some(ibex_process_monitor);

        Ok(Self {
            shutdown_tx: Some(shutdown_tx),
            process_handle,
            prom_shutdown_tx: Some(prom_shutdown_tx),
            prom_process_handle: Some(prom_process_handle),
            dropped: false,
        })
    }

    async fn terminate(&mut self) {
        if let Some(prom_shutdown_tx) = self.prom_shutdown_tx.take() {
            drop(prom_shutdown_tx);
        }
        if let Some(prom_process_handle) = self.prom_process_handle.take() {
            let _ = prom_process_handle.await;
        }
        if let Some(shutdown_tx) = self.shutdown_tx.take() {
            drop(shutdown_tx);
        }
        if let Some(process_handle) = self.process_handle.take() {
            let _ = process_handle.await;
        }
        info!("Ibex process terminated correctly");
    }
}

impl Drop for IbexProcess {
    fn drop(&mut self) {
        info!("Dropping IbexProcess started");
        if !self.dropped {
            let mut this = IbexProcess::default();
            std::mem::swap(&mut this, self);
            this.dropped = true;
            let task = tokio::spawn(async move { this.terminate().await });
            match futures::executor::block_on(task) {
                Ok(_) => {
                    info!("Dropping IbexProcess ended");
                }
                Err(e) => {
                    info!(
                        "Error dropping IbexProcess: {:?}, cleanup task finish with error",
                        e
                    );
                }
            }
        }
    }
}

async fn report_metrics(system: Arc<Mutex<System>>) -> BenchmarkResult<()> {
    if let Ok((num_nodes, num_edges)) = fetch_stats().await {
        IBEX_NODES_GAUGE.set(num_nodes as i64);
        IBEX_RELATIONSHIPS_GAUGE.set(num_edges as i64);
    }

    fill_memory_and_cpu_metrics(system).await?;

    Ok(())
}

async fn fill_memory_and_cpu_metrics(sys: Arc<Mutex<System>>) -> BenchmarkResult<()> {
    let mut system = sys.lock().map_err(|_| OtherError("system mutex poisoned".to_string()))?;
    // Refresh CPU usage
    system.refresh_all();
    let logical_cpus = system.cpus().len();
    let cpu_usage = system.global_cpu_usage() as i64 / logical_cpus as i64;
    CPU_USAGE_GAUGE.set(cpu_usage);

    // Refresh memory usage
    let mem_used = system.used_memory();
    MEM_USAGE_GAUGE.set(mem_used as i64);

    // Find the specific process
    if let Some(pid) = get_ibex_server_pid(&system) {
        if let Some(process) = system.process(Pid::from(pid as usize)) {
            let cpu_usage = process.cpu_usage() as i64 / logical_cpus as i64;
            IBEX_CPU_USAGE_GAUGE.set(cpu_usage);
            let mem_used = process.memory() as i64;
            IBEX_MEM_USAGE_GAUGE.set(mem_used);
        }
    }

    Ok(())
}

fn get_ibex_server_pid(system: &System) -> Option<u32> {
    let res = system.processes().iter().find(|(_, process)| {
        if let Some(name_str) = process.name().to_str() {
            name_str == "ibexdb"
        } else {
            false
        }
    });
    res.map(|(pid, _)| pid.as_u32())
}
