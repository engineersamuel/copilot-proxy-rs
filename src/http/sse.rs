use bytes::Bytes;
use futures_util::{Stream, StreamExt};

pub(crate) fn map_sse_lines<S, F>(
    stream: S,
    mapper: F,
) -> impl Stream<Item = Result<Bytes, std::io::Error>>
where
    S: Stream<Item = Result<Bytes, reqwest::Error>> + Send + 'static,
    F: Fn(&str) -> Option<String> + Send + Sync + 'static,
{
    let mut src = Box::pin(stream);
    async_stream::stream! {
        let mut buf = String::new();
        while let Some(chunk_result) = src.next().await {
            match chunk_result {
                Err(e) => {
                    yield Err(std::io::Error::other(e));
                    return;
                }
                Ok(chunk) => buf.push_str(&String::from_utf8_lossy(&chunk)),
            }
            while let Some(nl) = buf.find('\n') {
                let line = buf[..nl].trim_end_matches('\r').to_string();
                buf.drain(..=nl);
                if let Some(normalized) = mapper(&line) {
                    yield Ok(Bytes::from(format!("{normalized}\n")));
                }
            }
        }
        if !buf.is_empty() {
            let line = buf.trim_end_matches('\r').to_string();
            if let Some(normalized) = mapper(&line) {
                yield Ok(Bytes::from(format!("{normalized}\n")));
            }
        }
    }
}
