use ion::manager::AgentManager;
use ion::types::{PoolOptions, TaskConfig, TaskPayload};
use ion::worker::stub::StubWorker;
/// Demo binary: submits several tasks, streams events to stdout, and prints
/// final summary.
///
/// Usage:
///   cargo run --bin demo
use std::time::Duration;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    tracing::info!("ION Demo — Agent Manager Orchestration");

    // Build the manager with a pool of stub workers
    let mgr = AgentManager::new(
        PoolOptions {
            min_workers: 0,
            max_workers: 4,
            idle_timeout: Duration::from_secs(60),
            worker_timeout: Duration::from_secs(300),
        },
        TaskConfig { max_retries: 2 },
        |_id| Box::new(StubWorker::new("demo").with_delay(30)),
    );

    // Subscribe to events
    let mut rx = mgr.handle.subscribe().await?;

    // Spawn an event printer
    let event_printer = tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(event) => tracing::info!("{event}"),
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!("skipped {n} events");
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    });

    // Submit tasks
    let tasks = vec![
        "Analyse the codebase architecture",
        "Write unit tests for the module",
        "Refactor the authentication flow",
        "Document the API endpoints",
        "Fix the memory leak in worker",
        "Optimise the database queries",
        "Add metrics collection",
        "Update the CI pipeline",
    ];

    tracing::info!("Submitting {} tasks…", tasks.len());

    let mut task_ids = Vec::new();
    for desc in &tasks {
        let id = mgr
            .handle
            .submit(TaskPayload::Prompt(desc.to_string()))
            .await?;
        tracing::info!("Submitted task {id}: {desc}");
        task_ids.push(id);
    }

    tracing::info!("All submitted, waiting for completion…");

    // Poll for completion
    for id in &task_ids {
        loop {
            let snap = mgr.handle.status(*id).await?;
            match snap {
                Some(s) if s.status.is_terminal() => {
                    tracing::info!(
                        "Task {} finished — status={:?} retries={} error={:?}",
                        id,
                        s.status,
                        s.retry_count,
                        s.error,
                    );
                    break;
                }
                _ => {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
            }
        }
    }

    // Print final stats
    let pool_stats = mgr.handle.pool_stats().await?;
    let queue_stats = mgr.handle.queue_stats().await?;
    tracing::info!("=== Final Stats ===");
    tracing::info!("Pool:  {pool_stats:?}");
    tracing::info!("Queue: {queue_stats:?}");

    // Shutdown
    mgr.handle.shutdown().await;
    let _ = event_printer.await;

    tracing::info!("Demo complete!");
    Ok(())
}
