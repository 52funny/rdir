use futures::future::{self, Either};
use futures::{ready, stream, FutureExt, Stream, StreamExt};
use headers::{
    HeaderMap, HeaderMapExt, IfModifiedSince, IfRange, IfUnmodifiedSince, LastModified, Range,
};
use std::cmp;
use std::convert::Infallible;
use std::fs::Metadata;
use std::path::PathBuf;
use std::pin::Pin;
use std::str::FromStr;
use std::task::Poll;
use tokio::io::AsyncSeekExt;
use tokio_util::io::poll_read_buf;
use warp::hyper::{Body, StatusCode};
use warp::reply::Response;
use warp::Reply;
use warp::{path::FullPath, reject, Filter, Rejection};

type TkFile = tokio::fs::File;

#[derive(Debug)]
pub struct Conditionals {
    if_modified_since: Option<IfModifiedSince>,
    if_unmodified_since: Option<IfUnmodifiedSince>,
    if_range: Option<IfRange>,
    range: Option<Range>,
}
struct BadRange;

enum Cond {
    NoBody(Response),
    WithBody(Option<Range>),
}

impl Conditionals {
    fn check(self, last_modified: Option<LastModified>) -> Cond {
        if let Some(since) = self.if_unmodified_since {
            let precondition = last_modified
                .map(|time| since.precondition_passes(time.into()))
                .unwrap_or(false);
            tracing::trace!(
                "if-unmodified-since: {:?} vs {:?} =  {}",
                since,
                last_modified,
                precondition
            );
            if !precondition {
                let mut resp = Response::new(Body::empty());
                *resp.status_mut() = StatusCode::PRECONDITION_FAILED;
                return Cond::NoBody(resp);
            }
        }

        if let Some(since) = self.if_modified_since {
            let unmodified = last_modified
                .map(|time| !since.is_modified(time.into()))
                .unwrap_or(false);
            tracing::trace!(
                "if-modified-since: {:?} vs {:?} = {}",
                since,
                last_modified,
                unmodified
            );
            if unmodified {
                let mut resp = Response::new(Body::empty());
                *resp.status_mut() = StatusCode::NOT_MODIFIED;
                return Cond::NoBody(resp);
            }
        }

        if let Some(if_range) = self.if_range {
            tracing::trace!("if-range? {:?} vs {:?}", if_range, last_modified);
            let can_range = !if_range.is_modified(None, last_modified.as_ref());

            if !can_range {
                return Cond::WithBody(None);
            }
        }

        Cond::WithBody(self.range)
    }
}

fn bytes_range(range: Option<Range>, max_len: u64) -> Result<(u64, u64), BadRange> {
    use std::ops::Bound;

    let range = if let Some(range) = range {
        range
    } else {
        return Ok((0, max_len));
    };

    let ret = range
        .iter()
        .map(|(start, end)| {
            let start = match start {
                Bound::Unbounded => 0,
                Bound::Included(s) => s,
                Bound::Excluded(s) => s + 1,
            };

            let end = match end {
                Bound::Unbounded => max_len,
                Bound::Included(s) => {
                    // For the special case where s == the file size
                    if s == max_len {
                        s
                    } else {
                        s + 1
                    }
                }
                Bound::Excluded(s) => s,
            };

            if start < end && end <= max_len {
                Ok((start, end))
            } else {
                tracing::trace!("unsatisfiable byte range: {}-{}/{}", start, end, max_len);
                Err(BadRange)
            }
        })
        .next()
        .unwrap_or(Ok((0, max_len)));
    ret
}

pub fn conditionals() -> impl Filter<Extract = (Conditionals,), Error = Infallible> + Copy {
    warp::any()
        .and(warp::header::headers_cloned())
        .map(|headers: HeaderMap| {
            tracing::trace!("{:?}", headers);
            let if_modified_since = headers.typed_get::<IfModifiedSince>();
            let if_unmodified_since = headers.typed_get::<IfUnmodifiedSince>();
            let if_range = headers.typed_get::<IfRange>();
            let range = headers.typed_get::<Range>();
            Conditionals {
                if_modified_since,
                if_unmodified_since,
                if_range,
                range,
            }
        })
}

pub async fn reply(
    full_path: FullPath,
    conditionals: Conditionals,
) -> Result<impl Reply, Rejection> {
    let path = full_path.as_str();
    tracing::trace!("path: {}", path);

    // path len must be greater than 1
    assert!(!path.is_empty());

    let current_dir = std::env::current_dir().unwrap();
    let path = current_dir.join(&path[1..]);
    let file = TkFile::open(&path).await.map_err(|_| reject::not_found())?;
    let (f, meta) = file_metadata(file).await?;
    let cond = conditionals.check(meta.modified().ok().map(LastModified::from));
    match cond {
        Cond::NoBody(resp) => Ok(resp),
        Cond::WithBody(range) => {
            match meta.is_file() {
                true => {
                    let len = meta.len();
                    let (start, end) = bytes_range(range, len)
                        .map(|(start, end)| (start, end))
                        .map_err(|_| reject::custom(ReplyError::BytesRangeError))?;
                    let sub_len = end - start;
                    tracing::trace!("range: {:?}", (start, end));

                    let stream = file_stream(f, optimal_buf_size(&meta), (start, end));
                    let body = warp::hyper::Body::wrap_stream(stream);
                    let mut resp = Response::new(body);

                    // partial content
                    if sub_len != len {
                        *resp.status_mut() = StatusCode::PARTIAL_CONTENT;
                        resp.headers_mut().typed_insert(
                            headers::ContentRange::bytes(start..end, len)
                                .expect("valid ContentRange"),
                        );
                    }

                    // content type
                    let mime = mime_guess::from_path(path).first_or_octet_stream();
                    resp.headers_mut()
                        .typed_insert(headers::ContentType::from(mime));
                    // content length
                    resp.headers_mut()
                        .typed_insert(headers::ContentLength(sub_len));
                    // accept ranges
                    resp.headers_mut()
                        .typed_insert(headers::AcceptRanges::bytes());
                    Ok(resp)
                }
                false => {
                    let mut dirs = match tokio::fs::read_dir(path).await {
                        Ok(f) => f,
                        Err(_) => return Err(warp::reject::custom(ReplyError::InternalError)),
                    };
                    let mut res = vec![];
                    loop {
                        match dirs.next_entry().await {
                            Ok(Some(dir)) => {
                                res.push(dir);
                            }
                            Ok(None) => break,
                            Err(_) => return Err(warp::reject::custom(ReplyError::InternalError)),
                        }
                    }
                    let mut s = String::new();
                    s.push_str("<h1>Directory List</h1><hr><ul>");
                    for entry in res {
                        s += &format!(
                            "<li><a href=\"{}\">{}</a></li>",
                            PathBuf::from_str(full_path.as_str())
                                .map_err(|_| reject::custom(ReplyError::InternalError))?
                                .join(entry.file_name().to_str().unwrap())
                                .to_str()
                                .unwrap(),
                            entry.file_name().to_str().unwrap()
                        );
                    }
                    s.push_str("</ul><hr>");
                    let mut resp = Response::new(warp::hyper::Body::from(s));
                    resp.headers_mut()
                        .typed_insert(headers::ContentType::html());
                    Ok(resp)
                }
            }
        }
    }
}

use bytes::{Bytes, BytesMut};

use crate::ReplyError;

fn file_stream(
    mut file: TkFile,
    buf_size: usize,
    (start, end): (u64, u64),
) -> impl Stream<Item = Result<Bytes, std::io::Error>> + Send {
    use std::io::SeekFrom;
    let seek = async move {
        if start != 0 {
            file.seek(SeekFrom::Start(start)).await?;
        }
        Ok(file)
    };
    seek.into_stream()
        .map(move |result| {
            let mut buf = BytesMut::with_capacity(buf_size);
            let mut len = end - start;
            let mut f = match result {
                Ok(f) => f,
                Err(err) => return Either::Left(stream::once(future::err(err))),
            };
            Either::Right(stream::poll_fn(move |cx| {
                if len == 0 {
                    return Poll::Ready(None);
                }
                // reserve_at_least(&mut buf, buf_size);
                let n = match ready!(poll_read_buf(Pin::new(&mut f), cx, &mut buf)) {
                    Ok(n) => n as u64,
                    Err(err) => return Poll::Ready(Some(Err(err))),
                };
                if n == 0 {
                    return Poll::Ready(None);
                }
                let mut chunk = buf.split().freeze();
                if n > len {
                    chunk = chunk.split_to(len as usize);
                    len = 0;
                } else {
                    len -= n;
                }
                Poll::Ready(Some(Ok(chunk)))
            }))
        })
        .flatten()

    // todo!()
}

/// Returns the file's handle and metadata, or rejects if it doesn't exist.
async fn file_metadata(f: TkFile) -> Result<(TkFile, Metadata), Rejection> {
    match f.metadata().await {
        Ok(meta) => Ok((f, meta)),
        Err(_err) => Err(reject::not_found()),
    }
}

fn optimal_buf_size(metadata: &Metadata) -> usize {
    let block_size = get_block_size(metadata);

    // // If file length is smaller than block size, don't waste space
    // // reserving a bigger-than-needed buffer.
    cmp::min(block_size as u64, metadata.len()) as usize
}

const DEFAULT_READ_BUF_SIZE: usize = 0x10000;

#[cfg(unix)]
fn get_block_size(metadata: &Metadata) -> usize {
    use std::os::unix::fs::MetadataExt;
    //TODO: blksize() returns u64, should handle bad cast...
    //(really, a block size bigger than 4gb?)

    // Use device blocksize unless it's really small.
    cmp::max(metadata.blksize() as usize, DEFAULT_READ_BUF_SIZE)
}

#[cfg(not(unix))]
fn get_block_size(_metadata: &Metadata) -> usize {
    DEFAULT_READ_BUF_SIZE
}
