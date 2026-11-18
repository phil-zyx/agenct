//! Local conversion only. The requester must still review and confirm shared text.
use super::*;
use base64::{engine::general_purpose::STANDARD, Engine};
use std::io::{Read, Write};
use std::process::{Command, Stdio};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct PdfInput {
    pdf_base64: String,
}

pub(super) async fn extract(
    State(h): State<Hub>,
    headers: HeaderMap,
    Json(input): Json<PdfInput>,
) -> Result<Json<serde_json::Value>> {
    let user = h.identity(&headers)?;
    if input.pdf_base64.len() > 2_796_204 {
        return Err(invalid("PDF must be at most 2 MiB"));
    }
    let bytes = STANDARD
        .decode(input.pdf_base64)
        .map_err(|_| invalid("invalid PDF encoding"))?;
    if bytes.len() > 2 * 1024 * 1024 || !bytes.starts_with(b"%PDF-") {
        return Err(invalid("PDF must be valid and at most 2 MiB"));
    }
    let gate = h
        .gate(&user)?
        .try_lock_owned()
        .map_err(|_| Error(StatusCode::CONFLICT, "another operation is running"))?;
    let result = tokio::task::spawn_blocking(move || {
        let _guard = gate;
        let mut process = Command::new("python3")
            .arg("-I").arg("-c").arg(include_str!("../../scripts/extract-pdf.py"))
            .env_clear().env("PATH", "/usr/bin:/bin")
            .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null())
            .spawn().map_err(|_| invalid("PDF extraction unavailable; install Python 3 and poppler-utils"))?;
        let written = process.stdin.take().unwrap().write_all(&bytes);
        let mut output = Vec::new();
        let read = process.stdout.take().unwrap().take(262145).read_to_end(&mut output);
        if written.is_err() || read.is_err() || output.len() > 262144 {
            let _ = process.kill(); let _ = process.wait();
            return Err(invalid("PDF extraction failed or exceeds 256 KiB"));
        }
        let status = process.wait().map_err(|_| invalid("PDF extraction failed"))?;
        let text = String::from_utf8(output).map_err(|_| invalid("PDF extraction failed"))?;
        if !status.success() || text.trim().is_empty() {
            return Err(invalid("PDF conversion unavailable, invalid, too large, or contains no text; install poppler-utils and use a text PDF"));
        }
        Ok(text)
    }).await.map_err(|_| invalid("PDF extraction interrupted"))??;
    Ok(Json(
        serde_json::json!({"bytes":result.len(),"text":result}),
    ))
}

const MAX_ATTACHMENT_BYTES: usize = 5 * 1024 * 1024;
const MAX_ATTACHMENT_BASE64_LEN: usize = 7_000_000;

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct StoredAttachment {
    pub id: String,
    pub owner: String,
    pub name: String,
    pub mime_type: String,
    pub data_base64: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct AttachmentInput {
    name: String,
    mime_type: String,
    data_base64: String,
}

fn valid_attachment_id(id: &str) -> bool {
    id.starts_with("att_") && id.len() == 36 && id[4..].bytes().all(|b| b.is_ascii_hexdigit())
}

fn normalize_attachment_mime(mime_type: &str) -> &str {
    match mime_type.trim() {
        "application/x-zip-compressed" => "application/zip",
        other => other,
    }
}

fn valid_attachment_bytes(mime_type: &str, bytes: &[u8]) -> bool {
    match normalize_attachment_mime(mime_type) {
        "image/png" => bytes.starts_with(b"\x89PNG\r\n\x1a\n"),
        "image/jpeg" => bytes.starts_with(b"\xff\xd8\xff"),
        "image/gif" => bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a"),
        "image/webp" => bytes.len() >= 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP",
        "application/pdf" => bytes.starts_with(b"%PDF-"),
        "application/zip" => {
            bytes.starts_with(b"PK\x03\x04")
                || bytes.starts_with(b"PK\x05\x06")
                || bytes.starts_with(b"PK\x07\x08")
        }
        "text/plain" | "text/markdown" | "text/csv" | "application/json" => {
            !bytes.contains(&0)
                && std::str::from_utf8(bytes).is_ok()
                && !bytes.iter().all(u8::is_ascii_whitespace)
        }
        _ => false,
    }
}

fn sanitize_filename(name: &str, mime_type: &str) -> String {
    let cleaned: String = name
        .trim()
        .chars()
        .filter(|c| {
            !c.is_control()
                && *c != '/'
                && *c != '\\'
                && *c != '['
                && *c != ']'
                && *c != '('
                && *c != ')'
        })
        .take(80)
        .collect();
    let cleaned = cleaned.trim();
    if !cleaned.is_empty() {
        return cleaned.to_owned();
    }
    match normalize_attachment_mime(mime_type) {
        "image/jpeg" => "image.jpg".into(),
        "image/webp" => "image.webp".into(),
        "image/gif" => "image.gif".into(),
        "application/pdf" => "document.pdf".into(),
        "application/zip" => "archive.zip".into(),
        "text/markdown" => "notes.md".into(),
        "text/csv" => "data.csv".into(),
        "application/json" => "data.json".into(),
        "text/plain" => "notes.txt".into(),
        _ => "image.png".into(),
    }
}

pub(crate) fn store_attachment_raw(
    h: &Hub,
    owner: &str,
    name: &str,
    mime_type: &str,
    data_base64: &str,
) -> Result<(StoredAttachment, usize)> {
    let canonical_mime = normalize_attachment_mime(mime_type);
    if data_base64.len() > MAX_ATTACHMENT_BASE64_LEN {
        return Err(invalid("attachment must be at most 5 MiB"));
    }
    let bytes = STANDARD
        .decode(data_base64.trim())
        .map_err(|_| invalid("invalid attachment encoding"))?;
    if bytes.is_empty()
        || bytes.len() > MAX_ATTACHMENT_BYTES
        || !valid_attachment_bytes(canonical_mime, &bytes)
    {
        return Err(invalid(
            "attachment must be a valid PNG, JPEG, WEBP, GIF, PDF, ZIP, or UTF-8 text file up to 5 MiB",
        ));
    }
    let canonical_b64 = STANDARD.encode(&bytes);
    let id = format!("att_{}", Uuid::new_v4().simple());
    let clean_name = sanitize_filename(name, canonical_mime);
    let stored = StoredAttachment {
        id: id.clone(),
        owner: owner.to_owned(),
        name: clean_name,
        mime_type: canonical_mime.to_owned(),
        data_base64: canonical_b64,
    };
    if let Some(dir) = &h.attachments_dir {
        std::fs::create_dir_all(dir).map_err(|_| {
            Error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "attachment storage error",
            )
        })?;
        let file_path = dir.join(format!("{id}.json"));
        let json_bytes = serde_json::to_vec(&stored).map_err(|_| {
            Error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "attachment serialization error",
            )
        })?;
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut fh = opts.open(&file_path).map_err(|_| {
            Error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "attachment storage error",
            )
        })?;
        fh.write_all(&json_bytes).map_err(|_| {
            Error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "attachment storage error",
            )
        })?;
    }
    if let Ok(mut guard) = h.attachments.lock() {
        guard.insert(id, stored.clone());
    }
    Ok((stored, bytes.len()))
}

pub(super) async fn upload_attachment(
    State(h): State<Hub>,
    headers: HeaderMap,
    Json(input): Json<AttachmentInput>,
) -> Result<Json<serde_json::Value>> {
    let owner = h.identity(&headers)?;
    let (stored, len) = store_attachment_raw(
        &h,
        &owner,
        &input.name,
        &input.mime_type,
        &input.data_base64,
    )?;
    Ok(Json(serde_json::json!({
        "id": stored.id,
        "name": stored.name,
        "mime_type": stored.mime_type,
        "bytes": len,
    })))
}

fn load_attachment(h: &Hub, id: &str) -> Option<StoredAttachment> {
    if !valid_attachment_id(id) {
        return None;
    }
    if let Ok(guard) = h.attachments.lock() {
        if let Some(att) = guard.get(id) {
            return Some(att.clone());
        }
    }
    let dir = h.attachments_dir.as_ref()?;
    let raw = std::fs::read(dir.join(format!("{id}.json"))).ok()?;
    let att: StoredAttachment = serde_json::from_slice(&raw).ok()?;
    if att.id != id {
        return None;
    }
    if let Ok(mut guard) = h.attachments.lock() {
        guard.insert(id.to_owned(), att.clone());
    }
    Some(att)
}

pub(super) async fn get_attachment(
    State(h): State<Hub>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>> {
    let user = h.identity(&headers)?;
    let att = load_attachment(&h, &id).ok_or_else(denied)?;
    if att.owner != user {
        let db = h.db()?;
        let shared: bool = db.query_row(
            "SELECT EXISTS(
                SELECT 1 FROM tasks t
                WHERE (t.requester=?1 OR t.owner=?1)
                  AND (
                    instr(t.summary, ?2) > 0
                    OR EXISTS(SELECT 1 FROM deliveries d WHERE d.task_id=t.id AND instr(d.content, ?2) > 0)
                    OR EXISTS(SELECT 1 FROM task_details td WHERE td.task_id=t.id AND instr(td.content, ?2) > 0)
                    OR EXISTS(SELECT 1 FROM node_assignments a WHERE a.task_id=t.id AND instr(a.instruction, ?2) > 0)
                  )
            )
            OR EXISTS(
                SELECT 1 FROM contact_messages cm
                WHERE (cm.sender=?1 OR cm.recipient=?1) AND instr(cm.body, ?2) > 0
            )
            OR EXISTS(
                SELECT 1 FROM messages m
                WHERE m.owner=?1 AND instr(m.content, ?2) > 0
            )",
            params![user, id],
            |r| r.get(0),
        )?;
        if !shared {
            return Err(denied());
        }
    }
    Ok(Json(serde_json::json!({
        "id": att.id,
        "name": att.name,
        "mime_type": att.mime_type,
        "data_base64": att.data_base64,
    })))
}

pub(crate) fn collect_attachments(
    h: &Hub,
    owner: &str,
    task_requester: Option<&str>,
    texts: &[&str],
) -> Vec<serde_json::Value> {
    let mut ids: Vec<String> = Vec::new();
    for text in texts {
        let bytes = text.as_bytes();
        let mut i = 0;
        while i + 36 <= bytes.len() {
            if &bytes[i..i + 4] == b"att_" {
                let candidate = &text[i..i + 36];
                if valid_attachment_id(candidate) && !ids.iter().any(|x| x == candidate) {
                    ids.push(candidate.to_owned());
                    if ids.len() >= 4 {
                        break;
                    }
                    i += 36;
                    continue;
                }
            }
            i += 1;
        }
        if ids.len() >= 4 {
            break;
        }
    }
    let mut out = Vec::new();
    for id in ids {
        if let Some(att) = load_attachment(h, &id) {
            if att.owner == owner || task_requester == Some(att.owner.as_str()) {
                out.push(serde_json::json!({
                    "id": att.id,
                    "name": att.name,
                    "mime_type": att.mime_type,
                    "data_base64": att.data_base64,
                }));
            }
        }
    }
    out
}
