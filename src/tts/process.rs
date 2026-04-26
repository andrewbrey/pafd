use crate::tts::Cancellation;
use crate::tts::Result;
use crate::tts::TtsError;
use bytes::Bytes;
use futures_util::Stream;
use futures_util::StreamExt;
use std::process::Stdio;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tokio_util::io::ReaderStream;

/// Runs `command` to completion and returns its captured output.
///
/// # Errors
///
/// Returns an error when the process fails to spawn, exits non-zero, or
/// `cancellation` fires before completion.
///
/// # Panics
///
/// Panics if the spawned child's stdin handle is unexpectedly missing after
/// being configured as piped.
pub async fn output(
    command: &str,
    args: &[String],
    input: Option<&[u8]>,
    env: &[(&str, &str)],
    cancellation: &Cancellation,
) -> Result<std::process::Output> {
    let mut child = Command::new(command)
        .args(args)
        .envs(env.iter().copied())
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;

    let work = async {
        if let Some(input) = input {
            let mut stdin = child.stdin.take().expect("stdin piped above");
            stdin.write_all(input).await?;
            stdin.shutdown().await?;
            drop(stdin);
        }

        let output = child.wait_with_output().await?;
        if output.status.success() {
            return Ok(output);
        }

        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        Err(TtsError::CommandFailed {
            command: command.to_owned(),
            status: output.status,
            stderr,
        })
    };

    cancellation.run(work).await
}

/// Runs `command` to completion and discards its output.
///
/// # Errors
///
/// Returns an error when the process fails to spawn, exits non-zero, or
/// `cancellation` fires before completion.
pub async fn status(command: &str, args: &[String], cancellation: &Cancellation) -> Result<()> {
    output(command, args, None, &[], cancellation).await?;
    Ok(())
}

/// Spawns `command` and returns a stream of stdout chunks. The child is killed
/// on drop. If the process exits non-zero, the stream's final item is the
/// `CommandFailed` error containing collected stderr.
///
/// # Errors
///
/// Returns an error if the process fails to spawn.
///
/// # Panics
///
/// Panics if the spawned child's stdio handles are unexpectedly missing after
/// being configured as piped.
#[expect(clippy::needless_pass_by_value, reason = "owned to keep 'static")]
pub fn stream_output(
    command: String,
    args: Vec<String>,
    input: Option<Vec<u8>>,
    env: Vec<(String, String)>,
    cancellation: Cancellation,
) -> Result<impl Stream<Item = Result<Bytes>> + Send + 'static> {
    let mut child = Command::new(&command)
        .args(&args)
        .envs(env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;

    if let Some(input) = input {
        let mut stdin = child.stdin.take().expect("stdin piped above");
        tokio::spawn(async move {
            let _ = stdin.write_all(&input).await;
            let _ = stdin.shutdown().await;
        });
    }

    let stdout = child.stdout.take().expect("stdout piped above");
    let mut stderr = child.stderr.take().expect("stderr piped above");
    let stderr_handle = tokio::spawn(async move {
        let mut buf = Vec::new();
        let _ = stderr.read_to_end(&mut buf).await;
        buf
    });

    Ok(async_stream::try_stream! {
        let mut reader = ReaderStream::new(stdout);
        loop {
            let next = cancellation
                .run(async { reader.next().await.transpose().map_err(TtsError::from) })
                .await?;
            match next {
                Some(chunk) if !chunk.is_empty() => yield chunk,
                Some(_) => {}
                None => break,
            }
        }
        let status = child.wait().await?;
        if !status.success() {
            let stderr = stderr_handle.await.unwrap_or_default();
            let stderr = String::from_utf8_lossy(&stderr).trim().to_owned();
            Err(TtsError::CommandFailed { command, status, stderr })?;
        }
    })
}
