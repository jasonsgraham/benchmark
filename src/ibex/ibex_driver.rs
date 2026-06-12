use crate::error::BenchmarkError::OtherError;
use crate::error::BenchmarkResult;
use crate::ibex::ibex_process::{fetch_stats, ibex_endpoint, IbexProcess, IBEX_DATA_DIR};
use crate::queries_repository::PreparedQuery;
use crate::scenario::Size;
use crate::scheduler::Msg;
use crate::utils::{create_directory_if_not_exists, file_exists, spawn_command};
use crate::{IBEX_MSG_DEADLINE_OFFSET_GAUGE, OPERATION_COUNTER, OPERATION_ERROR_COUNTER};
use ibexdb_types::QueryResult;
use serde::{Deserialize, Serialize};
use std::hint::black_box;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::error::Elapsed;
use tokio::time::sleep;
use tracing::{error, info};

const IBEX_DB_DIR: &str = "./ibex-data/db";

#[allow(dead_code)]
pub struct Started(IbexProcess);
pub struct Stopped;

pub struct Ibex<U> {
    database: String,
    #[allow(dead_code)]
    state: U,
}

impl Default for Ibex<Stopped> {
    fn default() -> Self {
        Self::new()
    }
}

impl Ibex<Stopped> {
    fn new() -> Ibex<Stopped> {
        Ibex {
            database: IBEX_DB_DIR.to_string(),
            state: Stopped,
        }
    }

    pub async fn start(self) -> BenchmarkResult<Ibex<Started>> {
        create_directory_if_not_exists(IBEX_DATA_DIR).await?;
        create_directory_if_not_exists(self.database.as_str()).await?;
        let ibex_process = IbexProcess::new(self.database.as_str()).await?;
        Self::wait_for_ready().await?;
        Ok(Ibex {
            database: self.database.clone(),
            state: Started(ibex_process),
        })
    }

    pub async fn clean_db(&self) -> BenchmarkResult<()> {
        info!("deleting: {}", self.database);
        if file_exists(self.database.as_str()).await {
            spawn_command("rm", &["-rf", self.database.as_str()]).await?;
        }
        create_directory_if_not_exists(self.database.as_str()).await?;
        Ok(())
    }

    pub async fn save_db(
        &self,
        size: Size,
    ) -> BenchmarkResult<()> {
        let target = saved_db_path(size);
        info!("saving ibex database {} to {}", self.database, target);
        if file_exists(target.as_str()).await {
            spawn_command("rm", &["-rf", target.as_str()]).await?;
        }
        spawn_command("cp", &["-r", self.database.as_str(), target.as_str()]).await?;
        Ok(())
    }
}

impl Ibex<Started> {
    pub async fn stop(self) -> BenchmarkResult<Ibex<Stopped>> {
        drop(self.state.0);
        Ok(Ibex {
            database: self.database.clone(),
            state: Stopped,
        })
    }

    pub async fn graph_size(&self) -> BenchmarkResult<(u64, u64)> {
        fetch_stats().await
    }
}

impl<U> Ibex<U> {
    pub async fn client(&self) -> BenchmarkResult<IbexBenchmarkClient> {
        Ok(IbexBenchmarkClient {
            client: IbexHttpClient::new(ibex_endpoint()),
        })
    }

    async fn wait_for_ready() -> BenchmarkResult<()> {
        const MAX_ATTEMPTS: u32 = 20;
        for attempt in 1..=MAX_ATTEMPTS {
            if fetch_stats().await.is_ok() {
                return Ok(());
            }
            if attempt < MAX_ATTEMPTS {
                sleep(Duration::from_millis(500)).await;
            }
        }
        Err(OtherError(format!(
            "ibexdb server not ready after {} attempts",
            MAX_ATTEMPTS
        )))
    }

    pub async fn restore_db(
        &self,
        size: Size,
    ) -> BenchmarkResult<()> {
        let source = saved_db_path(size);
        if file_exists(self.database.as_str()).await {
            spawn_command("rm", &["-rf", self.database.as_str()]).await?;
        }
        info!("copy {} to {}", source, self.database);
        if file_exists(source.as_str()).await {
            spawn_command("cp", &["-r", source.as_str(), self.database.as_str()]).await?;
        }
        Ok(())
    }

    pub async fn dump_exists_or_error(
        &self,
        size: Size,
    ) -> BenchmarkResult<()> {
        let path = saved_db_path(size);
        if !file_exists(path.as_str()).await {
            Err(OtherError(format!("Dump directory not found: {}", path)))
        } else {
            Ok(())
        }
    }
}

fn saved_db_path(size: Size) -> String {
    format!("{}/{}_db", IBEX_DATA_DIR, size.to_string().to_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_saved_db_path() {
        assert_eq!(saved_db_path(Size::Small), "./ibex-data/small_db");
        assert_eq!(saved_db_path(Size::Medium), "./ibex-data/medium_db");
        assert_eq!(saved_db_path(Size::Large), "./ibex-data/large_db");
    }

    #[test]
    fn query_response_maps_to_query_result() -> BenchmarkResult<()> {
        let response = QueryResponse {
            columns: vec!["name".to_string()],
            rows: vec![vec![serde_json::json!("Alice")]],
            execution_time_ms: 3,
            row_count: 1,
            error: None,
        };

        let result = response.into_query_result()?;

        assert_eq!(result.columns, vec!["name"]);
        assert_eq!(result.rows, vec![vec![serde_json::json!("Alice")]]);
        assert_eq!(result.execution_time_ms, 3);
        assert_eq!(result.row_count, 1);
        assert_eq!(result.rows_examined, 1);
        assert!(!result.cache_hit);
        Ok(())
    }

    #[test]
    fn query_response_error_becomes_benchmark_error() {
        let response = QueryResponse {
            columns: Vec::new(),
            rows: Vec::new(),
            execution_time_ms: 0,
            row_count: 0,
            error: Some("syntax error".to_string()),
        };

        let result = response.into_query_result();

        assert!(matches!(
            result,
            Err(OtherError(message)) if message.contains("syntax error")
        ));
    }
}

#[derive(Clone)]
pub struct IbexBenchmarkClient {
    client: IbexHttpClient,
}

#[derive(Clone)]
struct IbexHttpClient {
    client: reqwest::Client,
    endpoint: String,
}

impl IbexHttpClient {
    fn new(endpoint: String) -> Self {
        Self {
            client: reqwest::Client::new(),
            endpoint,
        }
    }

    async fn query_string(
        &self,
        query: &str,
    ) -> BenchmarkResult<QueryResult> {
        let url = format!("{}/api/query", self.endpoint);
        let response: QueryResponse = self
            .client
            .post(url)
            .json(&QueryRequest { query })
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;

        response.into_query_result()
    }
}

#[derive(Serialize)]
struct QueryRequest<'a> {
    query: &'a str,
}

#[derive(Deserialize)]
struct QueryResponse {
    columns: Vec<String>,
    rows: Vec<Vec<serde_json::Value>>,
    execution_time_ms: u64,
    row_count: usize,
    error: Option<String>,
}

impl QueryResponse {
    fn into_query_result(self) -> BenchmarkResult<QueryResult> {
        if let Some(error) = self.error {
            return Err(OtherError(format!("IbexDB query error: {}", error)));
        }

        Ok(QueryResult {
            execution_time_ms: self.execution_time_ms,
            rows_examined: self.row_count as u64,
            cache_hit: false,
            row_count: self.row_count,
            columns: self.columns,
            rows: self.rows,
        })
    }
}

impl IbexBenchmarkClient {
    pub async fn execute_queries(
        &mut self,
        spawn_id: usize,
        queries: Arc<Box<dyn Iterator<Item = PreparedQuery> + Send + Sync>>,
    ) {
        let spawn_id = spawn_id.to_string();
        match Arc::try_unwrap(queries) {
            Ok(queries) => {
                for PreparedQuery { q_name, cypher, .. } in queries {
                    let res = self
                        ._execute_query(spawn_id.as_str(), q_name.as_str(), cypher.as_str())
                        .await;
                    if let Err(e) = res {
                        error!("Error executing query: {}, the error is: {:?}", cypher, e);
                    }
                }
            }
            Err(arc) => {
                error!(
                    "Failed to unwrap queries iterator, Remaining references count: {}",
                    Arc::strong_count(&arc)
                );
            }
        }
    }

    pub async fn execute_prepared_query<S: AsRef<str>>(
        &mut self,
        worker_id: S,
        msg: &Msg<PreparedQuery>,
        simulate: &Option<usize>,
    ) -> BenchmarkResult<()> {
        let Msg {
            payload: PreparedQuery { q_name, cypher, .. },
            ..
        } = msg;

        let worker_id = worker_id.as_ref();
        let query = cypher.as_str();

        let offset = msg.compute_offset_ms();
        IBEX_MSG_DEADLINE_OFFSET_GAUGE.set(offset);
        if offset > 0 {
            tokio::time::sleep(Duration::from_millis(offset as u64)).await;
        }

        if let Some(delay) = simulate {
            if *delay > 0 {
                let delay: u64 = *delay as u64;
                tokio::time::sleep(Duration::from_millis(delay)).await;
            }
            return Ok(());
        }

        let timeout = Duration::from_secs(60);
        let ibex_result = tokio::time::timeout(timeout, self.client.query_string(query)).await;
        OPERATION_COUNTER
            .with_label_values(&["ibex", worker_id, "", q_name, "", ""])
            .inc();
        Self::read_reply(worker_id, q_name, query, ibex_result)
    }

    pub async fn _execute_query<'a>(
        &'a mut self,
        spawn_id: &'a str,
        query_name: &'a str,
        query: &'a str,
    ) -> BenchmarkResult<()> {
        OPERATION_COUNTER
            .with_label_values(&["ibex", spawn_id, "", query_name, "", ""])
            .inc();

        let timeout = Duration::from_secs(5);
        let ibex_result = tokio::time::timeout(timeout, self.client.query_string(query)).await;
        Self::read_reply(spawn_id, query_name, query, ibex_result)
    }

    fn read_reply(
        spawn_id: &str,
        query_name: &str,
        query: &str,
        reply: Result<BenchmarkResult<QueryResult>, Elapsed>,
    ) -> BenchmarkResult<()> {
        match reply {
            Ok(ibex_result) => match ibex_result {
                Ok(query_result) => {
                    for row in query_result.rows {
                        black_box(row);
                    }
                    Ok(())
                }
                Err(e) => {
                    let error_type = std::any::type_name_of_val(&e);
                    error!("Error executing query: {}, the error is: {:?}", query, e);
                    Err(OtherError(format!(
                        "Error (type {}) executing query: {}, the error is: {:?}",
                        error_type, query, e
                    )))
                }
            },
            Err(e) => {
                OPERATION_ERROR_COUNTER
                    .with_label_values(&["ibex", spawn_id, "", query_name, "", ""])
                    .inc();
                let error_type = std::any::type_name_of_val(&e);
                error!("Error executing query: {}, the error is: {:?}", query, e);
                Err(OtherError(format!(
                    "Error (type {}) executing query: {}, the error is: {:?}",
                    error_type, query, e
                )))
            }
        }
    }
}
