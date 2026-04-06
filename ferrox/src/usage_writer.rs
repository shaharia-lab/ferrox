use std::time::Duration;

use tokio::sync::mpsc;
use uuid::Uuid;

/// A single usage record to be persisted.
#[derive(Debug, Clone)]
pub struct UsageEvent {
    pub client_id: Option<Uuid>,
    pub request_id: String,
    pub model: String,
    pub provider: String,
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub latency_ms: Option<u64>,
}

/// Handle for sending usage events from request handlers.
///
/// Cheap to clone — each handler gets its own sender.
#[derive(Clone)]
pub struct UsageWriter {
    tx: mpsc::Sender<UsageEvent>,
}

impl UsageWriter {
    /// Record a usage event.  Non-blocking — drops the event if the buffer is full.
    pub fn record(&self, event: UsageEvent) {
        if self.tx.try_send(event).is_err() {
            tracing::warn!("usage writer buffer full, dropping event");
        }
    }
}

/// A no-op writer that silently discards all events.
/// Used when no `usage_database_url` is configured.
pub fn noop_writer() -> UsageWriter {
    let (tx, mut rx) = mpsc::channel(1);
    // Spawn a tiny drainer so the channel doesn't back-pressure.
    tokio::spawn(async move { while rx.recv().await.is_some() {} });
    UsageWriter { tx }
}

/// Spawn the background flush task and return a `UsageWriter` handle.
///
/// The task collects events into a buffer and flushes to Postgres every
/// `flush_interval` or when the buffer reaches `batch_size`, whichever
/// comes first.
pub fn spawn_writer(
    pool: sqlx::PgPool,
    batch_size: usize,
    flush_interval: Duration,
    buffer_capacity: usize,
) -> UsageWriter {
    let (tx, rx) = mpsc::channel(buffer_capacity);

    tokio::spawn(flush_loop(pool, rx, batch_size, flush_interval));

    UsageWriter { tx }
}

async fn flush_loop(
    pool: sqlx::PgPool,
    mut rx: mpsc::Receiver<UsageEvent>,
    batch_size: usize,
    flush_interval: Duration,
) {
    let mut buffer: Vec<UsageEvent> = Vec::with_capacity(batch_size);
    let mut interval = tokio::time::interval(flush_interval);
    interval.tick().await; // skip the immediate first tick

    loop {
        tokio::select! {
            event = rx.recv() => {
                match event {
                    Some(e) => {
                        buffer.push(e);
                        if buffer.len() >= batch_size {
                            flush(&pool, &mut buffer).await;
                        }
                    }
                    None => {
                        // Channel closed — flush remaining and exit.
                        if !buffer.is_empty() {
                            flush(&pool, &mut buffer).await;
                        }
                        tracing::info!("usage writer shutting down");
                        return;
                    }
                }
            }
            _ = interval.tick() => {
                if !buffer.is_empty() {
                    flush(&pool, &mut buffer).await;
                }
            }
        }
    }
}

async fn flush(pool: &sqlx::PgPool, buffer: &mut Vec<UsageEvent>) {
    let events: Vec<UsageEvent> = std::mem::take(buffer);
    let count = events.len();

    // Filter out events without a client_id (e.g., unauthenticated requests).
    let valid: Vec<&UsageEvent> = events.iter().filter(|e| e.client_id.is_some()).collect();
    if valid.is_empty() {
        return;
    }

    let client_ids: Vec<Uuid> = valid.iter().map(|e| e.client_id.unwrap()).collect();
    let request_ids: Vec<&str> = valid.iter().map(|e| e.request_id.as_str()).collect();
    let models: Vec<&str> = valid.iter().map(|e| e.model.as_str()).collect();
    let providers: Vec<&str> = valid.iter().map(|e| e.provider.as_str()).collect();
    let prompt_tokens: Vec<i32> = valid.iter().map(|e| e.prompt_tokens as i32).collect();
    let completion_tokens: Vec<i32> = valid.iter().map(|e| e.completion_tokens as i32).collect();
    let total_tokens: Vec<i32> = valid
        .iter()
        .map(|e| (e.prompt_tokens + e.completion_tokens) as i32)
        .collect();
    let latency_ms: Vec<Option<i32>> = valid
        .iter()
        .map(|e| e.latency_ms.map(|l| l as i32))
        .collect();

    let result = sqlx::query(
        r#"
        INSERT INTO usage_log
            (client_id, request_id, model, provider, prompt_tokens, completion_tokens, total_tokens, latency_ms)
        SELECT * FROM UNNEST(
            $1::uuid[], $2::text[], $3::text[], $4::text[],
            $5::int[], $6::int[], $7::int[], $8::int[]
        )
        "#,
    )
    .bind(&client_ids)
    .bind(&request_ids)
    .bind(&models)
    .bind(&providers)
    .bind(&prompt_tokens)
    .bind(&completion_tokens)
    .bind(&total_tokens)
    .bind(&latency_ms)
    .execute(pool)
    .await;

    match result {
        Ok(_) => {
            tracing::debug!(count = count, "flushed usage records to database");
        }
        Err(e) => {
            tracing::error!(error = %e, count = count, "failed to flush usage records");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn noop_writer_does_not_panic() {
        let writer = noop_writer();
        writer.record(UsageEvent {
            client_id: Some(Uuid::new_v4()),
            request_id: "test".to_string(),
            model: "gpt-4".to_string(),
            provider: "openai".to_string(),
            prompt_tokens: 100,
            completion_tokens: 50,
            latency_ms: Some(200),
        });
        // Give the drainer a moment to consume.
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}
