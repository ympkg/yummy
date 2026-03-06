use anyhow::Result;
use console::style;

use crate::workspace::resolver;

pub fn execute(query: &str, limit: usize) -> Result<()> {
    println!(
        "  Searching Maven Central for '{}'...",
        style(query).bold()
    );
    println!();

    let results = resolver::search_maven(query)?;

    if results.is_empty() {
        println!("  No results found.");
        return Ok(());
    }

    let show: Vec<_> = results.iter().take(limit).collect();

    // Calculate column width for alignment
    let max_coord_len = show.iter()
        .map(|(g, a, _)| g.len() + 1 + a.len())
        .max()
        .unwrap_or(30);

    for (g, a, v) in &show {
        let coord = format!("{}:{}", g, a);
        println!(
            "  {:<width$}  {}",
            style(&coord).cyan(),
            style(v).dim(),
            width = max_coord_len
        );
    }

    println!();

    let total = results.len();
    if total > limit {
        println!(
            "  {} results, showing top {}. Add with: {}",
            total,
            limit,
            style("ym add <group>:<artifact>@<version>").dim()
        );
    } else {
        println!(
            "  {} results. Add with: {}",
            total,
            style("ym add <group>:<artifact>@<version>").dim()
        );
    }

    Ok(())
}
