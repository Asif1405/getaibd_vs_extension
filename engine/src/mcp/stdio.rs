use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use super::handler::McpHandler;
use super::protocol::JsonRpcRequest;

const MAX_REQUEST_SIZE: usize = 10 * 1024 * 1024; // 10 MB

pub async fn run_stdio(handler: McpHandler) -> Result<(), Box<dyn std::error::Error>> {
    let stdin = tokio::io::stdin();
    let mut stdout = tokio::io::stdout();
    let reader = BufReader::new(stdin);
    let mut lines = reader.lines();

    while let Some(line) = lines.next_line().await? {
        let line = line.trim().to_string();
        if line.is_empty() {
            continue;
        }

        if line.len() > MAX_REQUEST_SIZE {
            let err = super::protocol::JsonRpcResponse::error(
                None,
                -32600,
                format!(
                    "Request too large: {} bytes (max {})",
                    line.len(),
                    MAX_REQUEST_SIZE
                ),
            );
            let out = serde_json::to_string(&err)?;
            stdout.write_all(out.as_bytes()).await?;
            stdout.write_all(b"\n").await?;
            stdout.flush().await?;
            continue;
        }

        let request: JsonRpcRequest = match serde_json::from_str(&line) {
            Ok(r) => r,
            Err(e) => {
                let err = super::protocol::JsonRpcResponse::error(
                    None,
                    -32700,
                    format!("Parse error: {e}"),
                );
                let out = serde_json::to_string(&err)?;
                stdout.write_all(out.as_bytes()).await?;
                stdout.write_all(b"\n").await?;
                stdout.flush().await?;
                continue;
            }
        };

        if let Some(response) = handler.handle(request).await {
            let out = serde_json::to_string(&response)?;
            stdout.write_all(out.as_bytes()).await?;
            stdout.write_all(b"\n").await?;
            stdout.flush().await?;
        }
    }

    Ok(())
}
