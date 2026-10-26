use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock, RwLock};
use std::time::{SystemTime, UNIX_EPOCH};
use tauri::{AppHandle, Emitter};

pub type Shared = std::sync::Arc<Hub>;

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FileItem {
    pub name: String,
    pub size: u64,
    pub time: i64,
    pub from: String,
    pub client_id: String,
    pub kind: String,
    pub preview: bool,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionInfo {
    pub url: String,
    pub urls: Vec<String>,
    pub token: String,
    pub port: u16,
    pub qr_svg: String,
    pub share_dir: String,
    pub server_name: String,
    pub no_lan: bool,
    pub files: Vec<FileItem>,
}

#[derive(Clone, serde::Deserialize, Serialize)]
struct MetaEntry {
    from: String,
    #[serde(rename = "clientId", default)]
    client_id: String,
    time: i64,
}

struct Inner {
    meta: HashMap<String, MetaEntry>,
    reserved: HashSet<String>,
}

#[derive(Clone)]
pub struct NetInfo {
    pub port: u16,
    pub urls: Vec<String>,
    pub primary_url: String,
    pub qr_svg: String,
    pub no_lan: bool,
}

pub struct Hub {
    pub token: String,
    pub share_dir: PathBuf,
    pub meta_path: PathBuf,
    pub server_name: String,
    pub net: RwLock<Option<NetInfo>>,
    pub boot_error: Mutex<Option<String>>,
    inner: Mutex<Inner>,
    app: OnceLock<AppHandle>,
}

pub fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|err| err.into_inner())
}

impl Hub {
    pub fn open() -> Self {
        let share_dir = share_directory();
        let _ = fs::create_dir_all(&share_dir);
        clean_partials(&share_dir);
        let data_dir = dirs::data_local_dir()
            .unwrap_or_else(std::env::temp_dir)
            .join("快传");
        let _ = fs::create_dir_all(&data_dir);
        Self::new(share_dir, data_dir.join("meta.json"), computer_name())
    }

    pub fn new(share_dir: PathBuf, meta_path: PathBuf, server_name: String) -> Self {
        let _ = fs::create_dir_all(&share_dir);
        if let Some(parent) = meta_path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let meta = load_meta(&meta_path);
        Self {
            token: new_token(),
            share_dir,
            meta_path,
            server_name,
            net: RwLock::new(None),
            boot_error: Mutex::new(None),
            inner: Mutex::new(Inner {
                meta,
                reserved: HashSet::new(),
            }),
            app: OnceLock::new(),
        }
    }

    pub fn attach(&self, app: AppHandle) {
        let _ = self.app.set(app);
    }

    pub fn set_boot_error(&self, message: String) {
        *lock(&self.boot_error) = Some(message);
    }

    pub fn publish_net(&self, info: NetInfo) {
        let mut guard = self.net.write().unwrap_or_else(|err| err.into_inner());
        *guard = Some(info);
    }

    pub async fn wait_ready(&self) -> Result<(), String> {
        for _ in 0..200 {
            let ready = self
                .net
                .read()
                .unwrap_or_else(|err| err.into_inner())
                .is_some();
            if ready {
                return Ok(());
            }
            if let Some(message) = lock(&self.boot_error).clone() {
                return Err(message);
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        Err("局域网服务启动超时".into())
    }

    pub fn session(&self) -> Result<SessionInfo, String> {
        let net = self
            .net
            .read()
            .unwrap_or_else(|err| err.into_inner())
            .clone()
            .ok_or_else(|| "局域网服务还没准备好".to_string())?;
        Ok(SessionInfo {
            url: net.primary_url,
            urls: net.urls,
            token: self.token.clone(),
            port: net.port,
            qr_svg: net.qr_svg,
            share_dir: self.share_dir.display().to_string(),
            server_name: self.server_name.clone(),
            no_lan: net.no_lan,
            files: self.snapshot(),
        })
    }

    pub fn snapshot(&self) -> Vec<FileItem> {
        let mut inner = lock(&self.inner);
        let mut items = Vec::new();
        let mut live = HashSet::new();
        let entries = match fs::read_dir(&self.share_dir) {
            Ok(entries) => entries,
            Err(_) => return Vec::new(),
        };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with('.') {
                continue;
            }
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            live.insert(name.clone());
            let meta = inner.meta.get(&name).cloned();
            items.push(file_item(&path, &name, meta));
        }
        let before = inner.meta.len();
        inner.meta.retain(|name, _| live.contains(name));
        if inner.meta.len() != before {
            let _ = save_meta(&self.meta_path, &inner.meta);
        }
        items.sort_by(|a, b| b.time.cmp(&a.time).then_with(|| a.name.cmp(&b.name)));
        items
    }

    pub fn import_paths(
        &self,
        paths: &[String],
        from: &str,
        client_id: &str,
    ) -> Result<Vec<FileItem>, String> {
        let sources = collect_files(paths);
        if sources.is_empty() {
            return Err("没有找到可以发送的文件".into());
        }
        let planned = {
            let mut inner = lock(&self.inner);
            let mut planned = Vec::new();
            for src in sources {
                let original = src
                    .file_name()
                    .map(|name| name.to_string_lossy().to_string())
                    .unwrap_or_else(|| "file".into());
                let safe = safe_name(&original);
                let name = unique_name(&self.share_dir, &safe, &inner.reserved);
                inner.reserved.insert(name.clone());
                planned.push((src, name));
            }
            planned
        };

        let mut saved = Vec::new();
        let mut first_error = None;
        for (src, name) in planned {
            let dest = self.share_dir.join(&name);
            match fs::copy(&src, &dest) {
                Ok(_) => {
                    let meta = MetaEntry {
                        from: from.to_string(),
                        client_id: client_id.to_string(),
                        time: now_ms(),
                    };
                    let mut inner = lock(&self.inner);
                    inner.reserved.remove(&name);
                    inner.meta.insert(name.clone(), meta.clone());
                    let _ = save_meta(&self.meta_path, &inner.meta);
                    drop(inner);
                    saved.push(file_item(&dest, &name, Some(meta)));
                }
                Err(err) => {
                    let _ = fs::remove_file(&dest);
                    lock(&self.inner).reserved.remove(&name);
                    if first_error.is_none() {
                        first_error = Some(io_message(&err));
                    }
                    eprintln!("复制失败 {}: {err}", src.display());
                }
            }
        }
        if saved.is_empty() {
            return Err(first_error.unwrap_or_else(|| "没有文件被添加".into()));
        }
        self.notify();
        Ok(saved)
    }

    pub fn save_text(&self, text: &str, from: &str, client_id: &str) -> Result<FileItem, String> {
        if text.trim().is_empty() {
            return Err("没有可以发送的文字".into());
        }
        const MAX_TEXT: usize = 2 * 1024 * 1024;
        if text.len() > MAX_TEXT {
            return Err("文字太长了，最多 2 MB".into());
        }
        let name = self.reserve_name("文字.txt");
        let dest = self.share_dir.join(&name);
        if let Err(err) = fs::write(&dest, text.as_bytes()) {
            self.unreserve(&name);
            let _ = fs::remove_file(&dest);
            eprintln!("保存文字失败: {err}");
            return Err(io_message(&err));
        }
        Ok(self.remember_upload(&name, from, client_id))
    }

    pub fn read_text(&self, name: &str) -> Result<String, String> {
        let path = resolve_file(&self.share_dir, name).ok_or_else(|| "没有这个文件".to_string())?;
        let file_name = path
            .file_name()
            .map(|value| value.to_string_lossy().to_string())
            .unwrap_or_else(|| name.to_string());
        if classify(&file_name).0 != "text" {
            return Err("这不是文字文件".into());
        }
        let meta = fs::metadata(&path).map_err(|_| "没有这个文件".to_string())?;
        const MAX_TEXT: u64 = 2 * 1024 * 1024;
        if meta.len() > MAX_TEXT {
            return Err("文字太长，请直接打开文件".into());
        }
        let bytes = fs::read(&path).map_err(|err| {
            eprintln!("读取文字失败 {}: {err}", path.display());
            io_message(&err)
        })?;
        let content = bytes
            .strip_prefix(&[0xEF, 0xBB, 0xBF])
            .unwrap_or(bytes.as_slice());
        String::from_utf8(content.to_vec()).map_err(|_| "这个文件不是 UTF-8 文字".to_string())
    }

    pub fn remember_upload(&self, name: &str, from: &str, client_id: &str) -> FileItem {
        let meta = MetaEntry {
            from: from.to_string(),
            client_id: client_id.to_string(),
            time: now_ms(),
        };
        {
            let mut inner = lock(&self.inner);
            inner.reserved.remove(name);
            inner.meta.insert(name.to_string(), meta.clone());
            let _ = save_meta(&self.meta_path, &inner.meta);
        }
        let item = file_item(&self.share_dir.join(name), name, Some(meta));
        self.notify();
        item
    }

    pub fn reserve_name(&self, original: &str) -> String {
        let safe = safe_name(original);
        let mut inner = lock(&self.inner);
        let name = unique_name(&self.share_dir, &safe, &inner.reserved);
        inner.reserved.insert(name.clone());
        name
    }

    pub fn unreserve(&self, name: &str) {
        lock(&self.inner).reserved.remove(name);
    }

    pub fn delete(&self, name: &str) -> Result<(), String> {
        let path = resolve_file(&self.share_dir, name).ok_or_else(|| "没有这个文件".to_string())?;
        fs::remove_file(&path).map_err(|err| {
            eprintln!("删除失败 {}: {err}", path.display());
            io_message(&err)
        })?;
        {
            let mut inner = lock(&self.inner);
            inner.meta.remove(name);
            let _ = save_meta(&self.meta_path, &inner.meta);
        }
        self.notify();
        Ok(())
    }

    pub fn clear(&self) -> Result<usize, String> {
        let mut removed = 0;
        let entries = fs::read_dir(&self.share_dir).map_err(|err| io_message(&err))?;
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with('.') || !entry.path().is_file() {
                continue;
            }
            fs::remove_file(entry.path()).map_err(|err| {
                eprintln!("清空失败 {name}: {err}");
                io_message(&err)
            })?;
            removed += 1;
        }
        {
            let mut inner = lock(&self.inner);
            inner.meta.clear();
            let _ = save_meta(&self.meta_path, &inner.meta);
        }
        self.notify();
        Ok(removed)
    }

    pub fn notify(&self) {
        if let Some(app) = self.app.get() {
            let _ = app.emit("files-changed", ());
        }
    }
}

fn file_item(path: &Path, name: &str, meta: Option<MetaEntry>) -> FileItem {
    let (kind, preview) = classify(name);
    let size = path.metadata().map(|meta| meta.len()).unwrap_or(0);
    let (from, client_id, time) = match meta {
        Some(meta) => (meta.from, meta.client_id, meta.time),
        None => (String::new(), String::new(), mtime_ms(path)),
    };
    FileItem {
        name: name.to_string(),
        size,
        time,
        from,
        client_id,
        kind: kind.to_string(),
        preview,
    }
}

pub fn resolve_file(dir: &Path, name: &str) -> Option<PathBuf> {
    if name.is_empty()
        || name.starts_with('.')
        || name == "."
        || name == ".."
        || name.contains('/')
        || name.contains('\\')
        || name.contains('\0')
    {
        return None;
    }
    let candidate = dir.join(name);
    if !candidate.is_file() {
        return None;
    }
    let root = dir.canonicalize().ok()?;
    let real = candidate.canonicalize().ok()?;
    if real.parent()? != root {
        return None;
    }
    Some(real)
}

pub fn safe_name(raw: &str) -> String {
    let replaced = raw.replace('\\', "/");
    let base = replaced.rsplit('/').next().unwrap_or("file");
    let mut name: String = base
        .chars()
        .map(|ch| {
            if ch.is_control() || matches!(ch, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*')
            {
                '_'
            } else {
                ch
            }
        })
        .collect();
    name = name
        .trim_matches(|ch: char| ch == '.' || ch == ' ')
        .to_string();
    if name.is_empty() {
        name = "file".into();
    }
    let stem = name.split('.').next().unwrap_or("");
    if is_reserved(stem) {
        name = format!("_{name}");
    }
    if name.chars().count() > 80 {
        let (stem, ext) = split_ext(&name);
        let stem: String = stem.chars().take(70).collect();
        name = format!("{stem}{ext}");
    }
    if name.is_empty() {
        "file".into()
    } else {
        name
    }
}

pub fn unique_name(dir: &Path, name: &str, reserved: &HashSet<String>) -> String {
    let (stem, ext) = split_ext(name);
    let mut candidate = name.to_string();
    let mut index = 2u32;
    while reserved.contains(&candidate) || dir.join(&candidate).exists() {
        candidate = format!("{stem} ({index}){ext}");
        index += 1;
        if index > 5000 {
            let suffix = &uuid::Uuid::new_v4().simple().to_string()[..6];
            candidate = format!("{stem}-{suffix}{ext}");
            break;
        }
    }
    candidate
}

fn split_ext(name: &str) -> (&str, &str) {
    match name.rfind('.') {
        Some(index) if index > 0 && name.len() - index <= 12 => (&name[..index], &name[index..]),
        _ => (name, ""),
    }
}

fn is_reserved(stem: &str) -> bool {
    const NAMES: &[&str] = &[
        "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
        "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
    ];
    NAMES.iter().any(|name| stem.eq_ignore_ascii_case(name))
}

pub fn classify(name: &str) -> (&'static str, bool) {
    match extension(name).as_str() {
        "jpg" | "jpeg" | "png" | "gif" | "webp" | "bmp" | "avif" => ("image", true),
        "heic" | "heif" => ("image", false),
        "mp4" | "webm" | "mov" | "m4v" => ("video", true),
        "mkv" | "avi" => ("video", false),
        "mp3" | "wav" | "m4a" | "aac" | "ogg" | "flac" => ("audio", true),
        "pdf" => ("pdf", false),
        "zip" | "7z" | "rar" | "tar" | "gz" => ("archive", false),
        "txt" | "md" | "json" | "csv" | "log" => ("text", true),
        _ => ("file", false),
    }
}

pub fn extension(name: &str) -> String {
    match name.rfind('.') {
        Some(index) if index > 0 && index + 1 < name.len() => name[index + 1..].to_ascii_lowercase(),
        _ => String::new(),
    }
}

pub fn mime_for(name: &str, preview: bool) -> &'static str {
    if !preview {
        return "application/octet-stream";
    }
    match extension(name).as_str() {
        "jpg" | "jpeg" => "image/jpeg",
        "png" => "image/png",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "bmp" => "image/bmp",
        "avif" => "image/avif",
        "mp4" | "m4v" => "video/mp4",
        "webm" => "video/webm",
        "mov" => "video/quicktime",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "m4a" | "aac" => "audio/mp4",
        "ogg" => "audio/ogg",
        "flac" => "audio/flac",
        "txt" | "md" | "json" | "csv" | "log" => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

fn collect_files(paths: &[String]) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for path in paths {
        walk(Path::new(path), &mut out, 0);
        if out.len() >= 2000 {
            break;
        }
    }
    out
}

fn walk(path: &Path, out: &mut Vec<PathBuf>, depth: usize) {
    if out.len() >= 2000 || depth > 8 {
        return;
    }
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_default();
    if name.starts_with('.') {
        return;
    }
    if path.is_file() {
        out.push(path.to_path_buf());
        return;
    }
    if path.is_dir() {
        if let Ok(entries) = fs::read_dir(path) {
            for entry in entries.flatten() {
                walk(&entry.path(), out, depth + 1);
            }
        }
    }
}

fn clean_partials(dir: &Path) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with(".part-") {
            let _ = fs::remove_file(entry.path());
        }
    }
}

fn load_meta(path: &Path) -> HashMap<String, MetaEntry> {
    let Ok(text) = fs::read_to_string(path) else {
        return HashMap::new();
    };
    serde_json::from_str(&text).unwrap_or_default()
}

fn save_meta(path: &Path, meta: &HashMap<String, MetaEntry>) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|err| err.to_string())?;
    }
    let text = serde_json::to_string_pretty(meta).map_err(|err| err.to_string())?;
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, text).map_err(|err| err.to_string())?;
    fs::rename(&tmp, path).map_err(|err| err.to_string())?;
    Ok(())
}

fn share_directory() -> PathBuf {
    if let Some(desktop) = dirs::desktop_dir() {
        let dir = desktop.join("快传");
        if fs::create_dir_all(&dir).is_ok() {
            return dir;
        }
    }
    if let Some(docs) = dirs::document_dir() {
        let dir = docs.join("快传");
        if fs::create_dir_all(&dir).is_ok() {
            return dir;
        }
    }
    let fallback = std::env::temp_dir().join("快传");
    let _ = fs::create_dir_all(&fallback);
    fallback
}

fn computer_name() -> String {
    std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .unwrap_or_else(|_| "这台电脑".into())
}

fn new_token() -> String {
    const ALPHABET: &[u8] = b"abcdefghjkmnpqrstuvwxyz23456789";
    let bytes = uuid::Uuid::new_v4().into_bytes();
    bytes
        .iter()
        .take(6)
        .map(|byte| ALPHABET[(*byte as usize) % ALPHABET.len()] as char)
        .collect()
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0)
}

fn mtime_ms(path: &Path) -> i64 {
    path.metadata()
        .and_then(|meta| meta.modified())
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0)
}

fn io_message(err: &io::Error) -> String {
    match err.raw_os_error() {
        Some(112) | Some(28) => "磁盘空间不足".into(),
        Some(32) | Some(33) => "文件正在被占用".into(),
        _ => "保存文件失败".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safe_name_strips_traversal_and_reserved_words() {
        assert_eq!(safe_name("../../outside.txt"), "outside.txt");
        assert_eq!(safe_name("..\\..\\outside.txt"), "outside.txt");
        assert_eq!(safe_name("CON.txt"), "_CON.txt");
        assert_eq!(safe_name("   "), "file");
        assert_eq!(safe_name("照片.jpg"), "照片.jpg");
        assert_eq!(safe_name(".htaccess"), "htaccess");
    }

    #[test]
    fn save_text_roundtrip_and_rejects_blank() {
        let dir = std::env::temp_dir().join(format!("kuaichuan-text-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(dir.join("share")).unwrap();
        let hub = Hub::new(dir.join("share"), dir.join("meta.json"), "PC".into());
        assert!(hub.save_text("   \n", "这台电脑", "desktop").is_err());
        let item = hub.save_text("你好\n第二行", "这台电脑", "desktop").unwrap();
        assert_eq!(item.name, "文字.txt");
        assert_eq!(item.kind, "text");
        assert!(item.preview);
        assert_eq!(hub.read_text("文字.txt").unwrap(), "你好\n第二行");
        let again = hub.save_text("more", "这台电脑", "desktop").unwrap();
        assert_eq!(again.name, "文字 (2).txt");
        fs::write(dir.join("share").join("文字.txt"), [0xEF, 0xBB, 0xBF, b'A', b'B']).unwrap();
        assert_eq!(hub.read_text("文字.txt").unwrap(), "AB");
        fs::write(dir.join("share").join("照片.jpg"), b"not-text").unwrap();
        assert!(hub.read_text("照片.jpg").is_err());
        assert!(hub.read_text("../meta.json").is_err());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn unique_name_adds_a_suffix() {
        let dir = std::env::temp_dir().join(format!("kuaichuan-name-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("a.txt"), "x").unwrap();
        let mut reserved = HashSet::new();
        assert_eq!(unique_name(&dir, "a.txt", &reserved), "a (2).txt");
        reserved.insert("a (2).txt".into());
        assert_eq!(unique_name(&dir, "a.txt", &reserved), "a (3).txt");
        let _ = fs::remove_dir_all(&dir);
    }
}
