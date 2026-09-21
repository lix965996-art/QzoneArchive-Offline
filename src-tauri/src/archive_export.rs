use std::{
    collections::{HashMap, HashSet},
    fs::{self, File},
    io::{Read, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        Mutex,
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use chrono::{Datelike, TimeZone, Utc};
use reqwest::header::{
    HeaderMap, HeaderValue, ACCEPT, ACCEPT_LANGUAGE, COOKIE, RANGE, REFERER, USER_AGENT,
};
use rusqlite::params;
use serde::Serialize;
use tokio::io::AsyncWriteExt;
use zip::{write::SimpleFileOptions, CompressionMethod, ZipWriter};

use crate::{
    archive::{
        archive_items_for_export, exports_root_dir, html_escape, is_qq_missing_image_placeholder,
        is_qq_missing_video_placeholder, open_database, picture_url_candidates, qzone_text_html,
        validate_category, video_urls, ArchiveItem,
    },
    qlogin::{QLoginState, QzoneAuth},
};

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TransferProgress {
    status: &'static str,
    kind: &'static str,
    total: u64,
    completed: u64,
    downloaded: u64,
    skipped: u64,
    unavailable: u64,
    failed: u64,
    current: String,
    output_dir: Option<String>,
    message: String,
}

impl Default for TransferProgress {
    fn default() -> Self {
        Self {
            status: "idle",
            kind: "idle",
            total: 0,
            completed: 0,
            downloaded: 0,
            skipped: 0,
            unavailable: 0,
            failed: 0,
            current: String::new(),
            output_dir: None,
            message: "尚未开始下载或导出".into(),
        }
    }
}

pub struct ArchiveTransferState {
    progress: Mutex<TransferProgress>,
    cancel: AtomicBool,
    active: AtomicBool,
}

impl ArchiveTransferState {
    pub fn new() -> Self {
        Self {
            progress: Mutex::new(TransferProgress::default()),
            cancel: AtomicBool::new(false),
            active: AtomicBool::new(false),
        }
    }

    pub(crate) fn ensure_idle(&self) -> Result<(), String> {
        if self.active.load(Ordering::Acquire) {
            Err("已有媒体下载或导出任务正在运行".into())
        } else {
            Ok(())
        }
    }

    fn ensure_not_cancelled(&self, message: &str) -> Result<(), String> {
        if !self.cancel.load(Ordering::Acquire) {
            return Ok(());
        }
        if let Ok(mut progress) = self.progress.lock() {
            progress.status = "cancelled";
            progress.current.clear();
            progress.message = message.into();
        }
        Err(message.into())
    }

    fn begin(
        &self,
        kind: &'static str,
        message: impl Into<String>,
    ) -> Result<TransferLease<'_>, String> {
        self.active
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| "已有媒体下载或导出任务正在运行".to_string())?;
        self.cancel.store(false, Ordering::Release);
        match self.progress.lock() {
            Ok(mut progress) => {
                *progress = TransferProgress {
                    status: "running",
                    kind,
                    message: message.into(),
                    ..TransferProgress::default()
                };
            }
            Err(_) => {
                self.active.store(false, Ordering::Release);
                return Err("下载状态锁已损坏".into());
            }
        }
        Ok(TransferLease {
            state: self,
            handled: false,
        })
    }
}

struct TransferLease<'a> {
    state: &'a ArchiveTransferState,
    handled: bool,
}

impl TransferLease<'_> {
    fn finish(mut self) {
        self.handled = true;
    }
}

impl Drop for TransferLease<'_> {
    fn drop(&mut self) {
        self.state.active.store(false, Ordering::Release);
        if self.handled {
            return;
        }
        if let Ok(mut progress) = self.state.progress.lock() {
            if progress.status != "cancelled" {
                progress.status = "error";
                progress.current.clear();
                progress.message = "任务异常结束，已自动解除占用；现在可以重新下载或导出".into();
            }
        }
    }
}

#[derive(Clone, Copy)]
struct TimeRange {
    start_at: Option<i64>,
    end_at: Option<i64>,
}

impl TimeRange {
    fn new(start_at: Option<i64>, end_at: Option<i64>) -> Result<Self, String> {
        if start_at.zip(end_at).is_some_and(|(start, end)| start > end) {
            return Err("开始时间不能晚于结束时间".into());
        }
        Ok(Self { start_at, end_at })
    }

    fn includes(self, timestamp: i64) -> bool {
        self.start_at.is_none_or(|start| timestamp >= start)
            && self.end_at.is_none_or(|end| timestamp <= end)
    }
}

#[derive(Clone)]
enum MediaTaskKind {
    Image(Vec<String>),
    Video(Vec<String>),
}

#[derive(Clone)]
struct MediaTask {
    dynamic_id: i64,
    picture_index: Option<usize>,
    published_at: i64,
    kind: MediaTaskKind,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct DownloadFailure {
    dynamic_id: i64,
    media_type: &'static str,
    picture_index: Option<usize>,
    error: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct DownloadUnavailable {
    dynamic_id: i64,
    media_type: &'static str,
    picture_index: Option<usize>,
    reason: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct DownloadManifest {
    generated_at: i64,
    mode: String,
    total: u64,
    downloaded: u64,
    skipped: u64,
    unavailable: u64,
    failed: u64,
    unavailable_items: Vec<DownloadUnavailable>,
    failures: Vec<DownloadFailure>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExportResult {
    path: String,
    files: Vec<String>,
    warnings: Vec<String>,
    dynamics: usize,
    downloaded: u64,
    skipped: u64,
    unavailable: u64,
    failed: u64,
}

enum MediaDownloadOutcome {
    Downloaded,
    Skipped,
    Unavailable(String),
}

fn normalized_download_candidates(candidates: &[String]) -> Vec<String> {
    let mut seen = HashSet::new();
    candidates
        .iter()
        .filter_map(|candidate| {
            let candidate = candidate.trim().replace("&amp;", "&");
            if candidate.is_empty() {
                return None;
            }
            let candidate = if candidate.starts_with("//") {
                format!("https:{candidate}")
            } else {
                candidate
            };
            let parsed = url::Url::parse(&candidate).ok()?;
            if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none() {
                return None;
            }
            let normalized = parsed.to_string();
            seen.insert(normalized.clone()).then_some(normalized)
        })
        .collect()
}

fn media_download_client(auth: &QzoneAuth) -> Result<reqwest::Client, String> {
    let mut headers = HeaderMap::new();
    headers.insert(
        USER_AGENT,
        HeaderValue::from_str(auth.user_agent.trim())
            .map_err(|_| "QQ 登录会话中的浏览器标识无效，请重新登录后重试".to_owned())?,
    );
    headers.insert(
        COOKIE,
        HeaderValue::from_str(auth.cookie_header.trim())
            .map_err(|_| "QQ 登录会话中的 Cookie 格式无效，请重新登录后重试".to_owned())?,
    );
    headers.insert(
        REFERER,
        HeaderValue::from_static("https://user.qzone.qq.com/"),
    );
    headers.insert(
        ACCEPT_LANGUAGE,
        HeaderValue::from_static("zh-CN,zh;q=0.9,en;q=0.8"),
    );
    reqwest::Client::builder()
        .default_headers(headers)
        .redirect(reqwest::redirect::Policy::limited(5))
        .connect_timeout(std::time::Duration::from_secs(20))
        .timeout(std::time::Duration::from_secs(300))
        .build()
        .map_err(|error| format!("创建媒体下载客户端失败：{error}"))
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

fn set_progress(state: &ArchiveTransferState, update: impl FnOnce(&mut TransferProgress)) {
    if let Ok(mut progress) = state.progress.lock() {
        update(&mut progress);
    }
}

fn year_of(timestamp: i64) -> i32 {
    Utc.timestamp_opt(timestamp, 0)
        .single()
        .map(|value| value.year())
        .unwrap_or(1970)
}

fn ensure_absolute_directory(path: &str) -> Result<PathBuf, String> {
    let directory = PathBuf::from(path.trim());
    if !directory.is_absolute() {
        return Err("请选择有效的绝对保存路径".into());
    }
    fs::create_dir_all(&directory).map_err(|error| format!("创建保存目录失败：{error}"))?;
    Ok(directory)
}

fn collect_media_tasks(
    app: &tauri::AppHandle,
    owner_uin: &str,
    mode: &str,
    range: TimeRange,
    allowed_ids: Option<&HashSet<i64>>,
) -> Result<Vec<MediaTask>, String> {
    if !matches!(mode, "images" | "videos" | "all") {
        return Err("媒体下载类型必须是 images、videos 或 all".into());
    }
    let connection = open_database(app)?;
    let mut statement = connection
        .prepare(
            "SELECT id,published_at,pictures_json,video_json FROM archive_dynamics
             WHERE owner_uin=?1 ORDER BY published_at ASC,id ASC",
        )
        .map_err(|error| format!("准备媒体下载查询失败：{error}"))?;
    let rows = statement
        .query_map(params![owner_uin], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, Option<String>>(3)?,
            ))
        })
        .map_err(|error| format!("查询媒体下载列表失败：{error}"))?;
    let mut tasks = Vec::new();
    for row in rows {
        let (dynamic_id, published_at, pictures_json, video_json) =
            row.map_err(|error| format!("读取媒体下载记录失败：{error}"))?;
        if !range.includes(published_at)
            || allowed_ids.is_some_and(|ids| !ids.contains(&dynamic_id))
        {
            continue;
        }
        if matches!(mode, "images" | "all") {
            tasks.extend(
                picture_url_candidates(pictures_json)
                    .into_iter()
                    .enumerate()
                    .map(|(picture_index, candidates)| MediaTask {
                        dynamic_id,
                        picture_index: Some(picture_index),
                        published_at,
                        kind: MediaTaskKind::Image(candidates),
                    }),
            );
        }
        if matches!(mode, "videos" | "all") {
            let candidates = video_urls(video_json);
            if !candidates.is_empty() {
                tasks.push(MediaTask {
                    dynamic_id,
                    picture_index: None,
                    published_at,
                    kind: MediaTaskKind::Video(candidates),
                });
            }
        }
    }
    Ok(tasks)
}

fn existing_image(directory: &Path, stem: &str) -> Option<PathBuf> {
    ["jpg", "jpeg", "png", "gif", "webp", "avif", "bmp"]
        .into_iter()
        .map(|extension| directory.join(format!("{stem}.{extension}")))
        .find_map(|path| {
            if !path.metadata().is_ok_and(|metadata| metadata.len() > 32) {
                return None;
            }
            if !fs::read(&path).is_ok_and(|bytes| {
                image_extension(&bytes).is_some() && !is_qq_missing_image_placeholder(&bytes)
            }) {
                return None;
            }
            Some(path)
        })
}

fn media_requests(
    client: &reqwest::Client,
    auth: &QzoneAuth,
    url: &str,
) -> Vec<reqwest::RequestBuilder> {
    let trusted = url::Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(str::to_owned))
        .is_some_and(|host| {
            ["qq.com", "qpic.cn", "gtimg.cn", "qlogo.cn"]
                .iter()
                .any(|suffix| host == *suffix || host.ends_with(&format!(".{suffix}")))
        });
    [(true, true), (false, true), (false, false), (true, false)]
        .into_iter()
        .map(|(cookie, referer)| {
            client
                .get(url)
                .header(
                    COOKIE,
                    if cookie && trusted {
                        auth.cookie_header.as_str()
                    } else {
                        ""
                    },
                )
                .header(
                    REFERER,
                    if referer {
                        "https://user.qzone.qq.com/"
                    } else {
                        ""
                    },
                )
                .timeout(Duration::from_secs(45))
        })
        .collect()
}

fn validated_range_total(header: &str, expected_start: u64) -> Option<u64> {
    let (range, total) = header.strip_prefix("bytes ")?.split_once('/')?;
    let (start, end) = range.split_once('-')?;
    let (start, end, total) = (
        start.parse::<u64>().ok()?,
        end.parse::<u64>().ok()?,
        total.parse::<u64>().ok()?,
    );
    (start == expected_start && start <= end && end < total).then_some(total)
}

fn valid_video_file(path: &Path) -> bool {
    let Ok(mut file) = File::open(path) else {
        return false;
    };
    let Ok(metadata) = file.metadata() else {
        return false;
    };
    if metadata.len() <= 1024 {
        return false;
    }
    let mut header = [0; 16];
    if file.read_exact(&mut header).is_err() {
        return false;
    }
    let valid = &header[4..8] == b"ftyp" || header.starts_with(&[0x1a, 0x45, 0xdf, 0xa3]);
    valid
        && !(metadata.len() == 249_574
            && fs::read(path).is_ok_and(|bytes| is_qq_missing_video_placeholder(&bytes)))
}

fn image_extension(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        Some("jpg")
    } else if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("png")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some("gif")
    } else if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
        Some("webp")
    } else if bytes.starts_with(b"BM") {
        Some("bmp")
    } else if bytes.len() >= 12
        && &bytes[4..8] == b"ftyp"
        && matches!(&bytes[8..12], b"avif" | b"avis")
    {
        Some("avif")
    } else {
        None
    }
}

async fn download_image(
    client: &reqwest::Client,
    auth: &QzoneAuth,
    candidates: &[String],
    directory: &Path,
    stem: &str,
) -> Result<MediaDownloadOutcome, String> {
    fs::create_dir_all(directory).map_err(|error| format!("创建图片目录失败：{error}"))?;
    if let Some(path) = existing_image(directory, stem) {
        let _ = path;
        return Ok(MediaDownloadOutcome::Skipped);
    }
    let candidates = normalized_download_candidates(candidates);
    if candidates.is_empty() {
        return Ok(MediaDownloadOutcome::Unavailable(
            "QQ 归档中只保存了空白占位或格式失效的图片地址".into(),
        ));
    }
    let mut last_error = String::new();
    for attempt in 1..=3 {
        let mut unavailable_reason = None;
        for request in candidates
            .iter()
            .flat_map(|url| media_requests(client, auth, url))
        {
            let response = request
                .header(
                    ACCEPT,
                    "image/avif,image/webp,image/png,image/jpeg,image/*,*/*;q=0.8",
                )
                .send()
                .await;
            match response {
                Ok(response) if response.status().is_success() => {
                    if response
                        .content_length()
                        .is_some_and(|size| size > 50 * 1024 * 1024)
                    {
                        last_error = "图片超过 50 MB 安全限制".into();
                        continue;
                    }
                    match response.bytes().await {
                        Ok(bytes) => {
                            let Some(extension) = image_extension(&bytes) else {
                                last_error = "QQ 返回了非图片内容".into();
                                continue;
                            };
                            if is_qq_missing_image_placeholder(&bytes) {
                                unavailable_reason = Some(
                                    "当前图片地址返回占位图；可能是旧地址失效，尚不能确认原图已删除"
                                        .to_owned(),
                                );
                                continue;
                            }
                            let final_path = directory.join(format!("{stem}.{extension}"));
                            let part_path = directory.join(format!("{stem}.{extension}.part"));
                            tokio::fs::write(&part_path, &bytes)
                                .await
                                .map_err(|error| format!("写入图片临时文件失败：{error}"))?;
                            publish_file(&part_path, &final_path)?;
                            return Ok(MediaDownloadOutcome::Downloaded);
                        }
                        Err(error) => last_error = format!("读取图片失败：{error}"),
                    }
                }
                Ok(response) => last_error = format!("HTTP {}", response.status()),
                Err(error) => last_error = format!("请求图片失败：{error}"),
            }
        }
        if let Some(reason) = unavailable_reason {
            return Ok(MediaDownloadOutcome::Unavailable(reason));
        }
        if attempt < 3 {
            tokio::time::sleep(std::time::Duration::from_secs(attempt)).await;
        }
    }
    Err(last_error)
}

async fn download_video(
    client: &reqwest::Client,
    auth: &QzoneAuth,
    candidates: &[String],
    directory: &Path,
    stem: &str,
) -> Result<MediaDownloadOutcome, String> {
    fs::create_dir_all(directory).map_err(|error| format!("创建视频目录失败：{error}"))?;
    let final_path = directory.join(format!("{stem}.mp4"));
    if valid_video_file(&final_path) {
        return Ok(MediaDownloadOutcome::Skipped);
    }
    let candidates = normalized_download_candidates(candidates);
    if candidates.is_empty() {
        return Ok(MediaDownloadOutcome::Unavailable(
            "QQ 归档中没有可继续访问的原视频地址".into(),
        ));
    }
    let mut last_error = String::new();
    for attempt in 1..=3 {
        let mut unavailable_reason = None;
        for (url, request) in candidates.iter().flat_map(|url| {
            media_requests(client, auth, url)
                .into_iter()
                .map(move |request| (url, request))
        }) {
            // Never append bytes from different signed URLs to the same partial file.
            let key = url.bytes().fold(0xcbf29ce484222325_u64, |hash, byte| {
                (hash ^ u64::from(byte)).wrapping_mul(0x100000001b3)
            });
            let part_path = directory.join(format!("{stem}-{key:016x}.mp4.part"));
            let existing = part_path
                .metadata()
                .map(|metadata| metadata.len())
                .unwrap_or(0);
            let mut request = request.header("Accept-Encoding", "identity").header(
                ACCEPT,
                "video/mp4,video/*;q=0.9,application/octet-stream;q=0.8,*/*;q=0.5",
            );
            if existing > 0 {
                request = request.header(RANGE, format!("bytes={existing}-"));
            }
            match request.send().await {
                Ok(mut response) if response.status().is_success() => {
                    let content_type = response
                        .headers()
                        .get(reqwest::header::CONTENT_TYPE)
                        .and_then(|value| value.to_str().ok())
                        .unwrap_or("")
                        .to_ascii_lowercase();
                    if content_type.contains("text/html")
                        || content_type.contains("application/json")
                    {
                        last_error = format!("QQ 返回了非视频内容（{content_type}）");
                        continue;
                    }
                    let partial = response.status() == reqwest::StatusCode::PARTIAL_CONTENT;
                    let expected_total = if partial {
                        match response
                            .headers()
                            .get(reqwest::header::CONTENT_RANGE)
                            .and_then(|v| v.to_str().ok())
                            .and_then(|v| validated_range_total(v, existing))
                        {
                            Some(total) => Some(total),
                            None => {
                                let _ = tokio::fs::remove_file(&part_path).await;
                                last_error = "视频分片范围不匹配，已准备从头重试".into();
                                continue;
                            }
                        }
                    } else {
                        response.content_length()
                    };
                    let append = existing > 0 && partial;
                    let received =
                        match write_video_response(&mut response, &part_path, append).await {
                            Ok(received) => received,
                            Err(error) => {
                                last_error = error;
                                continue;
                            }
                        };
                    if received == 0
                        || !part_path
                            .metadata()
                            .is_ok_and(|metadata| metadata.len() > 1024)
                    {
                        last_error = "QQ 返回了空视频或非视频内容".into();
                        continue;
                    }
                    if expected_total.is_some_and(|size| {
                        part_path.metadata().map(|m| m.len()).unwrap_or(0) != size
                    }) {
                        last_error = "视频尚未下载完整，保留分片后重试".into();
                        continue;
                    }
                    if !valid_video_file(&part_path) {
                        let _ = tokio::fs::remove_file(&part_path).await;
                        unavailable_reason =
                            Some("当前视频地址未返回有效视频，需刷新地址后重试".into());
                        continue;
                    }
                    publish_file(&part_path, &final_path)?;
                    return Ok(MediaDownloadOutcome::Downloaded);
                }
                Ok(response) if response.status() == reqwest::StatusCode::RANGE_NOT_SATISFIABLE => {
                    let _ = tokio::fs::remove_file(&part_path).await;
                    last_error = "视频断点已失效，已清除临时分片并准备重新下载".into();
                }
                Ok(response) => last_error = format!("HTTP {}", response.status()),
                Err(error) => last_error = format!("请求视频失败：{error}"),
            }
        }
        if let Some(reason) = unavailable_reason {
            return Ok(MediaDownloadOutcome::Unavailable(reason));
        }
        if attempt < 3 {
            tokio::time::sleep(std::time::Duration::from_secs(attempt * 2)).await;
        }
    }
    Err(last_error)
}

async fn write_video_response(
    response: &mut reqwest::Response,
    part_path: &Path,
    append: bool,
) -> Result<u64, String> {
    let mut options = tokio::fs::OpenOptions::new();
    options.create(true).write(true);
    if append {
        options.append(true);
    } else {
        options.truncate(true);
    }
    let mut file = options
        .open(part_path)
        .await
        .map_err(|error| format!("打开视频临时文件失败：{error}"))?;
    let mut received = 0_u64;
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|error| format!("读取视频分块失败：{error}"))?
    {
        file.write_all(&chunk)
            .await
            .map_err(|error| format!("写入视频分块失败：{error}"))?;
        received += chunk.len() as u64;
    }
    file.flush()
        .await
        .map_err(|error| format!("刷新视频文件失败：{error}"))?;
    Ok(received)
}

async fn cancellable_download(
    operation: impl std::future::Future<Output = Result<MediaDownloadOutcome, String>>,
    state: &ArchiveTransferState,
    seconds: u64,
) -> Result<MediaDownloadOutcome, String> {
    tokio::select! {
        result = tokio::time::timeout(Duration::from_secs(seconds), operation) =>
            result.map_err(|_| "媒体处理超时，已跳过；再次导出时可重试".to_string())?,
        _ = async {
            while !state.cancel.load(Ordering::Acquire) {
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        } => Err("媒体下载已停止".into()),
    }
}

async fn run_media_download(
    app: &tauri::AppHandle,
    auth: &QzoneAuth,
    state: &ArchiveTransferState,
    mode: &str,
    range: TimeRange,
    output: &Path,
    allowed_ids: Option<&HashSet<i64>>,
) -> Result<
    (
        TransferProgress,
        Vec<DownloadFailure>,
        Vec<DownloadUnavailable>,
    ),
    String,
> {
    let tasks = collect_media_tasks(app, &auth.uin, mode, range, allowed_ids)?;
    fs::create_dir_all(output).map_err(|error| format!("创建媒体保存目录失败：{error}"))?;
    set_progress(state, |progress| {
        *progress = TransferProgress {
            status: "running",
            kind: "media",
            total: tasks.len() as u64,
            output_dir: Some(output.to_string_lossy().into_owned()),
            message: "正在准备批量下载媒体…".into(),
            ..TransferProgress::default()
        };
    });
    let client = media_download_client(auth)?;
    let mut failures = Vec::new();
    let mut unavailable_items = Vec::new();
    for task in tasks {
        if state.cancel.load(Ordering::Relaxed) {
            break;
        }
        let year = year_of(task.published_at);
        let directory = output.join(year.to_string());
        let (label, media_type) = match &task.kind {
            MediaTaskKind::Image(_) => {
                let index = task.picture_index.unwrap_or(0);
                (format!("图片 {}-{}", task.dynamic_id, index + 1), "image")
            }
            MediaTaskKind::Video(_) => (format!("视频 {}", task.dynamic_id), "video"),
        };
        set_progress(state, |progress| {
            progress.current = label.clone();
            progress.message = format!(
                "正在处理 {}（{}/{}）",
                label,
                progress.completed + 1,
                progress.total
            );
        });
        let result = match &task.kind {
            MediaTaskKind::Image(candidates) => {
                let index = task.picture_index.unwrap_or(0);
                let stem = format!("{}-p{}", task.dynamic_id, index + 1);
                let original = crate::archive::images_root_dir(app)?.join(&auth.uin);
                if existing_image(&directory, &stem).is_none() {
                    if let Some(cached) =
                        existing_image(&original, &format!("{}-{index}", task.dynamic_id))
                    {
                        fs::create_dir_all(&directory)
                            .map_err(|e| format!("创建媒体缓存失败：{e}"))?;
                        if let Some(extension) = cached.extension() {
                            fs::copy(
                                &cached,
                                directory.join(format!("{stem}.{}", extension.to_string_lossy())),
                            )
                            .map_err(|e| format!("复用归档图片失败：{e}"))?;
                        }
                    }
                }
                cancellable_download(
                    download_image(&client, auth, candidates, &directory, &stem),
                    state,
                    90,
                )
                .await
            }
            MediaTaskKind::Video(candidates) => {
                let stem = format!("{}-video", task.dynamic_id);
                let original = crate::archive::videos_root_dir(app)?
                    .join(format!("{}-{}.mp4", auth.uin, task.dynamic_id));
                let destination = directory.join(format!("{stem}.mp4"));
                if !valid_video_file(&destination) && valid_video_file(&original) {
                    fs::create_dir_all(&directory)
                        .map_err(|e| format!("创建视频缓存目录失败：{e}"))?;
                    fs::copy(&original, &destination)
                        .map_err(|e| format!("复用已播放视频失败：{e}"))?;
                }
                cancellable_download(
                    download_video(&client, auth, candidates, &directory, &stem),
                    state,
                    600,
                )
                .await
            }
        };
        match result {
            Ok(MediaDownloadOutcome::Downloaded) => set_progress(state, |progress| {
                progress.completed += 1;
                progress.downloaded += 1;
                progress.message = format!(
                    "已处理 {}/{}，新下载 {}，跳过已有 {}，原文件不可用 {}",
                    progress.completed,
                    progress.total,
                    progress.downloaded,
                    progress.skipped,
                    progress.unavailable
                );
            }),
            Ok(MediaDownloadOutcome::Skipped) => set_progress(state, |progress| {
                progress.completed += 1;
                progress.skipped += 1;
                progress.message = format!(
                    "已处理 {}/{}，新下载 {}，跳过已有 {}，原文件不可用 {}",
                    progress.completed,
                    progress.total,
                    progress.downloaded,
                    progress.skipped,
                    progress.unavailable
                );
            }),
            Ok(MediaDownloadOutcome::Unavailable(reason)) => {
                unavailable_items.push(DownloadUnavailable {
                    dynamic_id: task.dynamic_id,
                    media_type,
                    picture_index: task.picture_index,
                    reason: reason.clone(),
                });
                set_progress(state, |progress| {
                    progress.completed += 1;
                    progress.unavailable += 1;
                    progress.message =
                        format!("{} 原文件不可用，已如实标记并继续：{}", label, reason);
                });
            }
            Err(error) => {
                failures.push(DownloadFailure {
                    dynamic_id: task.dynamic_id,
                    media_type,
                    picture_index: task.picture_index,
                    error: error.clone(),
                });
                set_progress(state, |progress| {
                    progress.completed += 1;
                    progress.failed += 1;
                    progress.message = format!("{} 下载失败，已记录并继续：{}", label, error);
                });
            }
        }
    }
    let cancelled = state.cancel.load(Ordering::Relaxed);
    set_progress(state, |progress| {
        progress.status = if cancelled { "cancelled" } else { "completed" };
        progress.kind = "media";
        progress.current.clear();
        progress.message = if cancelled {
            "下载已停止；视频 .part 文件会在下次从断点继续".into()
        } else {
            format!(
                "媒体下载完成：新下载 {}，跳过已有 {}，原文件不可用 {}，真正失败 {}",
                progress.downloaded, progress.skipped, progress.unavailable, progress.failed
            )
        };
    });
    let progress = state
        .progress
        .lock()
        .map_err(|_| "下载状态锁已损坏")?
        .clone();
    Ok((progress, failures, unavailable_items))
}

#[tauri::command]
pub async fn start_media_download(
    app: tauri::AppHandle,
    login: tauri::State<'_, QLoginState>,
    state: tauri::State<'_, ArchiveTransferState>,
    mode: String,
    start_at: Option<i64>,
    end_at: Option<i64>,
    output_dir: Option<String>,
) -> Result<TransferProgress, String> {
    let transfer = state.begin("media", "正在准备批量下载媒体…")?;
    let auth = login.qzone_auth().await?;
    let range = TimeRange::new(start_at, end_at)?;
    let output = match output_dir.filter(|value| !value.trim().is_empty()) {
        Some(path) => ensure_absolute_directory(&path)?,
        None => exports_root_dir(&app)?.join(format!("media-{}-{}", auth.uin, now())),
    };
    let (progress, failures, unavailable_items) =
        run_media_download(&app, &auth, &state, &mode, range, &output, None).await?;
    let manifest = DownloadManifest {
        generated_at: now(),
        mode,
        total: progress.total,
        downloaded: progress.downloaded,
        skipped: progress.skipped,
        unavailable: progress.unavailable,
        failed: progress.failed,
        unavailable_items,
        failures,
    };
    fs::write(
        output.join("download-manifest.json"),
        serde_json::to_vec_pretty(&manifest)
            .map_err(|error| format!("生成下载清单失败：{error}"))?,
    )
    .map_err(|error| format!("写入下载清单失败：{error}"))?;
    transfer.finish();
    Ok(progress)
}

#[tauri::command]
pub fn get_transfer_progress(
    state: tauri::State<'_, ArchiveTransferState>,
) -> Result<TransferProgress, String> {
    state
        .progress
        .lock()
        .map(|progress| progress.clone())
        .map_err(|_| "下载状态锁已损坏".into())
}

#[tauri::command]
pub fn cancel_transfer(state: tauri::State<'_, ArchiveTransferState>) {
    state.cancel.store(true, Ordering::Relaxed);
}

fn selected_items(
    app: &tauri::AppHandle,
    owner_uin: &str,
    category: &str,
    ids: Option<Vec<i64>>,
    range: TimeRange,
) -> Result<Vec<ArchiveItem>, String> {
    validate_category(category)?;
    let selected = ids.map(|values| values.into_iter().collect::<HashSet<_>>());
    if selected.as_ref().is_some_and(HashSet::is_empty) {
        return Err("请先选择需要导出的归档".into());
    }
    let connection = open_database(app)?;
    let mut items = archive_items_for_export(&connection, owner_uin, category, selected.as_ref())?;
    items.retain(|item| range.includes(item.published_at));
    if items.is_empty() {
        return Err("所选分类和时间范围没有可以导出的归档".into());
    }
    Ok(items)
}

fn local_asset_map(items: &[ArchiveItem], media_root: &Path) -> HashMap<String, String> {
    let mut result = HashMap::new();
    for item in items {
        let year = year_of(item.published_at);
        let directory = media_root.join(year.to_string());
        for index in 0..item.picture_urls.len() {
            if let Some(path) = existing_image(&directory, &format!("{}-p{}", item.id, index + 1)) {
                if let Ok(relative) = path.strip_prefix(media_root.parent().unwrap_or(media_root)) {
                    result.insert(
                        format!("p:{}:{index}", item.id),
                        relative.to_string_lossy().replace('\\', "/"),
                    );
                }
            }
        }
        let video = directory.join(format!("{}-video.mp4", item.id));
        if valid_video_file(&video) {
            if let Ok(relative) = video.strip_prefix(media_root.parent().unwrap_or(media_root)) {
                result.insert(
                    format!("v:{}", item.id),
                    relative.to_string_lossy().replace('\\', "/"),
                );
            }
        }
    }
    result
}

fn render_html(
    owner_uin: &str,
    category: &str,
    items: &[&ArchiveItem],
    assets: &HashMap<String, String>,
) -> String {
    let category_name = match category {
        "self" => "本人动态",
        "other" => "其他动态",
        _ => "留言",
    };
    let mut cards = String::new();
    for item in items {
        cards.push_str(&format!(
            "<article class=\"card\" id=\"record-{}\"><header><strong>",
            item.id
        ));
        cards.push_str(&html_escape(
            item.author_name
                .as_deref()
                .or(item.author_uin.as_deref())
                .unwrap_or("QQ 用户"),
        ));
        cards.push_str("</strong><time data-time=\"");
        cards.push_str(&item.published_at.to_string());
        cards.push_str("\">");
        cards.push_str(
            &Utc.timestamp_opt(item.published_at, 0)
                .single()
                .map(|time| {
                    time.with_timezone(&chrono::Local)
                        .format("%Y-%m-%d %H:%M")
                        .to_string()
                })
                .unwrap_or_else(|| "时间未知".into()),
        );
        cards.push_str("</time></header><div class=\"content\">");
        cards.push_str(&qzone_text_html(item.content.as_deref()));
        cards.push_str("</div>");
        if !item.picture_urls.is_empty() {
            cards.push_str("<div class=\"pictures\">");
            for (index, _remote) in item.picture_urls.iter().enumerate() {
                if let Some(source) = assets.get(&format!("p:{}:{index}", item.id)) {
                    cards.push_str("<a target=\"_blank\" href=\"");
                    cards.push_str(&html_escape(source));
                    cards.push_str("\"><img loading=\"eager\" decoding=\"sync\" alt=\"点击查看完整图片\" src=\"");
                    cards.push_str(&html_escape(source));
                    cards.push_str("\"></a>");
                } else {
                    cards.push_str(&format!("<span class=\"missing\">图片未收录（记录 {}，第 {} 张）。请查看导出说明；不能据此判断原图已删除。</span>", item.id, index + 1));
                }
            }
            cards.push_str("</div>");
        }
        if item.video_url.is_some() {
            let source = assets.get(&format!("v:{}", item.id));
            if let Some(source) = source {
                cards.push_str("<video controls preload=\"metadata\" playsinline src=\"");
                cards.push_str(&html_escape(source));
                cards.push_str("\">浏览器不支持播放。</video><p><a class=\"video\" href=\"");
                cards.push_str(&html_escape(source));
                cards.push_str("\">▶ 单独打开视频（PDF 内不能播放，请在解压后的网页观看）</a></p>");
            } else {
                cards.push_str(&format!("<p class=\"missing\">视频未收录（记录 {}）。请查看导出说明；可能需要重新获取有效地址。</p>", item.id));
            }
        }
        cards.push_str("<div class=\"stats\">♥ ");
        cards.push_str(&item.like_count.to_string());
        cards.push_str("　💬 ");
        cards.push_str(&item.comment_count.to_string());
        cards.push_str("</div>");
        if !item.comments.is_empty() {
            cards.push_str("<section class=\"comments\">");
            for comment in &item.comments {
                cards.push_str("<div class=\"comment\"><b>");
                cards.push_str(&html_escape(
                    comment
                        .nickname
                        .as_deref()
                        .or(comment.uin.as_deref())
                        .unwrap_or("QQ 用户"),
                ));
                cards.push_str("：</b>");
                cards.push_str(&qzone_text_html(Some(&comment.content)));
                for reply in &comment.replies {
                    cards.push_str("<div class=\"reply\"><b>");
                    cards.push_str(&html_escape(
                        reply
                            .nickname
                            .as_deref()
                            .or(reply.uin.as_deref())
                            .unwrap_or("QQ 用户"),
                    ));
                    cards.push_str(" 回复：</b>");
                    cards.push_str(&qzone_text_html(Some(&reply.content)));
                    cards.push_str("</div>");
                }
                cards.push_str("</div>");
            }
            cards.push_str("</section>");
        }
        cards.push_str("</article>");
    }
    include_str!("customer_archive.html")
        .replace("{{TITLE}}", category_name)
        .replace("{{OWNER}}", &html_escape(owner_uin))
        .replace("{{COUNT}}", &items.len().to_string())
        .replace(
            "{{GENERATED}}",
            &chrono::Local::now().format("%Y-%m-%d %H:%M").to_string(),
        )
        .replace("{{CARDS}}", &cards)
}

fn edge_path() -> Option<PathBuf> {
    ["PROGRAMFILES(X86)", "PROGRAMFILES", "LOCALAPPDATA"]
        .into_iter()
        .filter_map(std::env::var_os)
        .map(PathBuf::from)
        .map(|base| {
            if base.ends_with("AppData\\Local") {
                base.join("Microsoft/Edge/Application/msedge.exe")
            } else {
                base.join("Microsoft/Edge/Application/msedge.exe")
            }
        })
        .find(|path| path.exists())
}

#[cfg(test)]
fn print_pdf(html: &Path, pdf: &Path, profile: &Path) -> Result<(), String> {
    print_pdf_cancellable(html, pdf, profile, None)
}

fn print_pdf_cancellable(
    html: &Path,
    pdf: &Path,
    profile: &Path,
    state: Option<&ArchiveTransferState>,
) -> Result<(), String> {
    let edge = edge_path().ok_or("未找到 Microsoft Edge，无法生成 PDF")?;
    fs::create_dir_all(profile).map_err(|error| format!("创建 PDF 浏览器配置目录失败：{error}"))?;
    let pending = pdf.with_extension(format!("{}.pending.pdf", std::process::id()));
    if pending.exists() {
        fs::remove_file(&pending).map_err(|e| e.to_string())?;
    }
    let url = url::Url::from_file_path(html)
        .map_err(|_| "无法把临时 HTML 转换为本地地址")?
        .to_string();
    let mut command = Command::new(edge);
    command
        .arg("--headless=new")
        .arg("--disable-gpu")
        .arg("--no-first-run")
        .arg("--allow-file-access-from-files")
        .arg("--disable-features=LazyFrameLoading,LazyImageLoading")
        .arg("--run-all-compositor-stages-before-draw")
        .arg("--virtual-time-budget=15000")
        .arg("--no-pdf-header-footer")
        .arg(format!("--user-data-dir={}", profile.display()))
        .arg(format!("--print-to-pdf={}", pending.display()))
        .arg(url);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000);
    }
    // Never pipe unread browser output: a full pipe can deadlock printing.
    command.stdout(Stdio::null()).stderr(Stdio::null());
    let mut child = command
        .spawn()
        .map_err(|error| format!("启动 Edge 生成 PDF 失败：{error}"))?;
    let started = Instant::now();
    let status = loop {
        if state.is_some_and(|s| s.cancel.load(Ordering::Acquire))
            || started.elapsed() > Duration::from_secs(120)
        {
            let _ = child.kill();
            let _ = child.wait();
            return Err("PDF 打印已取消或超过 120 秒；请重试，已下载的媒体会复用".into());
        }
        if let Some(status) = child
            .try_wait()
            .map_err(|error| format!("检查打印进程失败：{error}"))?
        {
            break status;
        }
        std::thread::sleep(Duration::from_millis(200));
    };
    let valid_pdf = fs::read(&pending)
        .ok()
        .is_some_and(|bytes| bytes.len() > 1000 && bytes.starts_with(b"%PDF-"));
    if !valid_pdf {
        let _ = fs::remove_file(&pending);
        return Err(format!("Edge 生成 PDF 失败：{}", status));
    }
    publish_file(&pending, pdf)?;
    Ok(())
}

fn publish_file(pending: &Path, output: &Path) -> Result<(), String> {
    let backup = output.with_extension(format!(
        "{}.previous",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    let existed = output.exists();
    if existed {
        fs::rename(output, &backup)
            .map_err(|e| format!("目标文件正在使用，旧文件保持不变：{e}"))?;
    }
    if let Err(error) = fs::rename(pending, output) {
        if existed {
            fs::rename(&backup, output).map_err(|restore| {
                format!(
                    "保存失败：{error}；旧文件保留于 {}：{restore}",
                    backup.display()
                )
            })?;
        }
        return Err(format!("保存失败，旧文件保持不变：{error}"));
    }
    if existed {
        let _ = fs::remove_file(backup);
    }
    Ok(())
}

fn group_items<'a>(
    items: &'a [ArchiveItem],
    split_by_year: bool,
) -> Vec<(String, Vec<&'a ArchiveItem>)> {
    if !split_by_year {
        return vec![("全部年份".into(), items.iter().collect())];
    }
    let mut groups: std::collections::BTreeMap<i32, Vec<&ArchiveItem>> =
        std::collections::BTreeMap::new();
    for item in items {
        groups
            .entry(year_of(item.published_at))
            .or_default()
            .push(item);
    }
    groups
        .into_iter()
        .map(|(year, values)| (year.to_string(), values))
        .collect()
}

// Bound browser memory even when a single year contains thousands of records.
fn pdf_groups(items: &[ArchiveItem], split_by_year: bool) -> Vec<(String, Vec<&ArchiveItem>)> {
    group_items(items, split_by_year)
        .into_iter()
        .flat_map(|(label, group)| {
            let split = group.len() > 100;
            group
                .chunks(100)
                .enumerate()
                .map(|(index, chunk)| {
                    (
                        if split {
                            format!("{label}-第{:03}册", index + 1)
                        } else {
                            label.clone()
                        },
                        chunk.to_vec(),
                    )
                })
                .collect::<Vec<_>>()
        })
        .collect()
}

struct ExportWorkspace(PathBuf);
impl ExportWorkspace {
    fn new(root: &Path) -> Result<Self, String> {
        static NEXT_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        fs::create_dir_all(root).map_err(|error| format!("创建导出父目录失败：{error}"))?;
        loop {
            let serial = NEXT_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let path = root.join(format!(".export-{}-{nonce}-{serial}", std::process::id()));
            match fs::create_dir(&path) {
                Ok(()) => return Ok(Self(path)),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(format!("创建导出工作目录失败：{error}")),
            }
        }
    }
}
impl Drop for ExportWorkspace {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

async fn prepare_offline_export(
    app: &tauri::AppHandle,
    auth: &QzoneAuth,
    state: &ArchiveTransferState,
    category: &str,
    items: &[ArchiveItem],
    root: &Path,
    media_mode: &str,
) -> Result<
    (
        HashMap<String, String>,
        TransferProgress,
        Vec<DownloadFailure>,
        Vec<DownloadUnavailable>,
    ),
    String,
> {
    let ids = items.iter().map(|item| item.id).collect::<HashSet<_>>();
    let range = TimeRange {
        start_at: None,
        end_at: None,
    };
    // Stable account-isolated download cache survives failures and cancellations.
    let cached_media = exports_root_dir(app)?
        .join("media-cache")
        .join(&auth.uin)
        .join("media");
    let (progress, failures, unavailable_items) = run_media_download(
        app,
        auth,
        state,
        media_mode,
        range,
        &cached_media,
        Some(&ids),
    )
    .await?;
    state.ensure_not_cancelled("导出已停止，已下载媒体已保留")?;
    let media_root = root.join("media");
    for relative in local_asset_map(items, &cached_media).values() {
        let source = cached_media.parent().unwrap().join(relative);
        let destination = root.join(relative);
        fs::create_dir_all(destination.parent().unwrap())
            .map_err(|e| format!("创建媒体目录失败：{e}"))?;
        fs::copy(source, destination).map_err(|e| format!("复制离线媒体失败：{e}"))?;
    }
    let assets = local_asset_map(items, &media_root);
    let all = items.iter().collect::<Vec<_>>();
    fs::write(
        root.join("index.html"),
        render_html(&auth.uin, category, &all, &assets),
    )
    .map_err(|error| format!("写入离线归档网页失败：{error}"))?;
    Ok((assets, progress, failures, unavailable_items))
}

#[tauri::command]
pub async fn export_archive_pdf(
    app: tauri::AppHandle,
    login: tauri::State<'_, QLoginState>,
    state: tauri::State<'_, ArchiveTransferState>,
    category: String,
    ids: Option<Vec<i64>>,
    start_at: Option<i64>,
    end_at: Option<i64>,
    output_dir: String,
    split_by_year: bool,
) -> Result<ExportResult, String> {
    let transfer = state.begin("pdf", "正在准备 PDF 导出…")?;
    let auth = login.qzone_auth().await?;
    let range = TimeRange::new(start_at, end_at)?;
    let items = selected_items(&app, &auth.uin, &category, ids, range)?;
    let output = ensure_absolute_directory(&output_dir)?;
    let workspace = ExportWorkspace::new(&exports_root_dir(&app)?)?;
    let temporary = workspace.0.join("content");
    fs::create_dir_all(&temporary).map_err(|error| format!("创建 PDF 临时目录失败：{error}"))?;
    let (assets, progress, failures, unavailable_items) =
        prepare_offline_export(&app, &auth, &state, &category, &items, &temporary, "images")
            .await?;
    state.ensure_not_cancelled("PDF 导出已停止")?;
    let groups = pdf_groups(&items, split_by_year);
    set_progress(&state, |value| {
        value.status = "running";
        value.kind = "pdf";
        value.total = groups.len() as u64;
        value.completed = 0;
        value.current.clear();
        value.message = "媒体准备完成，正在按年份生成 PDF…".into();
    });
    let mut files = Vec::new();
    let mut warnings = failures
        .iter()
        .map(|failure| {
            format!(
                "动态 {} 的图片下载失败：{}",
                failure.dynamic_id, failure.error
            )
        })
        .collect::<Vec<_>>();
    warnings.extend(unavailable_items.iter().map(|item| {
        format!(
            "动态 {} 的图片原文件不可用：{}",
            item.dynamic_id, item.reason
        )
    }));
    for (index, (label, group)) in groups.into_iter().enumerate() {
        state.ensure_not_cancelled("PDF 导出已停止")?;
        set_progress(&state, |value| {
            value.current = format!("{label} PDF");
            value.message = format!("正在生成 {label} PDF（{}/{}）", index + 1, value.total);
        });
        let html = temporary.join(format!("QQ空间归档-{label}.html"));
        fs::write(&html, render_html(&auth.uin, &category, &group, &assets))
            .map_err(|error| format!("写入 PDF 页面失败：{error}"))?;
        let pdf = output.join(format!("QQ空间归档-{label}.pdf"));
        match print_pdf_cancellable(
            &html,
            &pdf,
            &workspace.0.join(format!("edge-profile-{index}")),
            Some(&state),
        ) {
            Ok(()) => files.push(pdf.to_string_lossy().into_owned()),
            Err(error) => warnings.push(format!("{label}：{error}")),
        }
        set_progress(&state, |value| value.completed = index as u64 + 1);
    }
    state.ensure_not_cancelled("PDF 导出已停止，已完成的文件已保留")?;
    let _ = fs::remove_dir_all(&temporary);
    set_progress(&state, |value| {
        value.status = if files.is_empty() {
            "error"
        } else {
            "completed"
        };
        value.kind = "pdf";
        value.current.clear();
        value.output_dir = Some(output.to_string_lossy().into_owned());
        value.message = format!("PDF 导出完成，共生成 {} 个文件", files.len());
    });
    if files.is_empty() {
        return Err(warnings.join("；"));
    }
    transfer.finish();
    Ok(ExportResult {
        path: output.to_string_lossy().into_owned(),
        files,
        warnings,
        dynamics: items.len(),
        downloaded: progress.downloaded,
        skipped: progress.skipped,
        unavailable: progress.unavailable,
        failed: progress.failed,
    })
}

#[cfg(test)]
fn add_directory_to_zip(
    writer: &mut ZipWriter<File>,
    root: &Path,
    current: &Path,
) -> Result<(), String> {
    add_directory_to_zip_with_progress(writer, root, current, None)
}

fn count_directory_files(current: &Path) -> Result<u64, String> {
    let mut count = 0_u64;
    for entry in fs::read_dir(current).map_err(|error| format!("读取 ZIP 内容失败：{error}"))?
    {
        let path = entry
            .map_err(|error| format!("读取 ZIP 项目失败：{error}"))?
            .path();
        if path.is_dir() {
            count += count_directory_files(&path)?;
        } else {
            count += 1;
        }
    }
    Ok(count)
}

fn add_directory_to_zip_with_progress(
    writer: &mut ZipWriter<File>,
    root: &Path,
    current: &Path,
    state: Option<&ArchiveTransferState>,
) -> Result<(), String> {
    let options = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
    for entry in fs::read_dir(current).map_err(|error| format!("读取 ZIP 内容失败：{error}"))?
    {
        let entry = entry.map_err(|error| format!("读取 ZIP 项目失败：{error}"))?;
        let path = entry.path();
        let relative = path
            .strip_prefix(root)
            .map_err(|error| format!("生成 ZIP 相对路径失败：{error}"))?;
        let name = relative.to_string_lossy().replace('\\', "/");
        if path.is_dir() {
            writer
                .add_directory(format!("{name}/"), options)
                .map_err(|error| format!("创建 ZIP 目录失败：{error}"))?;
            add_directory_to_zip_with_progress(writer, root, &path, state)?;
        } else {
            if let Some(state) = state {
                state.ensure_not_cancelled("ZIP 压缩已停止")?;
                set_progress(state, |progress| {
                    progress.current = name.clone();
                    progress.message = format!(
                        "正在压缩 {}（{}/{}）",
                        name,
                        progress.completed + 1,
                        progress.total
                    );
                });
            }
            writer
                .start_file(name, options)
                .map_err(|error| format!("创建 ZIP 文件项失败：{error}"))?;
            let mut file =
                File::open(&path).map_err(|error| format!("打开待压缩文件失败：{error}"))?;
            let mut buffer = [0_u8; 64 * 1024];
            loop {
                if let Some(state) = state {
                    state.ensure_not_cancelled("ZIP 压缩已停止")?;
                }
                let read = file
                    .read(&mut buffer)
                    .map_err(|error| format!("读取待压缩文件失败：{error}"))?;
                if read == 0 {
                    break;
                }
                writer
                    .write_all(&buffer[..read])
                    .map_err(|error| format!("写入 ZIP 失败：{error}"))?;
            }
            if let Some(state) = state {
                set_progress(state, |progress| progress.completed += 1);
            }
        }
    }
    Ok(())
}

fn write_customer_delivery(
    root: &Path,
    files: &[String],
    warnings: &[String],
    count: usize,
    progress: &TransferProgress,
) -> Result<(), String> {
    let reader = fs::read(root.join("index.html")).map_err(|e| e.to_string())?;
    fs::write(root.join("开始阅读.html"), reader).map_err(|e| e.to_string())?;
    let urls = regex::Regex::new(r"https?://[^\s）)]+").expect("URL redaction regex");
    let details = warnings
        .iter()
        .map(|warning| {
            format!(
                "<li>{}</li>",
                html_escape(&urls.replace_all(warning, "[原始媒体地址已省略]"))
            )
        })
        .collect::<String>();
    let links = files
        .iter()
        .filter(|name| name.ends_with(".pdf") || name.starts_with("year-"))
        .map(|name| {
            format!(
                "<li><a href=\"{}\">{}</a></li>",
                html_escape(name),
                html_escape(name)
            )
        })
        .collect::<String>();
    let html = format!("<!doctype html><html lang=zh-CN><meta charset=utf-8><meta name=viewport content='width=device-width,initial-scale=1'><title>导出说明</title><style>body{{max-width:850px;margin:30px auto;padding:20px;font:16px/1.9 system-ui,'Microsoft YaHei';color:#243247}}li{{overflow-wrap:anywhere}}a{{color:#1765ba}}</style><h1>归档交付说明</h1><p><a href='开始阅读.html'>打开归档正文 →</a></p><p>本包包含 {count} 条记录。新下载 {} 项，复用 {} 项；当前地址不可用 {} 项，下载失败 {} 项。</p><h2>如何阅读</h2><ol><li>完整解压 ZIP，保留网页、media 和 pdf 文件夹的相对位置。</li><li>电脑用 Edge / Chrome 打开“开始阅读.html”，图片可放大，视频可播放，无需登录。</li><li>手机聊天预览不能正常打开网页时，先阅读 PDF。PDF 是静态阅读版，视频请在解压后的网页或 media 文件夹打开。</li></ol><h2>PDF 与分年目录</h2><ul>{links}</ul><h2>未收录内容与注意事项</h2><p>以下是本次导出的实际结果，不代表原文件永久删除。仅网页能访问而尚未成功保存的远程文件，不会假装已经离线收录。请向提供归档的人确认补充结果。</p><ul>{details}</ul><p>没有列出的警告不代表数据覆盖了账号全部历史；本包范围以导出时选定的分类和时间为准。归档可能包含私人内容，请妥善保管。</p></html>", progress.downloaded, progress.skipped, progress.unavailable, progress.failed);
    fs::write(root.join("导出说明.html"), html).map_err(|e| e.to_string())?;
    fs::write(root.join("先读我.txt"), "请先全部解压 ZIP，再打开“开始阅读.html”。\r\n不要在聊天软件或压缩包内直接预览网页。\r\n手机可直接阅读 pdf 文件夹内的 PDF；视频需在解压后的网页或 media 文件夹观看。\r\n未收录项目见“导出说明.html”，客户无需阅读任何 JSON 文件。\r\n").map_err(|e| e.to_string())?;
    Ok(())
}

#[tauri::command]
pub async fn export_share_zip(
    app: tauri::AppHandle,
    login: tauri::State<'_, QLoginState>,
    state: tauri::State<'_, ArchiveTransferState>,
    category: String,
    ids: Option<Vec<i64>>,
    start_at: Option<i64>,
    end_at: Option<i64>,
    output_path: String,
    split_by_year: bool,
    include_pdf: bool,
) -> Result<ExportResult, String> {
    let transfer = state.begin("zip", "正在准备可分享 ZIP…")?;
    let auth = login.qzone_auth().await?;
    let range = TimeRange::new(start_at, end_at)?;
    let items = selected_items(&app, &auth.uin, &category, ids, range)?;
    let output = PathBuf::from(output_path.trim());
    if !output.is_absolute()
        || output
            .extension()
            .is_none_or(|extension| extension != "zip")
    {
        return Err("请选择以 .zip 结尾的绝对保存路径".into());
    }
    if let Some(parent) = output.parent() {
        fs::create_dir_all(parent).map_err(|error| format!("创建 ZIP 保存目录失败：{error}"))?;
    }
    let workspace = ExportWorkspace::new(&exports_root_dir(&app)?)?;
    let temporary = workspace.0.join("content");
    fs::create_dir_all(&temporary).map_err(|error| format!("创建 ZIP 临时目录失败：{error}"))?;
    let (assets, progress, failures, unavailable_items) =
        prepare_offline_export(&app, &auth, &state, &category, &items, &temporary, "all").await?;
    state.ensure_not_cancelled("ZIP 导出已停止")?;
    set_progress(&state, |value| {
        value.status = "running";
        value.kind = "zip";
        value.total = 1;
        value.completed = 0;
        value.current = "离线网页".into();
        value.message = "媒体准备完成，正在生成离线网页和 ZIP…".into();
    });
    let mut files = vec!["index.html".into()];
    let mut warnings = failures
        .iter()
        .map(|failure| {
            format!(
                "动态 {} 的 {} 下载失败：{}",
                failure.dynamic_id, failure.media_type, failure.error
            )
        })
        .collect::<Vec<_>>();
    warnings.extend(unavailable_items.iter().map(|item| {
        format!(
            "动态 {} 的 {} 原文件不可用：{}",
            item.dynamic_id, item.media_type, item.reason
        )
    }));
    let groups = group_items(&items, split_by_year);
    if split_by_year {
        set_progress(&state, |value| {
            value.total = groups.len() as u64;
            value.completed = 0;
            value.message = "正在生成分年份离线网页…".into();
        });
        for (index, (label, group)) in groups.iter().enumerate() {
            state.ensure_not_cancelled("ZIP 导出已停止")?;
            set_progress(&state, |value| {
                value.current = format!("{label} 年网页");
                value.message = format!("正在生成 {label} 年网页（{}/{}）", index + 1, value.total);
            });
            let html = temporary.join(format!("year-{label}.html"));
            fs::write(&html, render_html(&auth.uin, &category, group, &assets))
                .map_err(|error| format!("写入年份网页失败：{error}"))?;
            files.push(format!("year-{label}.html"));
            set_progress(&state, |value| value.completed = index as u64 + 1);
        }
    }
    if include_pdf {
        let groups = pdf_groups(&items, split_by_year);
        let pdf_dir = temporary.join("pdf");
        fs::create_dir_all(&pdf_dir).map_err(|error| format!("创建 PDF 目录失败：{error}"))?;
        set_progress(&state, |value| {
            value.total = groups.len() as u64;
            value.completed = 0;
            value.current.clear();
            value.message = "正在生成 ZIP 内的 PDF…".into();
        });
        for (index, (label, group)) in groups.iter().enumerate() {
            state.ensure_not_cancelled("ZIP 内 PDF 生成已停止")?;
            set_progress(&state, |value| {
                value.current = format!("{label} PDF");
                value.message = format!(
                    "正在生成 ZIP 内的 {label} PDF（{}/{}）",
                    index + 1,
                    value.total
                );
            });
            let html = temporary.join(format!("print-{label}.html"));
            fs::write(&html, render_html(&auth.uin, &category, group, &assets))
                .map_err(|error| format!("写入打印页面失败：{error}"))?;
            let pdf = pdf_dir.join(format!("QQ空间归档-{label}.pdf"));
            match print_pdf_cancellable(
                &html,
                &pdf,
                &workspace.0.join(format!("edge-profile-{index}")),
                Some(&state),
            ) {
                Ok(()) => files.push(format!("pdf/QQ空间归档-{label}.pdf")),
                Err(error) => warnings.push(format!("{label} PDF：{error}")),
            }
            let _ = fs::remove_file(html);
            set_progress(&state, |value| value.completed = index as u64 + 1);
        }
        let _ = fs::remove_dir_all(temporary.join("edge-profile"));
    }
    write_customer_delivery(&temporary, &files, &warnings, items.len(), &progress)?;
    state.ensure_not_cancelled("ZIP 导出已停止")?;
    let zip_files = count_directory_files(&temporary)?;
    set_progress(&state, |value| {
        value.total = zip_files;
        value.completed = 0;
        value.current.clear();
        value.message = format!("正在压缩 ZIP，共 {zip_files} 个文件…");
    });
    let part = output.with_extension("zip.part");
    let file = File::create(&part).map_err(|error| format!("创建 ZIP 临时文件失败：{error}"))?;
    let mut writer = ZipWriter::new(file);
    if let Err(error) =
        add_directory_to_zip_with_progress(&mut writer, &temporary, &temporary, Some(&state))
    {
        drop(writer);
        let _ = fs::remove_file(&part);
        return Err(error);
    }
    if let Err(error) = writer.finish() {
        let _ = fs::remove_file(&part);
        return Err(format!("完成 ZIP 写入失败：{error}"));
    }
    if !part.metadata().is_ok_and(|metadata| metadata.len() > 100) {
        return Err("生成的 ZIP 文件为空".into());
    }
    publish_file(&part, &output)?;
    let _ = fs::remove_dir_all(&temporary);
    set_progress(&state, |value| {
        value.status = "completed";
        value.kind = "zip";
        value.current.clear();
        value.output_dir = output
            .parent()
            .map(|path| path.to_string_lossy().into_owned());
        value.message = format!("分享 ZIP 已生成：{}", output.display());
    });
    transfer.finish();
    Ok(ExportResult {
        path: output.to_string_lossy().into_owned(),
        files,
        warnings,
        dynamics: items.len(),
        downloaded: progress.downloaded,
        skipped: progress.skipped,
        unavailable: progress.unavailable,
        failed: progress.failed,
    })
}

#[cfg(test)]
mod tests {
    #[cfg(windows)]
    use super::print_pdf;
    use super::{
        add_directory_to_zip, image_extension, media_download_client,
        normalized_download_candidates, render_html, set_progress, ArchiveTransferState, TimeRange,
    };
    use crate::archive::ArchiveItem;
    use crate::qlogin::QzoneAuth;
    use std::{
        collections::HashMap,
        fs,
        fs::File,
        io::Read,
        time::{SystemTime, UNIX_EPOCH},
    };
    use zip::{ZipArchive, ZipWriter};

    #[test]
    fn time_range_includes_both_boundaries() {
        let range = TimeRange::new(Some(100), Some(200)).unwrap();
        assert!(range.includes(100));
        assert!(range.includes(200));
        assert!(!range.includes(99));
        assert!(!range.includes(201));
        assert!(TimeRange::new(Some(2), Some(1)).is_err());
    }

    #[test]
    fn pdf_batches_keep_all_records_in_order() {
        let items = (0..251)
            .map(|id| ArchiveItem {
                owner_uin: "test".into(),
                id,
                cell_id: id.to_string(),
                published_at: 1_700_000_000,
                content: None,
                author_uin: None,
                author_name: None,
                picture_urls: vec![],
                video_url: None,
                video_urls: vec![],
                video_cover_url: None,
                like_count: 0,
                comment_count: 0,
                likes: vec![],
                comments: vec![],
            })
            .collect::<Vec<_>>();
        for split in [true, false] {
            let groups = super::pdf_groups(&items, split);
            assert_eq!(groups.len(), 3);
            assert!(groups.iter().all(|(_, group)| group.len() <= 100));
            assert_eq!(
                groups
                    .iter()
                    .flat_map(|(_, g)| g.iter().map(|v| v.id))
                    .collect::<Vec<_>>(),
                (0..251).collect::<Vec<_>>()
            );
        }
    }

    #[test]
    fn failed_publish_preserves_previous_export() {
        let workspace = super::ExportWorkspace::new(&std::env::temp_dir()).unwrap();
        let output = workspace.0.join("archive.zip");
        fs::write(&output, b"previous valid archive").unwrap();
        assert!(super::publish_file(&workspace.0.join("missing.part"), &output).is_err());
        assert_eq!(fs::read(&output).unwrap(), b"previous valid archive");
        let path = workspace.0.clone();
        drop(workspace);
        assert!(!path.exists());
    }

    #[test]
    fn export_workspaces_are_isolated() {
        let workspaces = (0..100)
            .map(|_| super::ExportWorkspace::new(&std::env::temp_dir()).unwrap())
            .collect::<Vec<_>>();
        let unique = workspaces
            .iter()
            .map(|v| v.0.clone())
            .collect::<std::collections::HashSet<_>>();
        assert_eq!(unique.len(), workspaces.len());
        drop(workspaces);
        assert!(unique.iter().all(|path| !path.exists()));
    }

    #[test]
    fn detects_common_image_formats() {
        assert_eq!(image_extension(&[0xff, 0xd8, 0xff, 0x00]), Some("jpg"));
        assert_eq!(image_extension(b"\x89PNG\r\n\x1a\nrest"), Some("png"));
        assert_eq!(image_extension(b"not an image"), None);
    }

    #[test]
    fn validates_video_ranges_and_rejects_html_cache() {
        assert_eq!(
            super::validated_range_total("bytes 1024-2047/4096", 1024),
            Some(4096)
        );
        assert_eq!(
            super::validated_range_total("bytes 0-1023/4096", 1024),
            None
        );
        assert_eq!(
            super::validated_range_total("bytes 1024-4096/4096", 1024),
            None
        );
        let workspace = super::ExportWorkspace::new(&std::env::temp_dir()).unwrap();
        let file = workspace.0.join("bad.mp4");
        fs::write(&file, "<html>403 Forbidden</html>".repeat(100)).unwrap();
        assert!(!super::valid_video_file(&file));
        assert_eq!(super::image_extension(b"\0\0\0\x20ftypavif"), Some("avif"));
    }

    #[tokio::test]
    async fn image_download_recovers_when_server_rejects_referer() {
        use std::io::Write;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let mut requests = 0;
            loop {
                let (mut stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                    .unwrap();
                let mut header = Vec::new();
                let mut byte = [0];
                while !header.ends_with(b"\r\n\r\n") {
                    if stream.read(&mut byte).unwrap() == 0 {
                        break;
                    }
                    header.push(byte[0]);
                }
                requests += 1;
                let text = String::from_utf8_lossy(&header).to_ascii_lowercase();
                assert!(!text.contains("private-test-cookie"));
                if text.contains("referer: https://") {
                    stream.write_all(b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
                } else {
                    let mut body = b"\x89PNG\r\n\x1a\n".to_vec();
                    body.resize(64, 0);
                    stream
                        .write_all(
                            b"HTTP/1.1 200 OK\r\nContent-Length: 64\r\nConnection: close\r\n\r\n",
                        )
                        .unwrap();
                    stream.write_all(&body).unwrap();
                    return requests;
                }
            }
        });
        let auth = QzoneAuth {
            uin: "test".into(),
            cookie_header: "private-test-cookie".into(),
            user_agent: "test".into(),
            g_tk: 1,
        };
        let client = super::media_download_client(&auth).unwrap();
        let workspace = super::ExportWorkspace::new(&std::env::temp_dir()).unwrap();
        let outcome = super::download_image(
            &client,
            &auth,
            &[format!("http://{address}/picture")],
            &workspace.0,
            "image",
        )
        .await
        .unwrap();
        assert!(matches!(outcome, super::MediaDownloadOutcome::Downloaded));
        assert_eq!(server.join().unwrap(), 3);
        assert!(workspace.0.join("image.png").exists());
    }

    #[tokio::test]
    async fn video_download_resumes_truncated_body_and_reuses_valid_file() {
        use std::io::Write;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let mut bytes = vec![0_u8; 2048];
        bytes[4..8].copy_from_slice(b"ftyp");
        let expected = bytes.clone();
        let server = std::thread::spawn(move || {
            for attempt in 0..2 {
                let (mut stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                    .unwrap();
                let mut header = Vec::new();
                let mut byte = [0];
                while !header.ends_with(b"\r\n\r\n") {
                    if stream.read(&mut byte).unwrap() == 0 {
                        break;
                    }
                    header.push(byte[0]);
                }
                if attempt == 0 {
                    stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: video/mp4\r\nContent-Length: 2048\r\nConnection: close\r\n\r\n").unwrap();
                    stream.write_all(&bytes[..1024]).unwrap();
                } else {
                    assert!(String::from_utf8_lossy(&header)
                        .to_ascii_lowercase()
                        .contains("range: bytes=1024-"));
                    stream.write_all(b"HTTP/1.1 206 Partial Content\r\nContent-Type: video/mp4\r\nContent-Range: bytes 1024-2047/2048\r\nContent-Length: 1024\r\nConnection: close\r\n\r\n").unwrap();
                    stream.write_all(&bytes[1024..]).unwrap();
                }
            }
        });
        let auth = QzoneAuth {
            uin: "test".into(),
            g_tk: 1,
            cookie_header: "".into(),
            user_agent: "test".into(),
        };
        let client = media_download_client(&auth).unwrap();
        let workspace = super::ExportWorkspace::new(&std::env::temp_dir()).unwrap();
        let urls = vec![format!("http://{address}/video")];
        let result = super::download_video(&client, &auth, &urls, &workspace.0, "video")
            .await
            .unwrap();
        assert!(matches!(result, super::MediaDownloadOutcome::Downloaded));
        server.join().unwrap();
        assert_eq!(fs::read(workspace.0.join("video.mp4")).unwrap(), expected);
        let cached = super::download_video(&client, &auth, &urls, &workspace.0, "video")
            .await
            .unwrap();
        assert!(matches!(cached, super::MediaDownloadOutcome::Skipped));
    }

    #[test]
    fn normalizes_download_urls_and_rejects_placeholder_paths() {
        let candidates = vec![
            " /ac/b.gif ".into(),
            "javascript:alert(1)".into(),
            "//m.qpic.cn/example.jpg?a=1&amp;b=2".into(),
            "https://m.qpic.cn/example.jpg?a=1&b=2".into(),
        ];
        assert_eq!(
            normalized_download_candidates(&candidates),
            vec!["https://m.qpic.cn/example.jpg?a=1&b=2"]
        );
    }

    #[test]
    fn rejects_invalid_login_headers_before_starting_all_downloads() {
        let auth = QzoneAuth {
            uin: "10001".into(),
            g_tk: 1,
            cookie_header: "p_skey=valid\ninvalid".into(),
            user_agent: "Mozilla/5.0".into(),
        };
        let error = media_download_client(&auth).unwrap_err();
        assert!(error.contains("Cookie 格式无效"));
    }

    #[test]
    fn failed_transfer_releases_lease_and_allows_retry() {
        let state = ArchiveTransferState::new();
        let failed = state.begin("zip", "正在测试 ZIP").unwrap();
        assert!(state.ensure_idle().is_err());
        assert!(state.begin("pdf", "不应并发开始").is_err());
        drop(failed);

        assert!(state.ensure_idle().is_ok());
        let progress = state.progress.lock().unwrap().clone();
        assert_eq!(progress.status, "error");
        assert!(progress.message.contains("自动解除占用"));

        let retry = state.begin("zip", "正在重试 ZIP").unwrap();
        set_progress(&state, |progress| {
            progress.status = "completed";
            progress.message = "ZIP 已完成".into();
        });
        retry.finish();
        assert!(state.ensure_idle().is_ok());
        assert_eq!(state.progress.lock().unwrap().status, "completed");
    }

    #[test]
    fn share_zip_contains_nested_offline_files() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root =
            std::env::temp_dir().join(format!("qza-zip-test-{}-{nonce}", std::process::id()));
        let nested = root.join("media/2026");
        fs::create_dir_all(&nested).unwrap();
        fs::write(root.join("index.html"), b"offline archive").unwrap();
        fs::write(nested.join("1-p1.jpg"), b"fake-image").unwrap();
        let zip_path = root.with_extension("zip");
        let mut writer = ZipWriter::new(File::create(&zip_path).unwrap());
        add_directory_to_zip(&mut writer, &root, &root).unwrap();
        writer.finish().unwrap();

        let mut archive = ZipArchive::new(File::open(&zip_path).unwrap()).unwrap();
        let mut html = String::new();
        archive
            .by_name("index.html")
            .unwrap()
            .read_to_string(&mut html)
            .unwrap();
        assert_eq!(html, "offline archive");
        assert!(archive.by_name("media/2026/1-p1.jpg").is_ok());
        drop(archive);
        fs::remove_dir_all(&root).unwrap();
        fs::remove_file(&zip_path).unwrap();
    }

    #[test]
    fn printable_html_eagerly_loads_local_images() {
        let item = ArchiveItem {
            owner_uin: "10001".into(),
            id: 7,
            cell_id: "cell-7".into(),
            published_at: 1_700_000_000,
            content: Some("带图片的动态".into()),
            author_uin: Some("10001".into()),
            author_name: Some("测试用户".into()),
            picture_urls: vec!["https://example.invalid/image.jpg".into()],
            video_url: None,
            video_urls: Vec::new(),
            video_cover_url: None,
            like_count: 0,
            comment_count: 0,
            likes: Vec::new(),
            comments: Vec::new(),
        };
        let mut assets = HashMap::new();
        assets.insert("p:7:0".into(), "media/2023/7-p1.jpg".into());
        let html = render_html("10001", "self", &[&item], &assets);

        assert!(html.contains("loading=\"eager\""));
        assert!(html.contains("decoding=\"sync\""));
        assert!(html.contains("media/2023/7-p1.jpg"));
        assert!(html.contains("printReady"));
        assert!(!html.contains("loading=\"lazy\""));
    }

    #[test]
    #[ignore = "requires the locally installed Microsoft Edge"]
    #[cfg(windows)]
    fn edge_prints_a_real_pdf() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root =
            std::env::temp_dir().join(format!("qza-pdf-test-{}-{nonce}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        let html = root.join("sample.html");
        let pdf = root.join("sample.pdf");
        fs::write(
            &html,
            "<!doctype html><meta charset=utf-8><h1>QQ 空间归档 PDF 验证</h1>",
        )
        .unwrap();
        print_pdf(&html, &pdf, &root.join("profile")).unwrap();
        assert!(pdf.metadata().unwrap().len() > 1000);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    #[ignore = "generates fictional documentation fixtures using locally installed Edge"]
    #[cfg(windows)]
    fn public_documentation_fixture() {
        let root = std::path::PathBuf::from(std::env::var("QZA_PUBLIC_DEMO_DIR").unwrap());
        fs::create_dir_all(root.join("media")).unwrap();
        fs::copy("../public/demo.svg", root.join("media/demo.svg")).unwrap();
        let item = ArchiveItem {
            owner_uin: "100000001".into(), id: 1, cell_id: "fictional-demo".into(),
            published_at: 1_779_062_400,
            content: Some("虚构演示数据｜周末的记忆\n这是一条用于展示离线阅读效果的示例，不包含任何真实客户资料。图片已保存在 ZIP 内，解压后无需登录即可阅读。".into()),
            author_uin: Some("100000001".into()), author_name: Some("演示用户（虚构）".into()),
            picture_urls: vec!["https://example.invalid/demo.svg".into()],
            video_url: None, video_urls: vec![], video_cover_url: None,
            like_count: 0, comment_count: 0, likes: vec![], comments: vec![],
        };
        let mut assets = HashMap::new();
        assets.insert("p:1:0".into(), "media/demo.svg".into());
        let html = root.join("index.html");
        fs::write(&html, render_html("100000001", "self", &[&item], &assets)).unwrap();
        print_pdf(&html, &root.join("demo.pdf"), &root.join("profile")).unwrap();
        assert!(root.join("demo.pdf").metadata().unwrap().len() > 1000);
    }

    #[test]
    #[ignore = "requires explicitly selected local export fixture and Edge"]
    #[cfg(windows)]
    fn real_archive_pdf_and_zip_regression() {
        let source = std::path::PathBuf::from(std::env::var("QZA_EXPORT_FIXTURE").unwrap());
        let destination = std::path::PathBuf::from(std::env::var("QZA_VERIFY_OUTPUT").unwrap());
        fs::create_dir_all(&destination).unwrap();
        let workspace = super::ExportWorkspace::new(&destination).unwrap();
        let content = workspace.0.join("content");
        fs::create_dir_all(&content).unwrap();
        fn copy_tree(source: &std::path::Path, dest: &std::path::Path) {
            fs::create_dir_all(dest).unwrap();
            for entry in fs::read_dir(source).unwrap() {
                let entry = entry.unwrap();
                if entry.file_type().unwrap().is_dir() {
                    copy_tree(&entry.path(), &dest.join(entry.file_name()));
                } else {
                    fs::copy(entry.path(), dest.join(entry.file_name())).unwrap();
                }
            }
        }
        copy_tree(&source.join("media"), &content.join("media"));
        let original = fs::read_to_string(source.join("index.html")).unwrap();
        let pattern = regex::Regex::new(r"账号 (\d+)").unwrap();
        let owner = pattern.captures(&original).unwrap()[1].to_string();
        let database = std::env::var("QZA_EXPORT_DATABASE").unwrap();
        let connection = rusqlite::Connection::open_with_flags(
            database,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap();
        let items =
            crate::archive::archive_items_for_export(&connection, &owner, "self", None).unwrap();
        assert!(!items.is_empty());
        let assets = super::local_asset_map(&items, &content.join("media"));
        let html = render_html(&owner, "self", &items.iter().collect::<Vec<_>>(), &assets);
        fs::write(content.join("index.html"), &html).unwrap();
        let mut files = vec!["index.html".into()];
        fs::create_dir_all(content.join("pdf")).unwrap();
        for (index, (label, group)) in super::pdf_groups(&items, false).into_iter().enumerate() {
            let print_html = content.join("print.html");
            fs::write(&print_html, render_html(&owner, "self", &group, &assets)).unwrap();
            let relative = format!("pdf/QQ空间归档-{label}.pdf");
            print_pdf(
                &print_html,
                &content.join(&relative),
                &workspace.0.join(format!("profile-{index}")),
            )
            .unwrap();
            files.push(relative);
            fs::remove_file(print_html).unwrap();
        }
        let missing = items
            .iter()
            .map(|item| {
                item.picture_urls
                    .iter()
                    .enumerate()
                    .filter(|(index, _)| !assets.contains_key(&format!("p:{}:{index}", item.id)))
                    .count()
            })
            .sum::<usize>();
        let warnings = if missing > 0 {
            vec![format!("本次离线验证有 {missing} 张图片未在已有缓存中找到；未执行登录后的网络补取，不能据此判断已删除。")]
        } else {
            vec![]
        };
        super::write_customer_delivery(
            &content,
            &files,
            &warnings,
            items.len(),
            &super::TransferProgress {
                skipped: assets.len() as u64,
                failed: missing as u64,
                ..Default::default()
            },
        )
        .unwrap();
        copy_tree(&content, &destination.join("客户阅读"));
        let pending = destination.join("归档验证.zip.part");
        let zip_path = destination.join("归档验证.zip");
        let mut writer = ZipWriter::new(File::create(&pending).unwrap());
        add_directory_to_zip(&mut writer, &content, &content).unwrap();
        writer.finish().unwrap();
        super::publish_file(&pending, &zip_path).unwrap();
        let mut zip = ZipArchive::new(File::open(zip_path).unwrap()).unwrap();
        for index in 0..zip.len() {
            let mut file = zip.by_index(index).unwrap();
            assert!(!file.name().contains("profile"));
            std::io::copy(&mut file, &mut std::io::sink()).unwrap();
        }
        assert!(zip.by_name("开始阅读.html").is_ok());
        assert!(zip.by_name("导出说明.html").is_ok());
        assert!(!zip.file_names().any(|name| name.ends_with(".json")));
        assert!(files
            .iter()
            .filter(|name| name.ends_with(".pdf"))
            .all(|name| zip.by_name(name).unwrap().size() > 1000));
        println!(
            "Verified real archive PDF and all ZIP entries: {}",
            zip.len()
        );
    }
}
