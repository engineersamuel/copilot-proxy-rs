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
    map_sse_lines_checked(stream, move |line| Ok::<_, std::io::Error>(mapper(line)))
}

pub(crate) fn map_sse_lines_checked<S, F>(
    stream: S,
    mapper: F,
) -> impl Stream<Item = Result<Bytes, std::io::Error>>
where
    S: Stream<Item = Result<Bytes, reqwest::Error>> + Send + 'static,
    F: Fn(&str) -> Result<Option<String>, std::io::Error> + Send + Sync + 'static,
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
                if line.is_empty() {
                    yield Ok(Bytes::from_static(b"\n"));
                    continue;
                }
                match mapper(&line) {
                    Ok(Some(normalized)) => yield Ok(Bytes::from(format!("{normalized}\n"))),
                    Ok(None) => {}
                    Err(error) => {
                        yield Err(error);
                        return;
                    }
                }
            }
        }
        if !buf.is_empty() {
            let line = buf.trim_end_matches('\r').to_string();
            match mapper(&line) {
                Ok(Some(normalized)) => yield Ok(Bytes::from(format!("{normalized}\n"))),
                Ok(None) => {}
                Err(error) => yield Err(error),
            }
        }
    }
}

pub(crate) fn map_sse_lines_many<S, F>(
    stream: S,
    mut mapper: F,
) -> impl Stream<Item = Result<Bytes, std::io::Error>>
where
    S: Stream<Item = Result<Bytes, reqwest::Error>> + Send + 'static,
    F: FnMut(&str) -> Vec<String> + Send + 'static,
{
    let mut src = Box::pin(stream);
    async_stream::stream! {
        let mut buf = Vec::new();
        while let Some(chunk_result) = src.next().await {
            match chunk_result {
                Err(error) => {
                    yield Err(std::io::Error::other(error));
                    return;
                }
                Ok(chunk) => buf.extend_from_slice(&chunk),
            }
            while let Some(newline) = buf.iter().position(|byte| *byte == b'\n') {
                let line = String::from_utf8_lossy(&buf[..newline])
                    .trim_end_matches('\r')
                    .to_string();
                buf.drain(..=newline);
                for event in mapper(&line) {
                    yield Ok(Bytes::from(format!("{event}\n\n")));
                }
            }
        }
        if !buf.is_empty() {
            let line = String::from_utf8_lossy(&buf)
                .trim_end_matches('\r')
                .to_string();
            for event in mapper(&line) {
                yield Ok(Bytes::from(format!("{event}\n\n")));
            }
        }
    }
}
