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
