//! Binary-on-disk attachment store with explicit SQLite ACL grants (zero unbounded RAM cache).
use crate::{auth::AuthUser, config::now_unix, denied, invalid, Error, Hub, Result};
use axum::{
    extract::{Path, State},
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use base64::{engine::general_purpose::STANDARD, Engine};
use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::io::Write;
use uuid::Uuid;

pub const MAX_ATTACHMENT_BYTES: usize = 5 * 1024 * 1024;
const MAX_BASE64_LEN: usize = 7_000_000;

#[derive(Clone, Debug, Serialize)]
pub struct AttachmentMeta {
    pub id: String,
    pub name: String,
    pub mime_type: String,
    pub size_bytes: usize,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UploadInput {
    pub name: String,
    pub mime_type: String,
    pub data_base64: String,
}

pub fn valid_attachment_id(id: &str) -> bool {
    id.starts_with("att_") && id.len() == 36 && id[4..].bytes().all(|b| b.is_ascii_hexdigit())
}

fn normalize_mime(mime: &str) -> &str {
    match mime.trim() {
        "application/x-zip-compressed" => "application/zip",
        other => other,
    }
}

fn valid_magic(mime: &str, bytes: &[u8]) -> bool {
    match normalize_mime(mime) {
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
            !bytes.contains(&0) && std::str::from_utf8(bytes).is_ok()
        }
        _ => false,
    }
}

fn sanitize_name(name: &str, mime: &str) -> String {
    let cleaned: String = name
        .trim()
        .chars()
        .filter(|c| !c.is_control() && !matches!(*c, '/' | '\\' | '[' | ']' | '(' | ')' | '"'))
        .take(80)
        .collect();
    if !cleaned.trim().is_empty() {
        return cleaned.trim().to_owned();
    }
    match normalize_mime(mime) {
        "image/jpeg" => "image.jpg".into(),
        "image/webp" => "image.webp".into(),
        "image/gif" => "image.gif".into(),
        "application/pdf" => "document.pdf".into(),
        "application/zip" => "archive.zip".into(),
        "text/markdown" => "notes.md".into(),
        "text/csv" => "data.csv".into(),
        "application/json" => "data.json".into(),
        "text/plain" => "notes.txt".into(),
        _ => "file.png".into(),
    }
}

pub async fn store_bytes(
    hub: &Hub,
    owner_id: &str,
    name: &str,
    mime_type: &str,
    data_base64: &str,
    extra_grantees: &[String],
) -> Result<AttachmentMeta> {
    if data_base64.len() > MAX_BASE64_LEN {
        return Err(invalid("attachment exceeds 5 MiB limit"));
    }
    let canonical_mime = normalize_mime(mime_type).to_owned();
    let bytes = STANDARD
        .decode(data_base64.trim())
        .map_err(|_| invalid("invalid base64 attachment"))?;
    if bytes.is_empty()
        || bytes.len() > MAX_ATTACHMENT_BYTES
        || !valid_magic(&canonical_mime, &bytes)
    {
        return Err(invalid(
            "attachment must be a valid PNG/JPEG/WEBP/GIF/PDF/ZIP/UTF-8 text file up to 5 MiB",
        ));
    }

    let id = format!("att_{}", Uuid::new_v4().simple());
    let clean_name = sanitize_name(name, &canonical_mime);
    let size = bytes.len();
    let dir = hub.attachments_dir.clone();
    let id_for_disk = id.clone();

    tokio::task::spawn_blocking(move || -> Result<()> {
        std::fs::create_dir_all(&dir).map_err(|_| {
            Error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "attachment directory error",
            )
        })?;
        let path = dir.join(format!("{id_for_disk}.bin"));
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut fh = opts
            .open(&path)
            .map_err(|_| Error(StatusCode::INTERNAL_SERVER_ERROR, "attachment write error"))?;
        fh.write_all(&bytes)
            .map_err(|_| Error(StatusCode::INTERNAL_SERVER_ERROR, "attachment write error"))?;
        Ok(())
    })
    .await
    .map_err(|_| Error(StatusCode::INTERNAL_SERVER_ERROR, "attachment io failed"))??;

    let owner = owner_id.to_owned();
    let id_db = id.clone();
    let name_db = clean_name.clone();
    let mime_db = canonical_mime.clone();
    let grants: Vec<String> = extra_grantees.to_vec();

    let db_res = hub
        .db
        .write(move |conn| {
            let tx = conn.transaction()?;
            tx.execute(
                "INSERT INTO attachments(id, owner_id, name, mime_type, size_bytes, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![id_db, owner, name_db, mime_db, size as i64, now_unix()],
            )?;
            tx.execute(
                "INSERT OR IGNORE INTO attachment_grants(attachment_id, user_id) VALUES (?1, ?2)",
                params![id_db, owner],
            )?;
            for grantee in grants {
                tx.execute(
                    "INSERT OR IGNORE INTO attachment_grants(attachment_id, user_id) VALUES (?1, ?2)",
                    params![id_db, grantee],
                )?;
            }
            tx.commit()?;
            Ok(())
        })
        .await;
    if let Err(err) = db_res {
        let orphan = hub.attachments_dir.join(format!("{id}.bin"));
        let _ = tokio::task::spawn_blocking(move || std::fs::remove_file(orphan)).await;
        return Err(err);
    }

    Ok(AttachmentMeta {
        id,
        name: clean_name,
        mime_type: canonical_mime,
        size_bytes: size,
    })
}

pub async fn grant_in_text(hub: &Hub, owner_id: &str, grantee_id: &str, text: &str) -> Result<()> {
    let ids = extract_attachment_ids(text);
    if ids.is_empty() || owner_id == grantee_id {
        return Ok(());
    }
    let owner = owner_id.to_owned();
    let grantee = grantee_id.to_owned();
    hub.db
        .write(move |conn| {
            for att_id in ids {
                let owned: bool = conn.query_row(
                    "SELECT EXISTS(SELECT 1 FROM attachments WHERE id=?1 AND owner_id=?2)",
                    params![att_id, owner],
                    |r| r.get(0),
                )?;
                if owned {
                    conn.execute(
                        "INSERT OR IGNORE INTO attachment_grants(attachment_id, user_id) VALUES (?1, ?2)",
                        params![att_id, grantee],
                    )?;
                }
            }
            Ok(())
        })
        .await
}

pub fn extract_attachment_ids(text: &str) -> Vec<String> {
    let bytes = text.as_bytes();
    let mut ids = Vec::new();
    let mut i = 0;
    while i + 36 <= bytes.len() {
        if &bytes[i..i + 4] == b"att_" {
            if let Some(cand) = text.get(i..i + 36) {
                if valid_attachment_id(cand) && !ids.iter().any(|x| x == cand) {
                    ids.push(cand.to_owned());
                    if ids.len() >= 4 {
                        break;
                    }
                    i += 36;
                    continue;
                }
            }
        }
        i += 1;
    }
    ids
}

pub async fn collect_for_user(hub: &Hub, user_id: &str, text: &str) -> Result<Vec<Value>> {
    let ids = extract_attachment_ids(text);
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let uid = user_id.to_owned();
    let metas: Vec<(String, String, String)> = hub
        .db
        .read(move |conn| {
            let mut out = Vec::new();
            for id in ids {
                let row = conn
                    .query_row(
                        "SELECT a.id, a.name, a.mime_type
                         FROM attachments a
                         JOIN attachment_grants g ON g.attachment_id = a.id
                         WHERE a.id = ?1 AND g.user_id = ?2",
                        params![id, uid],
                        |r| {
                            Ok((
                                r.get::<_, String>(0)?,
                                r.get::<_, String>(1)?,
                                r.get::<_, String>(2)?,
                            ))
                        },
                    )
                    .optional()?;
                if let Some(m) = row {
                    out.push(m);
                }
            }
            Ok(out)
        })
        .await?;

    let dir = hub.attachments_dir.clone();
    tokio::task::spawn_blocking(move || {
        let mut items = Vec::new();
        for (id, name, mime_type) in metas {
            if let Ok(bytes) = std::fs::read(dir.join(format!("{id}.bin"))) {
                items.push(json!({
                    "id": id,
                    "name": name,
                    "mime_type": mime_type,
                    "data_base64": STANDARD.encode(bytes),
                }));
            }
        }
        Ok(items)
    })
    .await
    .map_err(|_| Error(StatusCode::INTERNAL_SERVER_ERROR, "attachment load failed"))?
}

pub async fn upload(
    State(hub): State<Hub>,
    AuthUser(user): AuthUser,
    Json(input): Json<UploadInput>,
) -> Result<Json<AttachmentMeta>> {
    let meta = store_bytes(
        &hub,
        &user.id,
        &input.name,
        &input.mime_type,
        &input.data_base64,
        &[],
    )
    .await?;
    Ok(Json(meta))
}

pub async fn download(
    State(hub): State<Hub>,
    AuthUser(user): AuthUser,
    Path(id): Path<String>,
) -> Result<Response> {
    if !valid_attachment_id(&id) {
        return Err(denied());
    }
    let id_clone = id.clone();
    let uid = user.id;
    let meta: Option<(String, String)> = hub
        .db
        .read(move |conn| {
            Ok(conn
                .query_row(
                    "SELECT a.name, a.mime_type
                     FROM attachments a
                     JOIN attachment_grants g ON g.attachment_id = a.id
                     WHERE a.id = ?1 AND g.user_id = ?2",
                    params![id_clone, uid],
                    |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
                )
                .optional()?)
        })
        .await?;
    let (name, mime_type) = meta.ok_or_else(denied)?;

    let path = hub.attachments_dir.join(format!("{id}.bin"));
    let bytes = tokio::task::spawn_blocking(move || std::fs::read(&path))
        .await
        .map_err(|_| Error(StatusCode::INTERNAL_SERVER_ERROR, "attachment read error"))?
        .map_err(|_| denied())?;

    let disposition_kind = if mime_type.starts_with("image/") {
        "inline"
    } else {
        "attachment"
    };
    let disposition = format!(
        "{disposition_kind}; filename=\"{}\"",
        name.replace('"', "_")
    );
    Ok((
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, mime_type),
            (header::CONTENT_DISPOSITION, disposition),
            (
                header::CONTENT_SECURITY_POLICY,
                "default-src 'none'; sandbox".to_owned(),
            ),
        ],
        bytes,
    )
        .into_response())
}
