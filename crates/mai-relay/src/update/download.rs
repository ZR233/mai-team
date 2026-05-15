use crate::error::{RelayErrorKind, RelayResult};
use flate2::read::GzDecoder;
use futures::StreamExt;
use reqwest::header::USER_AGENT;
use sha2::{Digest, Sha256};
use std::fs;
use std::io::Read;
use std::path::{Component, Path};
use tokio::io::AsyncWriteExt;

use super::{USER_AGENT_VALUE, release::SelectedReleaseAssets};

pub(super) async fn download_verified_binary(
    http: &reqwest::Client,
    assets: &SelectedReleaseAssets,
    archive_path: &Path,
    new_binary_path: &Path,
    max_archive_bytes: u64,
    max_checksum_bytes: u64,
) -> RelayResult<()> {
    download_to_file(http, &assets.archive_url, archive_path, max_archive_bytes).await?;
    let checksum_body = download_text(http, &assets.checksum_url, max_checksum_bytes).await?;
    let expected_checksum = parse_sha256_checksum(&checksum_body, super::RELAY_ASSET_NAME)?;
    verify_sha256(archive_path, &expected_checksum)?;
    extract_relay_binary(archive_path, new_binary_path, max_archive_bytes)
}

async fn download_to_file(
    http: &reqwest::Client,
    url: &str,
    path: &Path,
    max_bytes: u64,
) -> RelayResult<()> {
    let response = http
        .get(url)
        .header(USER_AGENT, USER_AGENT_VALUE)
        .send()
        .await?
        .error_for_status()?;
    if response
        .content_length()
        .is_some_and(|length| length > max_bytes)
    {
        return Err(RelayErrorKind::InvalidInput(
            "relay update download is larger than 100 MB".to_string(),
        ));
    }
    let mut stream = response.bytes_stream();
    let mut file = tokio::fs::File::create(path).await?;
    let mut received = 0_u64;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        received += chunk.len() as u64;
        if received > max_bytes {
            return Err(RelayErrorKind::InvalidInput(
                "relay update download is larger than 100 MB".to_string(),
            ));
        }
        file.write_all(&chunk).await?;
    }
    file.flush().await?;
    Ok(())
}

async fn download_text(http: &reqwest::Client, url: &str, max_bytes: u64) -> RelayResult<String> {
    let response = http
        .get(url)
        .header(USER_AGENT, USER_AGENT_VALUE)
        .send()
        .await?
        .error_for_status()?;
    if response
        .content_length()
        .is_some_and(|length| length > max_bytes)
    {
        return Err(RelayErrorKind::InvalidInput(
            "relay update checksum file is too large".to_string(),
        ));
    }
    let mut stream = response.bytes_stream();
    let mut received = 0_u64;
    let mut bytes = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        received += chunk.len() as u64;
        if received > max_bytes {
            return Err(RelayErrorKind::InvalidInput(
                "relay update checksum file is too large".to_string(),
            ));
        }
        bytes.extend_from_slice(&chunk);
    }
    String::from_utf8(bytes).map_err(|error| RelayErrorKind::InvalidInput(error.to_string()))
}

fn parse_sha256_checksum(body: &str, file_name: &str) -> RelayResult<String> {
    for line in body.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let mut fields = trimmed.split_whitespace();
        let Some(hash) = fields.next() else {
            continue;
        };
        let Some(name) = fields.next() else {
            continue;
        };
        if checksum_name_matches(name, file_name) {
            return validate_sha256(hash);
        }
    }
    Err(RelayErrorKind::InvalidInput(format!(
        "checksum file does not include {file_name}"
    )))
}

fn checksum_name_matches(value: &str, file_name: &str) -> bool {
    let value = value.trim_start_matches('*');
    value == file_name
        || Path::new(value)
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name == file_name)
}

fn validate_sha256(value: &str) -> RelayResult<String> {
    let normalized = value.to_ascii_lowercase();
    if normalized.len() == 64
        && normalized
            .chars()
            .all(|character| character.is_ascii_hexdigit())
    {
        Ok(normalized)
    } else {
        Err(RelayErrorKind::InvalidInput(
            "checksum file contains an invalid sha256".to_string(),
        ))
    }
}

fn verify_sha256(path: &Path, expected: &str) -> RelayResult<()> {
    let actual = sha256_file(path)?;
    if actual == expected {
        Ok(())
    } else {
        Err(RelayErrorKind::InvalidInput(
            "relay update checksum verification failed".to_string(),
        ))
    }
}

fn sha256_file(path: &Path) -> RelayResult<String> {
    let mut file = fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 8192];
    loop {
        let bytes_read = file.read(&mut buffer)?;
        if bytes_read == 0 {
            break;
        }
        hasher.update(&buffer[..bytes_read]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn extract_relay_binary(
    archive_path: &Path,
    output_path: &Path,
    max_entry_size: u64,
) -> RelayResult<()> {
    let archive_file = fs::File::open(archive_path)?;
    let decoder = GzDecoder::new(archive_file);
    let mut archive = tar::Archive::new(decoder);
    let mut found_binary = false;
    for entry in archive.entries()? {
        let mut entry = entry?;
        let path = entry.path()?.to_path_buf();
        validate_archive_path(&path)?;
        let size = entry.header().size()?;
        if size > max_entry_size {
            return Err(RelayErrorKind::InvalidInput(format!(
                "archive entry `{}` is larger than 100 MB",
                path.display()
            )));
        }
        if !is_relay_binary_entry(&path) {
            continue;
        }
        if !entry.header().entry_type().is_file() {
            return Err(RelayErrorKind::InvalidInput(
                "relay update archive entry is not a file".to_string(),
            ));
        }
        let mut output = fs::File::create(output_path)?;
        std::io::copy(&mut entry, &mut output)?;
        found_binary = true;
        break;
    }
    if found_binary {
        Ok(())
    } else {
        Err(RelayErrorKind::InvalidInput(
            "relay update archive does not include mai-relay".to_string(),
        ))
    }
}

fn validate_archive_path(path: &Path) -> RelayResult<()> {
    for component in path.components() {
        match component {
            Component::Normal(_) | Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(RelayErrorKind::InvalidInput(format!(
                    "relay update archive contains unsafe path `{}`",
                    path.display()
                )));
            }
        }
    }
    Ok(())
}

fn is_relay_binary_entry(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name == "mai-relay")
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::Compression;
    use flate2::write::GzEncoder;
    use pretty_assertions::assert_eq;
    use std::io::Write;

    #[test]
    fn parses_and_verifies_sha256_checksum() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let file_path = temp_dir.path().join(super::super::RELAY_ASSET_NAME);
        fs::write(&file_path, b"relay archive").expect("write archive");
        let checksum = sha256_file(&file_path).expect("checksum");
        let body = format!("{checksum}  {}\n", super::super::RELAY_ASSET_NAME);

        let parsed =
            parse_sha256_checksum(&body, super::super::RELAY_ASSET_NAME).expect("parse checksum");
        verify_sha256(&file_path, &parsed).expect("verify checksum");

        assert_eq!(parsed, checksum);
        assert!(verify_sha256(&file_path, &"0".repeat(64)).is_err());
    }

    #[test]
    fn archive_extraction_rejects_path_traversal() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let archive_path = temp_dir.path().join("bad.tar.gz");
        write_tar_gz_entry(&archive_path, "../mai-relay", b"bad").expect("write archive");
        let output_path = temp_dir.path().join("mai-relay");

        let error = extract_relay_binary(
            &archive_path,
            &output_path,
            super::super::MAX_DOWNLOAD_BYTES,
        )
        .expect_err("path traversal rejected");

        assert!(error.to_string().contains("unsafe path"));
        assert!(!output_path.exists());
    }

    fn write_tar_gz_entry(path: &Path, entry_path: &str, content: &[u8]) -> std::io::Result<()> {
        let file = fs::File::create(path)?;
        let encoder = GzEncoder::new(file, Compression::default());
        let mut builder = tar::Builder::new(encoder);
        write_raw_tar_entry(&mut builder, entry_path, content)?;
        builder.into_inner()?.finish()?;
        Ok(())
    }

    fn write_raw_tar_entry<W: Write>(
        builder: &mut tar::Builder<W>,
        entry_path: &str,
        content: &[u8],
    ) -> std::io::Result<()> {
        let mut header = [0_u8; 512];
        write_tar_bytes(&mut header[0..100], entry_path.as_bytes());
        write_tar_octal(&mut header[100..108], 0o755);
        write_tar_octal(&mut header[108..116], 0);
        write_tar_octal(&mut header[116..124], 0);
        write_tar_octal(&mut header[124..136], content.len() as u64);
        write_tar_octal(&mut header[136..148], 0);
        for byte in &mut header[148..156] {
            *byte = b' ';
        }
        header[156] = b'0';
        write_tar_bytes(&mut header[257..263], b"ustar\0");
        write_tar_bytes(&mut header[263..265], b"00");
        let checksum: u32 = header.iter().map(|byte| *byte as u32).sum();
        write_tar_octal(&mut header[148..156], checksum as u64);
        builder.get_mut().write_all(&header)?;
        builder.get_mut().write_all(content)?;
        let padding = (512 - (content.len() % 512)) % 512;
        if padding > 0 {
            builder.get_mut().write_all(&vec![0_u8; padding])?;
        }
        builder.get_mut().write_all(&[0_u8; 1024])?;
        Ok(())
    }

    fn write_tar_bytes(field: &mut [u8], value: &[u8]) {
        let len = field.len().min(value.len());
        field[..len].copy_from_slice(&value[..len]);
    }

    fn write_tar_octal(field: &mut [u8], value: u64) {
        let text = format!("{value:0width$o}\0", width = field.len() - 1);
        write_tar_bytes(field, text.as_bytes());
    }
}
