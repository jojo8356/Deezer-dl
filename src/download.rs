use anyhow::{bail, Context, Result};
use futures_util::StreamExt;
use indicatif::{ProgressBar, ProgressStyle};
use std::path::{Path, PathBuf};
use tokio::fs;
use tokio::io::AsyncWriteExt;

use crate::api::DeezerApi;
use crate::crypto;
use crate::models::*;

/// Sanitize a filename by removing/replacing invalid characters
fn sanitize_filename(name: &str) -> String {
    name.chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            _ => c,
        })
        .collect::<String>()
        .trim()
        .to_string()
}

/// Get a download URL for a track at the preferred format, with fallback
async fn get_download_url(
    api: &DeezerApi,
    track: &GwTrack,
    format: TrackFormat,
) -> Result<(String, TrackFormat, bool)> {
    let current_format = format;
    let is_crypted;

    // Try the new media API first
    if let Some(token) = &track.track_token {
        if !token.is_empty() {
            if let Ok(Some(url)) = api.get_track_url(token, current_format.api_name()).await {
                return Ok((url, current_format, true));
            }
            // Fallback formats with new API
            let mut fallback = current_format.fallback();
            while let Some(fb) = fallback {
                if let Ok(Some(url)) = api.get_track_url(token, fb.api_name()).await {
                    return Ok((url, fb, true));
                }
                fallback = fb.fallback();
            }
        }
    }

    // Fallback to legacy URL generation
    let md5 = track.md5();
    let media_version = track.media_ver();
    let sng_id = track.id_str();

    if md5.is_empty() {
        bail!("Track has no MD5, cannot generate download URL");
    }

    // Try preferred format first
    let mut try_format = Some(current_format);
    while let Some(fmt) = try_format {
        if track.filesize_for_format(fmt) > 0 {
            let url = crypto::generate_crypted_stream_url(&sng_id, &md5, &media_version, fmt.code());
            return Ok((url, fmt, true));
        }
        try_format = fmt.fallback();
    }

    // Last resort: try the preferred format anyway
    let url = crypto::generate_crypted_stream_url(&sng_id, &md5, &media_version, current_format.code());
    is_crypted = true;
    Ok((url, current_format, is_crypted))
}

/// Download and decrypt a single track
pub async fn download_track(
    api: &DeezerApi,
    track: &GwTrack,
    format: TrackFormat,
    output_dir: &Path,
    show_progress: bool,
) -> Result<PathBuf> {
    let artist = sanitize_filename(&track.artist());
    let title = sanitize_filename(&track.title());
    let sng_id = track.id_str();

    if sng_id == "0" || title.is_empty() {
        bail!("Invalid track data");
    }

    // Get download URL
    let (url, actual_format, is_crypted) = get_download_url(api, track, format).await?;
    let extension = actual_format.extension();

    // Create output directory
    let track_dir = output_dir.join(sanitize_filename(&artist));
    fs::create_dir_all(&track_dir).await?;

    let filename = format!("{} - {}{}", artist, title, extension);
    let filepath = track_dir.join(&filename);

    // Skip if already exists
    if filepath.exists() {
        if show_progress {
            println!("  [skip] {} (already exists)", filename);
        }
        return Ok(filepath);
    }

    // Download
    let client = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .build()?;

    let response = client
        .get(&url)
        .header("User-Agent", "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/79.0.3945.130 Safari/537.36")
        .send()
        .await
        .context("Failed to download track")?;

    if !response.status().is_success() {
        bail!("Download failed with status: {}", response.status());
    }

    let total_size = response.content_length().unwrap_or(0);

    let pb = if show_progress && total_size > 0 {
        let pb = ProgressBar::new(total_size);
        pb.set_style(
            ProgressStyle::default_bar()
                .template("  [{bar:40.cyan/blue}] {bytes}/{total_bytes} ({eta})")
                .unwrap()
                .progress_chars("##-"),
        );
        Some(pb)
    } else {
        None
    };

    // Download to memory (needed for decryption)
    let mut data = Vec::with_capacity(total_size as usize);
    let mut stream = response.bytes_stream();

    while let Some(chunk) = stream.next().await {
        let chunk = chunk.context("Error reading download stream")?;
        if let Some(ref pb) = pb {
            pb.inc(chunk.len() as u64);
        }
        data.extend_from_slice(&chunk);
    }

    if let Some(pb) = pb {
        pb.finish_and_clear();
    }

    if data.is_empty() {
        bail!("Downloaded file is empty");
    }

    // Decrypt if needed
    let final_data = if is_crypted {
        let blowfish_key = crypto::generate_blowfish_key(&sng_id);
        crypto::decrypt_stream(&data, &blowfish_key)
    } else {
        data
    };

    // Remove leading null bytes (depadding) - but not for ftyp (MP4)
    let output_data = if !final_data.is_empty() && final_data[0] == 0 {
        if final_data.len() > 8 && &final_data[4..8] == b"ftyp" {
            final_data
        } else {
            let start = final_data.iter().position(|&b| b != 0).unwrap_or(0);
            final_data[start..].to_vec()
        }
    } else {
        final_data
    };

    // Write to file
    let mut file = tokio::fs::File::create(&filepath).await?;
    file.write_all(&output_data).await?;
    file.flush().await?;

    Ok(filepath)
}

/// Download a playlist by ID
pub async fn download_playlist(
    api: &DeezerApi,
    playlist_id: &str,
    format: TrackFormat,
    output_dir: &Path,
) -> Result<()> {
    // Get playlist info
    let info = api.get_playlist_info(playlist_id).await?;
    let playlist_name = info["DATA"]["TITLE"]
        .as_str()
        .unwrap_or("Unknown Playlist");
    let playlist_dir = output_dir.join(sanitize_filename(playlist_name));

    println!("Downloading playlist: {}\n", playlist_name);

    // Get tracks
    let tracks = api.get_playlist_tracks(playlist_id).await?;
    let total = tracks.len();

    println!("Found {} tracks\n", total);

    let mut downloaded = 0;
    let mut failed = 0;

    for (i, track) in tracks.iter().enumerate() {
        let display = track.display_name();
        println!("[{}/{}] {}", i + 1, total, display);

        match download_track(api, track, format, &playlist_dir, true).await {
            Ok(_) => {
                downloaded += 1;
                println!("  [ok] Downloaded successfully");
            }
            Err(e) => {
                failed += 1;
                eprintln!("  [err] Failed: {}", e);
            }
        }
    }

    println!(
        "\nPlaylist complete: {} downloaded, {} failed out of {} tracks",
        downloaded, failed, total
    );
    Ok(())
}

/// Download user's favorite (liked) tracks
pub async fn download_favorites(
    api: &DeezerApi,
    format: TrackFormat,
    output_dir: &Path,
) -> Result<()> {
    println!("Fetching favorite tracks...\n");

    let ids = api.get_favorite_track_ids().await?;
    if ids.is_empty() {
        println!("No favorite tracks found.");
        return Ok(());
    }

    println!("Found {} favorite tracks\n", ids.len());

    // Fetch track data in batches
    let favorites_dir = output_dir.join("Favorites");
    let total = ids.len();
    let mut downloaded = 0;
    let mut failed = 0;

    // Process in batches of 50
    for (batch_start, batch) in ids.chunks(50).enumerate() {
        let batch_ids: Vec<String> = batch.to_vec();
        let tracks = api.get_tracks_by_ids(&batch_ids).await?;

        for (j, track) in tracks.iter().enumerate() {
            let i = batch_start * 50 + j + 1;
            let display = track.display_name();
            println!("[{}/{}] {}", i, total, display);

            match download_track(api, track, format, &favorites_dir, true).await {
                Ok(_) => {
                    downloaded += 1;
                    println!("  [ok] Downloaded successfully");
                }
                Err(e) => {
                    failed += 1;
                    eprintln!("  [err] Failed: {}", e);
                }
            }
        }
    }

    println!(
        "\nFavorites complete: {} downloaded, {} failed out of {} tracks",
        downloaded, failed, total
    );
    Ok(())
}

/// Download all tracks from an artist
pub async fn download_artist(
    api: &DeezerApi,
    art_id: &str,
    format: TrackFormat,
    output_dir: &Path,
) -> Result<()> {
    let artist_info = api.get_artist_info(art_id).await?;
    let artist_name = artist_info["ART_NAME"]
        .as_str()
        .unwrap_or("Unknown Artist");

    println!("Fetching discography for: {}\n", artist_name);

    let albums = api.get_artist_discography(art_id).await?;
    if albums.is_empty() {
        println!("No albums found for this artist.");
        return Ok(());
    }

    println!("Found {} albums/releases\n", albums.len());

    let artist_dir = output_dir.join(sanitize_filename(artist_name));
    let mut total_downloaded = 0;
    let mut total_failed = 0;

    for album in &albums {
        let alb_id = album.id_str();
        let album_title = album.alb_title.as_deref().unwrap_or("Unknown Album");
        let album_dir = artist_dir.join(sanitize_filename(album_title));

        println!("--- Album: {} ---", album_title);

        let tracks = match api.get_album_tracks(&alb_id).await {
            Ok(t) => t,
            Err(e) => {
                eprintln!("  [err] Failed to get album tracks: {}", e);
                total_failed += 1;
                continue;
            }
        };

        for (i, track) in tracks.iter().enumerate() {
            let display = track.display_name();
            println!("  [{}/{}] {}", i + 1, tracks.len(), display);

            match download_track(api, track, format, &album_dir, true).await {
                Ok(_) => {
                    total_downloaded += 1;
                    println!("    [ok] Downloaded");
                }
                Err(e) => {
                    total_failed += 1;
                    eprintln!("    [err] Failed: {}", e);
                }
            }
        }
    }

    println!(
        "\nArtist download complete: {} downloaded, {} failed",
        total_downloaded, total_failed
    );
    Ok(())
}

/// Download a single track by URL or ID
pub async fn download_single_track(
    api: &DeezerApi,
    track_id: &str,
    format: TrackFormat,
    output_dir: &Path,
) -> Result<()> {
    println!("Fetching track info...\n");

    let track = api.get_track(track_id).await?;
    let display = track.display_name();
    println!("Downloading: {}\n", display);

    match download_track(api, &track, format, output_dir, true).await {
        Ok(path) => {
            println!("\nSaved to: {}", path.display());
        }
        Err(e) => {
            eprintln!("\nFailed to download: {}", e);
        }
    }

    Ok(())
}
