mod api;
mod auth;
mod crypto;
mod download;
mod models;

use anyhow::Result;
use clap::{Parser, Subcommand};
use dialoguer::{Input, Select};
use std::path::PathBuf;

use crate::api::DeezerApi;
use crate::models::TrackFormat;

#[derive(Parser)]
#[command(name = "deezer-dl", version, about = "Deezer music downloader CLI")]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,

    /// Output directory for downloads
    #[arg(short, long)]
    output: Option<PathBuf>,

    /// Audio quality: flac, 320, 128
    #[arg(short, long, default_value = "320")]
    quality: String,
}

#[derive(Subcommand)]
enum Commands {
    /// Download a track by URL or ID
    Track {
        /// Deezer track URL or track ID
        url: String,
    },
    /// Download a playlist by URL or ID
    Playlist {
        /// Deezer playlist URL or playlist ID
        url: String,
    },
    /// Download your liked/favorite songs
    Favorites,
    /// Download all songs from an artist
    Artist {
        /// Deezer artist URL, ID, or search name
        query: String,
    },
    /// Interactive mode - choose what to download
    Interactive,
    /// Remove stored login credentials
    Logout,
}

fn parse_format(quality: &str) -> TrackFormat {
    match quality.to_lowercase().as_str() {
        "flac" | "lossless" | "9" => TrackFormat::Flac,
        "320" | "mp3_320" | "3" => TrackFormat::Mp3_320,
        "128" | "mp3_128" | "1" => TrackFormat::Mp3_128,
        _ => TrackFormat::Mp3_320,
    }
}

/// Extract ID from a Deezer URL or return the input as-is if it's already an ID
fn extract_id(input: &str, _entity: &str) -> String {
    // Handle URLs like https://www.deezer.com/en/track/12345
    if input.contains("deezer.com") {
        if let Some(pos) = input.rfind('/') {
            let id_part = &input[pos + 1..];
            // Remove query params
            let id = id_part.split('?').next().unwrap_or(id_part);
            return id.to_string();
        }
    }
    // Already an ID
    input.to_string()
}

fn default_output_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("Téléchargements")
        .join("mp3")
}

async fn interactive_mode(api: &DeezerApi, format: TrackFormat, output: &PathBuf) -> Result<()> {
    println!("Output directory: {}\n", output.display());

    loop {
        println!();
        let choices = &[
            "Download a track (URL or search)",
            "Download a playlist",
            "Download favorites (liked songs)",
            "Download all songs from an artist",
            "Quit",
        ];

        let selection = Select::new()
            .with_prompt("What would you like to do?")
            .items(choices)
            .default(0)
            .interact()?;

        match selection {
            0 => {
                let input: String = Input::new()
                    .with_prompt("Enter track URL or ID")
                    .interact_text()?;
                let id = extract_id(&input, "track");
                download::download_single_track(api, &id, format, output).await?;
            }
            1 => {
                // Show user playlists or enter URL
                let playlist_choices = &[
                    "Enter playlist URL or ID",
                    "Choose from my playlists",
                ];
                let pl_sel = Select::new()
                    .with_prompt("How to find the playlist?")
                    .items(playlist_choices)
                    .default(0)
                    .interact()?;

                match pl_sel {
                    0 => {
                        let input: String = Input::new()
                            .with_prompt("Enter playlist URL or ID")
                            .interact_text()?;
                        let id = extract_id(&input, "playlist");
                        download::download_playlist(api, &id, format, output).await?;
                    }
                    1 => {
                        let user = api.current_user.lock().await;
                        let user_id = user.as_ref().map(|u| u.id).unwrap_or(0);
                        drop(user);

                        let playlists = api.get_user_playlists(user_id).await?;
                        if playlists.is_empty() {
                            println!("No playlists found.");
                            continue;
                        }

                        let names: Vec<String> = playlists
                            .iter()
                            .map(|p| p.display_name())
                            .collect();

                        let sel = Select::new()
                            .with_prompt("Select a playlist")
                            .items(&names)
                            .default(0)
                            .interact()?;

                        let playlist_id = playlists[sel].id_str();
                        download::download_playlist(api, &playlist_id, format, output).await?;
                    }
                    _ => {}
                }
            }
            2 => {
                download::download_favorites(api, format, output).await?;
            }
            3 => {
                let input: String = Input::new()
                    .with_prompt("Enter artist URL, ID, or name to search")
                    .interact_text()?;

                // Check if it's a URL or ID
                if input.contains("deezer.com") || input.chars().all(|c| c.is_ascii_digit()) {
                    let id = extract_id(&input, "artist");
                    download::download_artist(api, &id, format, output).await?;
                } else {
                    // Search for artist
                    let results = api.search_artist(&input).await?;
                    let data = results["data"].as_array();
                    if data.is_none() || data.unwrap().is_empty() {
                        println!("No artists found for '{}'.", input);
                        continue;
                    }
                    let data = data.unwrap();

                    let names: Vec<String> = data
                        .iter()
                        .map(|a| {
                            let name = a["name"].as_str().unwrap_or("Unknown");
                            let fans = a["nb_fan"].as_u64().unwrap_or(0);
                            format!("{} ({} fans)", name, fans)
                        })
                        .collect();

                    let sel = Select::new()
                        .with_prompt("Select an artist")
                        .items(&names)
                        .default(0)
                        .interact()?;

                    let art_id = data[sel]["id"].as_u64().unwrap_or(0).to_string();
                    download::download_artist(api, &art_id, format, output).await?;
                }
            }
            4 => {
                println!("Bye!");
                break;
            }
            _ => {}
        }
    }
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let format = parse_format(&cli.quality);
    let is_interactive = matches!(cli.command, Some(Commands::Interactive) | None);
    let output = cli.output.clone().unwrap_or_else(|| {
        if is_interactive {
            default_output_dir()
        } else {
            PathBuf::from("./downloads")
        }
    });

    let api = DeezerApi::new()?;

    // Handle logout without login
    if let Some(Commands::Logout) = &cli.command {
        auth::remove_arl().await?;
        println!("Logged out. Stored ARL removed.");
        return Ok(());
    }

    // Login
    if !auth::login(&api).await? {
        return Ok(());
    }

    {
        let user = api.current_user.lock().await;
        if let Some(u) = user.as_ref() {
            println!("Logged in as: {}\n", u.name);
        }
    }

    // Create output dir
    tokio::fs::create_dir_all(&output).await?;

    match cli.command {
        Some(Commands::Track { url }) => {
            let id = extract_id(&url, "track");
            download::download_single_track(&api, &id, format, &output).await?;
        }
        Some(Commands::Playlist { url }) => {
            let id = extract_id(&url, "playlist");
            download::download_playlist(&api, &id, format, &output).await?;
        }
        Some(Commands::Favorites) => {
            download::download_favorites(&api, format, &output).await?;
        }
        Some(Commands::Artist { query }) => {
            if query.contains("deezer.com") || query.chars().all(|c| c.is_ascii_digit()) {
                let id = extract_id(&query, "artist");
                download::download_artist(&api, &id, format, &output).await?;
            } else {
                // Search
                let results = api.search_artist(&query).await?;
                let data = results["data"].as_array();
                if data.is_none() || data.unwrap().is_empty() {
                    println!("No artists found for '{}'.", query);
                    return Ok(());
                }
                let data = data.unwrap();

                let names: Vec<String> = data
                    .iter()
                    .map(|a| {
                        let name = a["name"].as_str().unwrap_or("Unknown");
                        let fans = a["nb_fan"].as_u64().unwrap_or(0);
                        format!("{} ({} fans)", name, fans)
                    })
                    .collect();

                let sel = Select::new()
                    .with_prompt("Select an artist")
                    .items(&names)
                    .default(0)
                    .interact()?;

                let art_id = data[sel]["id"].as_u64().unwrap_or(0).to_string();
                download::download_artist(&api, &art_id, format, &output).await?;
            }
        }
        Some(Commands::Interactive) | None => {
            interactive_mode(&api, format, &output).await?;
        }
        Some(Commands::Logout) => unreachable!(),
    }

    Ok(())
}
