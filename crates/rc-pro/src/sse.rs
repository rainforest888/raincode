//! Minimal SSE / JSON-line parser over a reqwest byte stream.
use crate::provider::ProviderError;
use bytes::Bytes;
use futures::Stream;
use futures::StreamExt;
use std::pin::Pin;

type ByteStream = Pin<Box<dyn Stream<Item = Result<Bytes, reqwest::Error>> + Send>>;
type SseStream = Pin<Box<dyn Stream<Item = Result<String, ProviderError>> + Send>>;

fn box_bytes(
    stream: impl Stream<Item = Result<Bytes, reqwest::Error>> + Send + 'static,
) -> ByteStream {
    Box::pin(stream)
}

pub(crate) fn response_bytes(resp: reqwest::Response) -> ByteStream {
    box_bytes(resp.bytes_stream())
}

/// Parse SSE frames (`data: ...` lines, `[DONE]` terminator).
pub fn sse_events(stream: ByteStream) -> SseStream {
    let unfolded =
        futures::stream::unfold((stream, Vec::<u8>::new()), |(mut s, mut buf)| async move {
            loop {
                if let Some(pos) = buf.iter().position(|&b| b == b'\n') {
                    let line: Vec<u8> = buf.drain(..=pos).collect();
                    let text = String::from_utf8_lossy(&line);
                    let text = text.trim_end_matches(['\n', '\r']);
                    if let Some(data) = text.strip_prefix("data:") {
                        let data = data.trim().to_string();
                        if data == "[DONE]" {
                            return None;
                        }
                        return Some((Ok(data), (s, buf)));
                    }
                    continue;
                }
                // 每块 120s 空闲超时兜底:长流式生成(连续块)不受影响,挂起连接报错。
                match tokio::time::timeout(std::time::Duration::from_secs(120), s.next()).await {
                    Ok(Some(Ok(chunk))) => {
                        buf.extend_from_slice(&chunk);
                        // 某些实现最后一行没有结尾换行:EOF 时若缓冲区还有未消费的
                        // data 行,也把它当一行收尾,否则最后的 Finish/工具事件被丢。
                        continue;
                    }
                    Ok(Some(Err(e))) => {
                        return Some((Err(ProviderError::Transport(e.to_string())), (s, buf)))
                    }
                    Ok(None) => {
                        let tail = flush_tail(&buf);
                        if let Some(out) = tail {
                            buf.clear();
                            return Some((Ok(out), (s, buf)));
                        }
                        return None;
                    }
                    Err(_) => {
                        return Some((
                            Err(ProviderError::Transport("SSE idle timeout".into())),
                            (s, buf),
                        ))
                    }
                }
            }
        });
    Box::pin(unfolded)
}

/// SSE 流末尾不带换行的最后一行 data:xxx,在 EOF 时按一行收尾。
fn flush_tail(buf: &[u8]) -> Option<String> {
    if buf.is_empty() {
        return None;
    }
    let text = String::from_utf8_lossy(buf);
    let text = text.trim_end_matches(['\n', '\r']);
    let data = text.strip_prefix("data:")?.trim().to_string();
    if data == "[DONE]" { None } else { Some(data) }
}

/// Some local endpoints stream JSON objects line-by-line without SSE framing.
pub fn json_lines(stream: ByteStream) -> SseStream {
    let unfolded =
        futures::stream::unfold((stream, Vec::<u8>::new()), |(mut s, mut buf)| async move {
            loop {
                if let Some(pos) = buf.iter().position(|&b| b == b'\n') {
                    let line: Vec<u8> = buf.drain(..=pos).collect();
                    let text = String::from_utf8_lossy(&line);
                    let text = text.trim();
                    if text.is_empty() {
                        continue;
                    }
                    return Some((Ok(text.to_string()), (s, buf)));
                }
                // 每块 120s 空闲超时兜底:长流式生成(连续块)不受影响,挂起连接报错。
                match tokio::time::timeout(std::time::Duration::from_secs(120), s.next()).await {
                    Ok(Some(Ok(chunk))) => {
                        buf.extend_from_slice(&chunk);
                        continue;
                    }
                    Ok(Some(Err(e))) => {
                        return Some((Err(ProviderError::Transport(e.to_string())), (s, buf)))
                    }
                    Ok(None) => {
                        // EOF 时缓冲区里可能还有最后一行(无结尾换行),按一行收尾,
                        // 否则最后的 Finish/工具事件被丢,agent 等不到流结束。
                        let text = String::from_utf8_lossy(&buf);
                        let text = text.trim();
                        if text.is_empty() {
                            return None;
                        }
                        let out = text.to_string();
                        buf.clear();
                        return Some((Ok(out), (s, buf)));
                    }
                    Err(_) => {
                        return Some((
                            Err(ProviderError::Transport("SSE idle timeout".into())),
                            (s, buf),
                        ))
                    }
                }
            }
        });
    Box::pin(unfolded)
}

/// Feed lines into a parser without a live HTTP response (tests).
#[cfg(test)]
pub(crate) fn parse_lines_from_bytes(chunks: Vec<Vec<u8>>, framed: bool) -> Vec<String> {
    let stream = box_bytes(futures::stream::iter(
        chunks
            .into_iter()
            .map(|c| Ok::<Bytes, reqwest::Error>(Bytes::from(c))),
    ));
    let mut s = if framed {
        sse_events(stream)
    } else {
        json_lines(stream)
    };
    // sse_events/json_lines 用 tokio::time::timeout,必须在 Tokio 运行时内驱动。
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async move {
            let mut out = Vec::new();
            while let Some(Ok(line)) = s.next().await {
                out.push(line);
            }
            out
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sse_frames() {
        let out = parse_lines_from_bytes(vec![b"data: hello\ndata: [DONE]\n".to_vec()], true);
        assert_eq!(out, vec!["hello"]);
    }

    #[test]
    fn json_lines_plain() {
        let out = parse_lines_from_bytes(vec![b"{\"a\":1}\n{\"b\":2}\n".to_vec()], false);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0], "{\"a\":1}");
    }

    #[test]
    fn final_line_without_newline_is_flushed() {
        // 末尾不带换行的最后一行:EOF 时必须收尾,否则 Finish/工具事件被丢。
        let out = parse_lines_from_bytes(
            vec![b"data: hello\ndata: world".to_vec()],
            true,
        );
        assert_eq!(out, vec!["hello", "world"]);
        let out2 = parse_lines_from_bytes(vec![b"{\"x\":1}".to_vec()], false);
        assert_eq!(out2, vec!["{\"x\":1}"]);
    }
}
