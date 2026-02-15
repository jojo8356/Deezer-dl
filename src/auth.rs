use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use tokio::fs;

use crate::api::DeezerApi;

/// Get the config directory for storing ARL
pub fn config_dir() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("deezer-dl")
}

/// Read the stored ARL from disk
pub async fn read_stored_arl() -> Option<String> {
    let path = config_dir().join(".arl");
    fs::read_to_string(&path).await.ok().map(|s| s.trim().to_string())
}

/// Save the ARL to disk
pub async fn save_arl(arl: &str) -> Result<()> {
    let dir = config_dir();
    fs::create_dir_all(&dir).await.context("Failed to create config dir")?;
    let path = dir.join(".arl");
    fs::write(&path, arl.trim()).await.context("Failed to save ARL")?;
    Ok(())
}

/// Remove stored ARL
pub async fn remove_arl() -> Result<()> {
    let path = config_dir().join(".arl");
    if Path::new(&path).exists() {
        fs::remove_file(&path).await.context("Failed to remove ARL")?;
    }
    Ok(())
}

/// Attempt login with stored ARL, or prompt the user
pub async fn login(api: &DeezerApi) -> Result<bool> {
    // Try stored ARL first
    if let Some(arl) = read_stored_arl().await {
        if !arl.is_empty() {
            match api.login_via_arl(&arl).await {
                Ok(true) => return Ok(true),
                _ => {
                    eprintln!("Stored ARL is invalid, removing...");
                    let _ = remove_arl().await;
                }
            }
        }
    }

    // Prompt for ARL
    println!("You need a Deezer ARL cookie to use this tool.");
    println!("Get it from your browser: open deezer.com, press F12, go to Application > Cookies > arl\n");

    let arl: String = dialoguer::Input::new()
        .with_prompt("Enter your ARL")
        .interact_text()?;

    let logged_in = api.login_via_arl(&arl).await?;
    if logged_in {
        save_arl(&arl).await?;
        Ok(true)
    } else {
        eprintln!("Login failed. Invalid ARL.");
        Ok(false)
    }
}
