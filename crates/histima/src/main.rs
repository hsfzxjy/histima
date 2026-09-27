use std::env;
use std::path::PathBuf;
use std::process::ExitCode;

use histima::Workspace;
use tima::identity::ContentIdentity;

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("error: {message}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), String> {
    let mut arguments = env::args().skip(1);
    let command = arguments.next().unwrap_or_else(|| "help".to_owned());
    if matches!(command.as_str(), "help" | "--help" | "-h") {
        print_usage();
        return Ok(());
    }
    match command.as_str() {
        "init" => {
            let workspace_path = required(&mut arguments, "workspace path")?;
            finished(&mut arguments)?;
            let workspace = Workspace::open(&workspace_path).map_err(|error| error.to_string())?;
            let info = workspace
                .catalog_info()
                .map_err(|error| error.to_string())?;
            println!("workspace = {}", workspace.root().display());
            println!("schema_version = {}", info.schema_version);
            println!("journal_mode = {}", info.journal_mode);
        }
        "import" => {
            let workspace_path = required(&mut arguments, "workspace path")?;
            let source_path = required(&mut arguments, "source asset path")?;
            finished(&mut arguments)?;
            let mut workspace =
                Workspace::open(&workspace_path).map_err(|error| error.to_string())?;
            let imported = workspace
                .import_file(&source_path)
                .map_err(|error| error.to_string())?;
            println!("locator = {}", imported.locator);
            println!("content_id = {}", imported.content_id);
            println!("source_id = {}", imported.source_id);
            println!("byte_length = {}", imported.byte_len);
        }
        "stats" => {
            let workspace_path = required(&mut arguments, "workspace path")?;
            finished(&mut arguments)?;
            let workspace = Workspace::open(&workspace_path).map_err(|error| error.to_string())?;
            let info = workspace
                .catalog_info()
                .map_err(|error| error.to_string())?;
            let stats = workspace
                .catalog_stats()
                .map_err(|error| error.to_string())?;
            println!("schema_version = {}", info.schema_version);
            println!("contents = {}", stats.contents);
            println!("source_versions = {}", stats.source_versions);
            println!("source_heads = {}", stats.source_heads);
        }
        "materialize" => {
            let workspace_path = required(&mut arguments, "workspace path")?;
            let identity_text = required(&mut arguments, "Content ID")?;
            let destination = required(&mut arguments, "destination path")?;
            finished(&mut arguments)?;
            let identity = identity_text
                .parse::<ContentIdentity>()
                .map_err(|error| format!("invalid Content ID: {error}"))?;
            let workspace = Workspace::open(&workspace_path).map_err(|error| error.to_string())?;
            workspace
                .materialize_content(identity, &destination)
                .map_err(|error| error.to_string())?;
            println!("content_id = {identity}");
            println!("materialized = {}", PathBuf::from(destination).display());
        }
        _ => {
            return Err(format!(
                "unknown command `{command}`; expected init, import, stats, or materialize"
            ));
        }
    }
    Ok(())
}

fn required(arguments: &mut impl Iterator<Item = String>, name: &str) -> Result<String, String> {
    arguments.next().ok_or_else(|| format!("missing {name}"))
}

fn finished(arguments: &mut impl Iterator<Item = String>) -> Result<(), String> {
    if let Some(argument) = arguments.next() {
        return Err(format!("unexpected additional argument {argument:?}"));
    }
    Ok(())
}

fn print_usage() {
    eprintln!("usage:");
    eprintln!("  histima init <workspace>");
    eprintln!("  histima import <workspace> <source-file>");
    eprintln!("  histima stats <workspace>");
    eprintln!("  histima materialize <workspace> <content-id> <destination>");
}
