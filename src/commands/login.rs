use anyhow::Result;
use console::style;
use dialoguer::{Input, Password};

pub fn execute() -> Result<()> {
    let registry: String = Input::new()
        .with_prompt("Registry URL")
        .default("https://repo1.maven.org/maven2".to_string())
        .interact_text()?;

    let username: String = Input::new()
        .with_prompt("Username")
        .interact_text()?;

    let password = Password::new()
        .with_prompt("Password")
        .interact()?;

    // Save credentials
    let creds_dir = credentials_dir();
    std::fs::create_dir_all(&creds_dir)?;

    let creds = serde_json::json!({
        "registry": registry,
        "username": username,
        "password": password
    });

    let creds_path = creds_dir.join("credentials.json");
    std::fs::write(&creds_path, serde_json::to_string_pretty(&creds)?)?;

    // Set restrictive permissions on Unix
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&creds_path, std::fs::Permissions::from_mode(0o600))?;
    }

    println!(
        "  {} Logged in to {}",
        style("✓").green(),
        style(&registry).cyan()
    );

    Ok(())
}

fn credentials_dir() -> std::path::PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    std::path::PathBuf::from(home).join(".ym")
}
