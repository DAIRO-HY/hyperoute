//! 读取请求体：受 [`max_body_size`] 限制的整体读取，以及边收边解析的 multipart。
//!
//! multipart 的文件字段先放在内存里，超过 [`multipart_memory_limit`] 后转存到临时文件，后续数据边收边写，
//! 所以几 GB 的上传也只占用有限的内存。临时文件由 [`TempFile`] 管理，请求处理完自动删除。

use std::collections::HashMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use futures_util::StreamExt;
use http_body_util::{BodyDataStream, BodyExt};
use hyper::body::{Body, Bytes};
use tokio::io::AsyncWriteExt;

use crate::extract::body_read_error;
use crate::limits::{max_body_size, multipart_memory_limit, upload_temp_dir};
use crate::param::{ParamError, ParamErrorKind, ParamLocation};
use crate::{MultipartField, RequestBody};

/// 请求处理完自动删除的临时文件（drop 时删除；文件已经被移走时什么也不做）。
#[derive(Debug)]
pub struct TempFile {
    path: PathBuf,
}

static TEMP_FILE_COUNTER: AtomicU64 = AtomicU64::new(0);

impl TempFile {
    /// 在上传临时目录（[`upload_temp_dir`]，默认系统临时目录）下新建一个空文件；目录不存在时自动创建。
    /// 失败时的错误信息带上目录，方便排查权限等问题。
    pub async fn create() -> std::io::Result<(TempFile, tokio::fs::File)> {
        Self::create_in(upload_temp_dir()).await
    }

    async fn create_in(dir: PathBuf) -> std::io::Result<(TempFile, tokio::fs::File)> {
        let with_dir = |e: std::io::Error| std::io::Error::new(e.kind(), format!("upload temp dir {}: {e}", dir.display()));
        tokio::fs::create_dir_all(&dir).await.map_err(with_dir)?;
        let n = TEMP_FILE_COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = dir.join(format!("hyperoute-upload-{}-{n}", std::process::id()));
        let file = tokio::fs::File::create(&path).await.map_err(with_dir)?;
        Ok((TempFile { path }, file))
    }

    pub fn as_path(&self) -> &Path {
        &self.path
    }

    /// 不再自动删除，返回路径。
    pub fn keep(mut self) -> PathBuf {
        // 取走路径后 drop 看到的是空路径，什么也不删
        std::mem::take(&mut self.path)
    }
}

impl Drop for TempFile {
    fn drop(&mut self) {
        if !self.path.as_os_str().is_empty() {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

impl MultipartField {
    /// 字段内容；已经转存到临时文件时从文件读回内存。
    pub async fn into_bytes(self) -> std::io::Result<Vec<u8>> {
        match &self.spooled {
            Some(file) => tokio::fs::read(file.as_path()).await,
            None => Ok(self.data),
        }
    }

    /// 字段内容所在的临时文件：已经转存的直接用；还在内存里的写成一个新的临时文件。
    pub async fn into_temp_file(self) -> std::io::Result<TempFile> {
        if let Some(file) = self.spooled {
            return Ok(file);
        }
        let (temp, mut file) = TempFile::create().await?;
        file.write_all(&self.data).await?;
        file.flush().await?;
        Ok(temp)
    }
}

/// 请求体超过 [`max_body_size`] 时，计数的流用它中断 multer。
#[derive(Debug)]
struct BodyTooLarge;

impl fmt::Display for BodyTooLarge {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("request body exceeds the configured limit")
    }
}

impl std::error::Error for BodyTooLarge {}

/// 请求体太大：生成代码直接返回 413，不经过 `#[PARAM_ERROR]`。
pub fn payload_too_large(detail: &str) -> ParamError {
    ParamError::new(ParamLocation::Body, None, ParamErrorKind::PayloadTooLarge).with_reason(detail)
}

/// 有 `Content-Length` 时先比较，超过上限就不读了。
fn check_declared_size(body: &RequestBody, max: Option<u64>) -> Result<(), ParamError> {
    match (max, body.size_hint().exact()) {
        (Some(max), Some(len)) if len > max => Err(payload_too_large(&format!("content-length {len} > limit {max}"))),
        _ => Ok(()),
    }
}

/// 读取整个请求体（JSON / urlencoded 用），超过 [`max_body_size`] 时返回 413 错误。
pub async fn read_body_limited(body: RequestBody) -> Result<Bytes, ParamError> {
    let max = max_body_size();
    check_declared_size(&body, max)?;
    let Some(max) = max else {
        return body.collect().await.map(|b| b.to_bytes()).map_err(body_read_error);
    };
    let mut stream = BodyDataStream::new(body);
    let mut buffer = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(body_read_error)?;
        if buffer.len() as u64 + chunk.len() as u64 > max {
            return Err(payload_too_large(&format!("body exceeds limit {max}")));
        }
        buffer.extend_from_slice(&chunk);
    }
    Ok(Bytes::from(buffer))
}

/// 边收边解析 multipart，按字段名建索引（同名字段只保留最后一个）。
pub async fn read_multipart_fields(body: RequestBody, content_type: &str) -> Result<HashMap<String, MultipartField>, ParamError> {
    Ok(read_multipart_list(body, content_type).await?.into_iter().collect())
}

/// 边收边解析 multipart，按出现顺序返回全部字段。
pub async fn read_multipart_list(body: RequestBody, content_type: &str) -> Result<Vec<(String, MultipartField)>, ParamError> {
    parse_stream(body, content_type, multipart_memory_limit(), max_body_size()).await
}

pub(crate) async fn parse_stream(
    body: RequestBody,
    content_type: &str,
    memory_limit: u64,
    max: Option<u64>,
) -> Result<Vec<(String, MultipartField)>, ParamError> {
    check_declared_size(&body, max)?;
    let malformed = |reason: multer::Error| {
        if is_too_large(&reason) {
            return payload_too_large("body exceeds limit");
        }
        ParamError::new(ParamLocation::Body, None, ParamErrorKind::MalformedBody)
            .with_detail("invalid multipart/form-data")
            .with_reason(reason)
    };
    let boundary = multer::parse_boundary(content_type).map_err(malformed)?;

    // 累计收到的字节数，超过上限时让 multer 读取失败
    let mut received = 0u64;
    let stream = BodyDataStream::new(body).map(move |chunk| {
        let chunk = chunk?;
        received += chunk.len() as u64;
        match max {
            Some(max) if received > max => Err(Box::new(BodyTooLarge) as crate::BoxError),
            _ => Ok(chunk),
        }
    });
    let mut multipart = multer::Multipart::new(stream, boundary);

    let mut fields = Vec::new();
    while let Some(mut field) = multipart.next_field().await.map_err(malformed)? {
        let Some(name) = field.name().map(|s| s.to_string()) else {
            continue;
        };
        let file_name = field.file_name().map(|s| s.to_string());
        let field_content_type = field.content_type().map(|m| m.to_string());
        let is_file = file_name.is_some();

        let mut data = Vec::new();
        let mut spooled: Option<(TempFile, tokio::fs::File)> = None;
        while let Some(chunk) = field.chunk().await.map_err(malformed)? {
            if let Some((_, file)) = spooled.as_mut() {
                file.write_all(&chunk).await.map_err(spool_error)?;
                continue;
            }
            if data.len() as u64 + chunk.len() as u64 > memory_limit {
                if !is_file {
                    // 文本字段不落盘：超过阈值直接拒绝，防止超大文本字段占满内存
                    return Err(payload_too_large(&format!("text field {name:?} exceeds memory limit {memory_limit}")));
                }
                let (temp, mut file) = TempFile::create().await.map_err(spool_error)?;
                file.write_all(&data).await.map_err(spool_error)?;
                file.write_all(&chunk).await.map_err(spool_error)?;
                data = Vec::new();
                spooled = Some((temp, file));
                continue;
            }
            data.extend_from_slice(&chunk);
        }
        let spooled = match spooled {
            Some((temp, mut file)) => {
                file.flush().await.map_err(spool_error)?;
                Some(temp)
            }
            None => None,
        };
        fields.push((name, MultipartField { file_name, content_type: field_content_type, data, spooled }));
    }
    Ok(fields)
}

fn is_too_large(error: &multer::Error) -> bool {
    match error {
        multer::Error::StreamReadFailed(inner) => inner.downcast_ref::<BodyTooLarge>().is_some(),
        _ => false,
    }
}

/// 写临时文件失败是服务端的问题（目录没有权限、磁盘满等）：生成代码记日志并返回 500。
fn spool_error(error: std::io::Error) -> ParamError {
    ParamError::new(ParamLocation::Body, None, ParamErrorKind::Internal).with_reason(error)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn body(parts: &[(&str, Option<&str>, Vec<u8>)]) -> (String, Vec<u8>) {
        let mut out = Vec::new();
        for (name, file, data) in parts {
            out.extend_from_slice(b"--B\r\n");
            match file {
                Some(file) => out.extend_from_slice(
                    format!("content-disposition: form-data; name=\"{name}\"; filename=\"{file}\"\r\n\r\n").as_bytes(),
                ),
                None => out.extend_from_slice(format!("content-disposition: form-data; name=\"{name}\"\r\n\r\n").as_bytes()),
            }
            out.extend_from_slice(data);
            out.extend_from_slice(b"\r\n");
        }
        out.extend_from_slice(b"--B--\r\n");
        ("multipart/form-data; boundary=B".to_string(), out)
    }

    /// 分成小块发送，模拟网络上陆续到达的数据。
    fn chunked(data: Vec<u8>) -> RequestBody {
        let chunks: Vec<Result<hyper::body::Frame<Bytes>, crate::BoxError>> =
            data.chunks(1000).map(|c| Ok(hyper::body::Frame::data(Bytes::copy_from_slice(c)))).collect();
        BodyExt::boxed(http_body_util::StreamBody::new(futures_util::stream::iter(chunks)))
    }

    #[tokio::test]
    async fn spools_large_files_and_keeps_later_text_fields() {
        let big: Vec<u8> = (0..50_000u32).map(|i| i as u8).collect();
        let (ct, data) = body(&[("file", Some("a.bin"), big.clone()), ("token", None, b"t1".to_vec()), ("small", Some("s.txt"), b"hi".to_vec())]);
        let fields = parse_stream(chunked(data), &ct, 4096, None).await.unwrap();
        let names: Vec<&str> = fields.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, ["file", "token", "small"]);

        let mut fields: HashMap<String, MultipartField> = fields.into_iter().collect();
        let file = fields.remove("file").unwrap();
        assert!(file.data.is_empty());
        let path = file.spooled.as_ref().unwrap().as_path().to_path_buf();
        assert_eq!(std::fs::read(&path).unwrap(), big);
        let temp = file.into_temp_file().await.unwrap();
        assert_eq!(temp.as_path(), path);
        drop(temp);
        assert!(!path.exists(), "drop 后临时文件应当被删除");

        assert_eq!(fields["token"].data, b"t1");
        let small = fields.remove("small").unwrap();
        assert!(small.spooled.is_none());
        let temp = small.into_temp_file().await.unwrap();
        assert_eq!(std::fs::read(temp.as_path()).unwrap(), b"hi");
    }

    #[tokio::test]
    async fn temp_dir_is_created_or_reported() {
        let base = std::env::temp_dir().join(format!("hyperoute-tempdir-test-{}", std::process::id()));
        let nested = base.join("a/b");
        let (temp, _) = TempFile::create_in(nested.clone()).await.unwrap();
        assert!(temp.as_path().starts_with(&nested));
        drop(temp);

        // 目录位置被一个普通文件占了：报错信息里带上目录
        let blocker = base.join("file");
        std::fs::write(&blocker, b"x").unwrap();
        let e = TempFile::create_in(blocker.join("sub")).await.unwrap_err();
        assert!(e.to_string().contains("upload temp dir"), "{e}");
        assert!(e.to_string().contains(&blocker.display().to_string()), "{e}");
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[tokio::test]
    async fn rejects_large_text_fields_and_bodies() {
        let (ct, data) = body(&[("token", None, vec![b'x'; 5000])]);
        let e = parse_stream(chunked(data), &ct, 4096, None).await.unwrap_err();
        assert_eq!(e.kind, ParamErrorKind::PayloadTooLarge);

        // 没有 Content-Length（分块到达）时边读边计数
        let (ct, data) = body(&[("file", Some("a.bin"), vec![0; 20_000])]);
        let e = parse_stream(chunked(data), &ct, 4096, Some(10_000)).await.unwrap_err();
        assert_eq!(e.kind, ParamErrorKind::PayloadTooLarge);

        // 有 Content-Length 时直接拒绝
        let (ct, data) = body(&[("file", Some("a.bin"), vec![0; 20_000])]);
        let e = parse_stream(crate::full_request_body(data), &ct, 4096, Some(10_000)).await.unwrap_err();
        assert_eq!(e.kind, ParamErrorKind::PayloadTooLarge);
        assert!(e.reason.unwrap().starts_with("content-length"));
    }
}
