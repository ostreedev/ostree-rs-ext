use anyhow::Result;
use futures::{Future, FutureExt, Stream, StreamExt};
use std::io::prelude::*;
use tokio::io::AsyncRead;

pub(crate) struct TarEntry {
    pub(crate) header: tar::Header,
    pub(crate) content: Option<tokio::sync::mpsc::Receiver<bytes::Bytes>>,
}

#[must_use]
pub(crate) fn parse_tar<S: AsyncRead + Unpin + Send + 'static>(
    s: S,
) -> Result<(
    impl Stream<Item = Result<TarEntry>>,
    impl Future<Output = Result<()>>,
)> {
    let (tx_tar, rx_tar) = tokio::sync::mpsc::channel(2);
    let (pipein, mut pipeout) = os_pipe::pipe()?;

    // Bridge from the async input to a synchronous pipe
    let copier = tokio::spawn(async move {
        let mut input = tokio_util::io::ReaderStream::new(s).boxed();
        while let Some(buf) = input.next().await {
            let buf = buf?;
            // TODO blocking executor
            pipeout.write_all(&buf)?;
        }
        Ok::<_, anyhow::Error>(())
    })
    .map(|e| e.unwrap())
    .boxed_local();

    // Thread which reads from the pipe (synchronously) and parses the tar archive
    let processor = tokio::task::spawn_blocking(move || {
        let mut archive = tar::Archive::new(pipein);
        let mut buf = vec![0u8; 4096];
        for entry in archive.entries()? {
            let mut entry = entry?;
            let (tx_content, rx_content) = if entry.header().entry_type() == tar::EntryType::Regular
            {
                let (s, r) = tokio::sync::mpsc::channel::<bytes::Bytes>(1);
                (Some(s), Some(r))
            } else {
                (None, None)
            };
            if tx_tar
                .blocking_send(Ok::<_, anyhow::Error>(TarEntry {
                    header: entry.header().clone(),
                    content: rx_content,
                }))
                .is_err()
            {
                // If the receiver closes the channel, we're done
                break;
            };
            if let Some(tx_content) = tx_content {
                loop {
                    let n = entry.read(&mut buf[..])?;
                    if 0 == n {
                        break;
                    } else {
                        if tx_content
                            .blocking_send(bytes::Bytes::copy_from_slice(&buf[0..n]))
                            .is_err()
                        {
                            // Receiver closing means they aren't interested in the content
                            break;
                        }
                    }
                }
            }
        }
        Ok::<_, anyhow::Error>(())
    })
    .boxed_local();

    let worker = async move {
        let (a, b) = tokio::join!(copier, processor);
        a?;
        b??;
        Ok::<_, anyhow::Error>(())
    }
    .boxed_local();

    Ok((tokio_stream::wrappers::ReceiverStream::new(rx_tar), worker))
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::stream::TryStreamExt;

    const EXAMPLEOS_TAR: &[u8] = include_bytes!("tests/it/fixtures/exampleos.tar.zst");

    #[tokio::test]
    async fn test_import() -> Result<()> {
        let uncomp = async_compression::tokio::bufread::ZstdDecoder::new(EXAMPLEOS_TAR);
        let (s, worker) = parse_tar(uncomp)?;
        let n = s
            .try_fold(0usize, |acc, f| async move {
                let n = if let Some(s) = f.content.map(tokio_stream::wrappers::ReceiverStream::new)
                {
                    s.fold(acc, |acc, buf| async move { acc + buf.len() }).await
                } else {
                    acc
                };
                Ok::<_, anyhow::Error>(n)
            })
            .await?;
        worker.await?;
        assert_eq!(n, 35usize);
        Ok(())
    }
}
