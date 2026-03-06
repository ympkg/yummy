use anyhow::Result;
use console::style;

use crate::config;
use crate::workspace::resolver;

pub fn execute(json: bool) -> Result<()> {
    let (_, cfg) = config::load_or_find_config()?;

    let deps = cfg.dependencies.as_ref().cloned().unwrap_or_default();
    let dev_deps = cfg.dev_dependencies.as_ref().cloned().unwrap_or_default();

    if deps.is_empty() && dev_deps.is_empty() {
        if json {
            println!("[]");
        } else {
            println!("  No dependencies found");
        }
        return Ok(());
    }

    let mut outdated_list: Vec<OutdatedEntry> = Vec::new();

    for (coord, version) in deps.iter().chain(dev_deps.iter()) {
        let parts: Vec<&str> = coord.split(':').collect();
        if parts.len() != 2 {
            continue;
        }

        let is_dev = dev_deps.contains_key(coord);

        match resolver::fetch_latest_version(parts[0], parts[1]) {
            Ok(latest) => {
                if latest != *version {
                    outdated_list.push(OutdatedEntry {
                        coordinate: coord.clone(),
                        current: version.clone(),
                        latest,
                        dev: is_dev,
                    });
                }
            }
            Err(_) => {
                if !json {
                    println!(
                        "  {:<45} {:<15} {}",
                        coord,
                        version,
                        style("(fetch failed)").dim()
                    );
                }
            }
        }
    }

    if json {
        print_json(&outdated_list);
    } else {
        print_table(&outdated_list);
    }

    Ok(())
}

struct OutdatedEntry {
    coordinate: String,
    current: String,
    latest: String,
    dev: bool,
}

fn print_json(entries: &[OutdatedEntry]) {
    println!("[");
    for (i, e) in entries.iter().enumerate() {
        let comma = if i + 1 < entries.len() { "," } else { "" };
        println!(
            "  {{\"coordinate\":\"{}\",\"current\":\"{}\",\"latest\":\"{}\",\"dev\":{}}}{}",
            e.coordinate, e.current, e.latest, e.dev, comma
        );
    }
    println!("]");
}

fn print_table(entries: &[OutdatedEntry]) {
    println!();
    println!(
        "  {:<45} {:<15} {}",
        style("Package").bold(),
        style("Current").bold(),
        style("Latest").bold()
    );
    println!("  {}", "─".repeat(75));

    if entries.is_empty() {
        println!("  {} All dependencies are up to date!", style("✓").green());
    } else {
        for e in entries {
            let suffix = if e.dev { " (dev)" } else { "" };
            println!(
                "  {:<45} {:<15} {}{}",
                style(&e.coordinate).cyan(),
                style(&e.current).yellow(),
                style(&e.latest).green(),
                style(suffix).dim()
            );
        }
        println!();
        println!(
            "  {} {} outdated dependenc{}",
            style("!").yellow(),
            entries.len(),
            if entries.len() == 1 { "y" } else { "ies" }
        );
        println!(
            "  {} Run {} to upgrade",
            style("→").dim(),
            style("ym upgrade").cyan()
        );
    }

    println!();
}
