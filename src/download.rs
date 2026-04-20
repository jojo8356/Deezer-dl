use anyhow::{bail, Context, Result};
use futures_util::{stream, StreamExt};
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use std::path::{Path, PathBuf};
use tokio::fs;
use tokio::io::AsyncWriteExt;

use crate::api::DeezerApi;
use crate::crypto;
use crate::models::*;

pub enum TrackOutcome {
    Downloaded(PathBuf),
    Skipped(PathBuf),
}

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
) -> Result<TrackOutcome> {
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
        return Ok(TrackOutcome::Skipped(filepath));
    }

    // Download
    let response = api.client()
        .get(&url)
        .header("User-Agent", "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/79.0.3945.130 Safari/537.36")
        .send()
        .await
        .context("Failed to download track")?;

    if !response.status().is_success() {
        bail!("Download failed with status: {}", response.status());
    }

    let total_size = response.content_length().unwrap_or(0);

    // Download to memory (needed for decryption)
    let mut data = Vec::with_capacity(total_size as usize);
    let mut stream = response.bytes_stream();

    while let Some(chunk) = stream.next().await {
        let chunk = chunk.context("Error reading download stream")?;
        data.extend_from_slice(&chunk);
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

    Ok(TrackOutcome::Downloaded(filepath))
}

/// Download a playlist by ID
pub async fn download_playlist(
    api: &DeezerApi,
    playlist_id: &str,
    format: TrackFormat,
    output_dir: &Path,
    concurrency: usize,
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
    let mut skipped = 0;
    let mut failed = 0;

    let pb = ProgressBar::new(total as u64);
    pb.set_style(
        ProgressStyle::default_bar()
            .template("  {bar:50.cyan/blue} {pos}/{len} tracks")
            .unwrap()
            .progress_chars("##-"),
    );

    let mut dl_stream = stream::iter(tracks)
        .map(|track| {
            let dir = playlist_dir.clone();
            async move {
                let display = track.display_name();
                let result = download_track(api, &track, format, &dir).await;
                (display, result)
            }
        })
        .buffer_unordered(concurrency);

    while let Some((display, result)) = dl_stream.next().await {
        match result {
            Ok(TrackOutcome::Downloaded(_)) => {
                downloaded += 1;
                pb.println(format!("  [ok]   {}", display));
            }
            Ok(TrackOutcome::Skipped(_)) => {
                skipped += 1;
                pb.println(format!("  [skip] {}", display));
            }
            Err(e) => {
                failed += 1;
                pb.println(format!("  [err]  {} — {}", display, e));
            }
        }
        pb.inc(1);
    }
    pb.finish_and_clear();

    println!(
        "\nPlaylist complete: {} downloaded, {} skipped, {} failed out of {} tracks",
        downloaded, skipped, failed, total
    );
    Ok(())
}

/// Download user's favorite (liked) tracks
pub async fn download_favorites(
    api: &DeezerApi,
    format: TrackFormat,
    output_dir: &Path,
    concurrency: usize,
) -> Result<()> {
    println!("Fetching favorite tracks...\n");

    let ids = api.get_favorite_track_ids().await?;
    if ids.is_empty() {
        println!("No favorite tracks found.");
        return Ok(());
    }

    println!("Found {} favorite tracks\n", ids.len());

    // Fetch all track metadata in batches of 50
    let mut all_tracks: Vec<GwTrack> = Vec::with_capacity(ids.len());
    for batch in ids.chunks(50) {
        let tracks = api.get_tracks_by_ids(batch).await?;
        all_tracks.extend(tracks);
    }

    let favorites_dir = output_dir.join("Favorites");
    let total = all_tracks.len();
    let mut downloaded = 0;
    let mut skipped = 0;
    let mut failed = 0;

    let pb = ProgressBar::new(total as u64);
    pb.set_style(
        ProgressStyle::default_bar()
            .template("  {bar:50.cyan/blue} {pos}/{len} tracks")
            .unwrap()
            .progress_chars("##-"),
    );

    let mut dl_stream = stream::iter(all_tracks)
        .map(|track| {
            let dir = favorites_dir.clone();
            async move {
                let display = track.display_name();
                let result = download_track(api, &track, format, &dir).await;
                (display, result)
            }
        })
        .buffer_unordered(concurrency);

    while let Some((display, result)) = dl_stream.next().await {
        match result {
            Ok(TrackOutcome::Downloaded(_)) => {
                downloaded += 1;
                pb.println(format!("  [ok]   {}", display));
            }
            Ok(TrackOutcome::Skipped(_)) => {
                skipped += 1;
                pb.println(format!("  [skip] {}", display));
            }
            Err(e) => {
                failed += 1;
                pb.println(format!("  [err]  {} — {}", display, e));
            }
        }
        pb.inc(1);
    }
    pb.finish_and_clear();

    println!(
        "\nFavorites complete: {} downloaded, {} skipped, {} failed out of {} tracks",
        downloaded, skipped, failed, total
    );
    Ok(())
}

/// Download all tracks from an artist
pub async fn download_artist(
    api: &DeezerApi,
    art_id: &str,
    format: TrackFormat,
    output_dir: &Path,
    concurrency: usize,
    include_all: bool,
) -> Result<()> {
    let mp = MultiProgress::new();
    download_artist_inner(api, art_id, format, output_dir, concurrency, include_all, &mp, None).await
}

async fn download_artist_inner(
    api: &DeezerApi,
    art_id: &str,
    format: TrackFormat,
    output_dir: &Path,
    concurrency: usize,
    include_all: bool,
    mp: &MultiProgress,
    anchor: Option<&ProgressBar>,
) -> Result<()> {
    let artist_info = api.get_artist_info(art_id).await?;
    let artist_name = artist_info["ART_NAME"]
        .as_str()
        .unwrap_or("Unknown Artist");

    let log = |msg: &str| {
        if let Some(pb) = anchor {
            pb.println(msg);
        } else {
            println!("{}", msg);
        }
    };

    log(&format!("Fetching discography for: {}\n", artist_name));

    let albums = api.get_artist_discography(art_id).await?;
    let albums: Vec<_> = albums.into_iter()
        .filter(|alb| include_all || alb.art_name.as_deref() == Some(artist_name))
        .collect();
    if albums.is_empty() {
        log(&format!("No albums found for {}.", artist_name));
        return Ok(());
    }

    log(&format!("Found {} albums/releases\n", albums.len()));

    let artist_dir = output_dir.join(sanitize_filename(artist_name));
    let mut total_downloaded = 0;
    let mut total_failed = 0;
    let total_albums = albums.len();

    let album_pb = match anchor {
        Some(a) => mp.insert_before(a, ProgressBar::new(total_albums as u64)),
        None => mp.add(ProgressBar::new(total_albums as u64)),
    };
    album_pb.set_style(
        ProgressStyle::default_bar()
            .template("  {bar:50.green/white} {pos}/{len} albums")
            .unwrap()
            .progress_chars("##-"),
    );

    for album in &albums {
        let alb_id = album.id_str();
        let album_title = album.alb_title.as_deref().unwrap_or("Unknown Album");
        let album_dir = artist_dir.join(sanitize_filename(album_title));

        album_pb.println(format!("--- Album: {} ---", album_title));

        let tracks = match api.get_album_tracks(&alb_id).await {
            Ok(t) => t,
            Err(e) => {
                album_pb.println(format!("  [err] Failed to get album tracks: {}", e));
                total_failed += 1;
                album_pb.inc(1);
                continue;
            }
        };

        let album_total = tracks.len();
        let track_pb = mp.insert_before(&album_pb, ProgressBar::new(album_total as u64));
        track_pb.set_style(
            ProgressStyle::default_bar()
                .template("    {bar:50.cyan/blue} {pos}/{len} tracks")
                .unwrap()
                .progress_chars("##-"),
        );

        let mut track_stream = stream::iter(tracks)
            .map(|track| {
                let dir = album_dir.clone();
                async move {
                    let display = track.display_name();
                    let result = download_track(api, &track, format, &dir).await;
                    (display, result)
                }
            })
            .buffer_unordered(concurrency);

        while let Some((display, result)) = track_stream.next().await {
            match result {
                Ok(TrackOutcome::Downloaded(_)) => {
                    total_downloaded += 1;
                    track_pb.println(format!("    [ok]   {}", display));
                }
                Ok(TrackOutcome::Skipped(_)) => {
                    track_pb.println(format!("    [skip] {}", display));
                }
                Err(e) => {
                    total_failed += 1;
                    track_pb.println(format!("    [err]  {} — {}", display, e));
                }
            }
            track_pb.inc(1);
        }
        track_pb.finish_and_clear();
        album_pb.inc(1);
    }
    album_pb.finish_and_clear();

    if let Some(pb) = anchor {
        pb.println(format!(
            "\n{}: {} downloaded, {} failed",
            artist_name, total_downloaded, total_failed
        ));
    } else {
        println!(
            "\nArtist download complete: {} downloaded, {} failed",
            total_downloaded, total_failed
        );
    }
    Ok(())
}

/// Download all favorited/followed artists' discographies
pub async fn download_favorite_artists(
    api: &DeezerApi,
    format: TrackFormat,
    output_dir: &Path,
    concurrency: usize,
    include_all: bool,
) -> Result<()> {
    println!("Fetching followed artists...\n");

    let ids = api.get_favorite_artist_ids().await?;
    if ids.is_empty() {
        println!("No followed artists found.");
        return Ok(());
    }

    println!("Found {} followed artists\n", ids.len());

    let mp = MultiProgress::new();
    let artist_pb = mp.add(ProgressBar::new(ids.len() as u64));
    artist_pb.set_style(
        ProgressStyle::default_bar()
            .template("{bar:50.yellow/white} {pos}/{len} artists")
            .unwrap()
            .progress_chars("##-"),
    );

    for art_id in &ids {
        match download_artist_inner(api, art_id, format, output_dir, concurrency, include_all, &mp, Some(&artist_pb)).await {
            Ok(()) => {}
            Err(e) => artist_pb.println(format!("  [err] {}: {}", art_id, e)),
        }
        artist_pb.inc(1);
    }
    artist_pb.finish_and_clear();

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

    match download_track(api, &track, format, output_dir).await {
        Ok(TrackOutcome::Downloaded(path)) => {
            println!("\nSaved to: {}", path.display());
        }
        Ok(TrackOutcome::Skipped(path)) => {
            println!("\nAlready exists: {}", path.display());
        }
        Err(e) => {
            eprintln!("\nFailed to download: {}", e);
        }
    }

    Ok(())
}
