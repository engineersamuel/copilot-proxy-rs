use bytes::Bytes;
use futures_util::{Stream, StreamExt};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SseStreamEnd {
    Eof,
    TransportError,
}

pub(crate) fn inspect_sse_lines<S, F, G>(
    stream: S,
    max_line_bytes: usize,
    mut observer: F,
    mut on_end: G,
) -> impl Stream<Item = Result<Bytes, std::io::Error>>
where
    S: Stream<Item = Result<Bytes, reqwest::Error>> + Send + 'static,
    F: FnMut(&str) + Send + 'static,
    G: FnMut(SseStreamEnd) + Send + 'static,
{
    let mut src = Box::pin(stream);
    async_stream::stream! {
        let mut line = Vec::new();
        let mut observing = true;
        while let Some(chunk_result) = src.next().await {
            let chunk = match chunk_result {
                Ok(chunk) => chunk,
                Err(error) => {
                    on_end(SseStreamEnd::TransportError);
                    yield Err(std::io::Error::other(error));
                    return;
                }
            };
            for byte in &chunk {
                if *byte == b'\n' {
                    if observing {
                        if line.last() == Some(&b'\r') {
                            line.pop();
                        }
                        observer(&String::from_utf8_lossy(&line));
                    }
                    line.clear();
                    observing = true;
                } else if observing {
                    if line.len() < max_line_bytes {
                        line.push(*byte);
                    } else {
                        line.clear();
                        observing = false;
                    }
                }
            }
            yield Ok(chunk);
        }
        if observing && !line.is_empty() {
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            observer(&String::from_utf8_lossy(&line));
        }
        on_end(SseStreamEnd::Eof);
    }
}

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
    map_sse_lines_many_checked(stream, max_line_bytes, move |line| {
        Ok::<_, std::io::Error>(mapper(line))
    })
}

pub(crate) fn map_sse_lines_many_checked<S, F>(
    stream: S,
    max_line_bytes: usize,
    mut mapper: F,
) -> impl Stream<Item = Result<Bytes, std::io::Error>>
where
    S: Stream<Item = Result<Bytes, reqwest::Error>> + Send + 'static,
    F: FnMut(&str) -> Result<Vec<String>, std::io::Error> + Send + 'static,
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
                match mapper(&line) {
                    Ok(events) => {
                        for event in events {
                            yield Ok(Bytes::from(format!("{event}\n\n")));
                        }
                    }
                    Err(error) => {
                        yield Err(error);
                        return;
                    }
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
            match mapper(&line) {
                Ok(events) => {
                    for event in events {
                        yield Ok(Bytes::from(format!("{event}\n\n")));
                    }
                }
                Err(error) => yield Err(error),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;
    use futures_util::StreamExt;

    use super::{SseStreamEnd, inspect_sse_lines, map_sse_lines_many};

    #[tokio::test]
    async fn inspect_sse_lines_observes_split_lines_without_changing_bytes() {
        let source = futures_util::stream::iter(vec![
            Ok::<Bytes, reqwest::Error>(Bytes::from_static(b"data: fir")),
            Ok::<Bytes, reqwest::Error>(Bytes::from_static(b"st\r\ndata: second\n\n")),
        ]);
        let observed = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let end = std::sync::Arc::new(std::sync::Mutex::new(None));
        let observer_output = observed.clone();
        let end_output = end.clone();
        let inspected = inspect_sse_lines(
            source,
            1024,
            move |line| {
                observer_output.lock().unwrap().push(line.to_string());
            },
            move |stream_end| {
                *end_output.lock().unwrap() = Some(stream_end);
            },
        );
        futures_util::pin_mut!(inspected);
        let mut output = Vec::new();
        while let Some(chunk) = inspected.next().await {
            output.extend_from_slice(&chunk.unwrap());
        }

        assert_eq!(output, b"data: first\r\ndata: second\n\n");
        assert_eq!(
            *observed.lock().unwrap(),
            vec!["data: first", "data: second", ""]
        );
        assert_eq!(*end.lock().unwrap(), Some(SseStreamEnd::Eof));
    }

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
