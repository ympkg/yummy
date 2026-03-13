use anyhow::{bail, Result};
use console::style;
use dialoguer::{Input, Password};
use std::collections::BTreeMap;

pub fn execute(
    list: bool,
    remove: Option<&str>,
    registry_url: Option<&str>,
    registry_name: Option<&str>,
    username: Option<&str>,
    password: Option<&str>,
) -> Result<()> {
    let creds_path = credentials_path();

    if list {
        return list_credentials(&creds_path);
    }

    if let Some(url) = remove {
        return remove_credentials(&creds_path, url);
    }

    // Resolve registry URL: --registry-url takes priority, then --registry (lookup from ym.json)
    let resolved_url = if let Some(url) = registry_url {
        Some(url.to_string())
    } else if let Some(name) = registry_name {
        Some(super::publish::resolve_registry_url_by_name(name)?)
    } else {
        None
    };

    // Non-interactive mode: all required params provided
    if let (Some(url), Some(user), Some(pass)) = (&resolved_url, username, password) {
        let mut creds = load_credentials_map(&creds_path);
        creds.insert(
            url.trim_end_matches('/').to_string(),
            serde_json::json!({
                "username": user,
                "password": pass
            }),
        );
        save_credentials_map(&creds_path, &creds)?;
        println!(
            "  {} Logged in to {}",
            style("✓").green(),
            style(url).cyan()
        );
        return Ok(());
    }

    // Interactive login (with optional defaults from flags)
    let registry: String = if let Some(url) = resolved_url {
        url
    } else {
        Input::new()
            .with_prompt("Registry URL")
            .interact_text()?
    };

    let user: String = if let Some(u) = username {
        u.to_string()
    } else {
        Input::new()
            .with_prompt("Username")
            .interact_text()?
    };

    let pass = if let Some(p) = password {
        p.to_string()
    } else {
        Password::new()
            .with_prompt("Password")
            .interact()?
    };

    // Load existing credentials or create new map
    let mut creds = load_credentials_map(&creds_path);

    // Store per-registry
    creds.insert(
        registry.trim_end_matches('/').to_string(),
        serde_json::json!({
            "username": user,
            "password": pass
        }),
    );

    save_credentials_map(&creds_path, &creds)?;

    println!(
        "  {} Logged in to {}",
        style("✓").green(),
        style(&registry).cyan()
    );

    Ok(())
}

fn list_credentials(creds_path: &std::path::Path) -> Result<()> {
    let creds = load_credentials_map(creds_path);

    if creds.is_empty() {
        println!("  No saved credentials.");
        println!("  Run {} to log in.", style("ym login").cyan());
        return Ok(());
    }

    println!();
    for (url, value) in &creds {
        let username = value.get("username").and_then(|v| v.as_str()).unwrap_or("?");
        println!(
            "  {} {} (username: {})",
            style("✓").green(),
            style(url).bold(),
            style(username).dim()
        );
    }
    println!();

    Ok(())
}

fn remove_credentials(creds_path: &std::path::Path, url: &str) -> Result<()> {
    let mut creds = load_credentials_map(creds_path);
    let normalized = url.trim_end_matches('/');

    if creds.remove(normalized).is_some() {
        save_credentials_map(creds_path, &creds)?;
        println!(
            "  {} Removed credentials for {}",
            style("✓").green(),
            style(url).cyan()
        );
    } else {
        bail!("No credentials found for '{}'", url);
    }

    Ok(())
}

fn credentials_path() -> std::path::PathBuf {
    crate::home_dir().join(".ym").join("credentials.json")
}

fn load_credentials_map(path: &std::path::Path) -> BTreeMap<String, serde_json::Value> {
    if !path.exists() {
        return BTreeMap::new();
    }
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => return BTreeMap::new(),
    };
    serde_json::from_str(&content).unwrap_or_default()
}

fn save_credentials_map(
    path: &std::path::Path,
    creds: &BTreeMap<String, serde_json::Value>,
) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let content = serde_json::to_string_pretty(creds)?;
    std::fs::write(path, content + "\n")?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }

    Ok(())
}
