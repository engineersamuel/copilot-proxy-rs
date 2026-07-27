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
    max_line_bytes: usize,
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
            let chunk = match chunk_result {
                Err(error) => {
                    yield Err(std::io::Error::other(error));
                    return;
                }
                Ok(chunk) => chunk,
            };
            let mut remaining = chunk.as_ref();
            while let Some(newline) = remaining.iter().position(|byte| *byte == b'\n') {
                if buf
                    .len()
                    .checked_add(newline)
                    .is_none_or(|line_len| line_len > max_line_bytes)
                {
                    yield Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "SSE line exceeds configured limit",
                    ));
                    return;
                }
                buf.extend_from_slice(&remaining[..newline]);
                let line = String::from_utf8_lossy(&buf)
                    .trim_end_matches('\r')
                    .to_string();
                buf.clear();
                remaining = &remaining[newline + 1..];
                for event in mapper(&line) {
                    yield Ok(Bytes::from(format!("{event}\n\n")));
                }
            }
            if buf
                .len()
                .checked_add(remaining.len())
                .is_none_or(|line_len| line_len > max_line_bytes)
            {
                yield Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "SSE line exceeds configured limit",
                ));
                return;
            }
            buf.extend_from_slice(remaining);
        }
        if !buf.is_empty() {
            if buf.len() > max_line_bytes {
                yield Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "SSE line exceeds configured limit",
                ));
                return;
            }
            let line = String::from_utf8_lossy(&buf)
                .trim_end_matches('\r')
                .to_string();
            for event in mapper(&line) {
                yield Ok(Bytes::from(format!("{event}\n\n")));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;
    use futures_util::StreamExt;

    use super::map_sse_lines_many;

    #[tokio::test]
    async fn map_sse_lines_many_rejects_oversized_unterminated_line() {
        let source = futures_util::stream::iter(vec![Ok::<Bytes, reqwest::Error>(
            Bytes::from_static(b"data: 123456789"),
        )]);
        let mapped = map_sse_lines_many(source, 8, |_| Vec::new());
        futures_util::pin_mut!(mapped);

        let error = mapped.next().await.unwrap().unwrap_err();

        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    }
}
