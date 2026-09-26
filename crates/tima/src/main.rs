use std::env;
use std::fs;
use std::process::ExitCode;

use tima::backend::NativeBackend;
use tima::backend::c::CBackend;
use tima::backend::native::{ClangCompiler, NativeModule};
use tima::runtime::{OuterValue, ValueData};
use tima::source::SourceFile;

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(()) => ExitCode::FAILURE,
    }
}

fn run() -> Result<(), ()> {
    let mut arguments = env::args().skip(1);
    let command = arguments.next().unwrap_or_else(|| "help".to_owned());
    if command == "help" || command == "--help" || command == "-h" {
        eprintln!("usage: tima <check|run|emit-c> <file.tima>");
        return Ok(());
    }
    let Some(path) = arguments.next() else {
        eprintln!("error: missing Tima source path");
        return Err(());
    };
    if arguments.next().is_some() {
        eprintln!("error: unexpected additional arguments");
        return Err(());
    }
    let text = fs::read_to_string(&path).map_err(|error| {
        eprintln!("error: could not read {path}: {error}");
    })?;
    let diagnostic_source = SourceFile::new(path.clone(), text.clone());
    let compiled = tima::compile(path, text).map_err(|diagnostics| {
        for diagnostic in diagnostics {
            eprint!("{}", diagnostic.render(&diagnostic_source));
        }
    })?;

    match command.as_str() {
        "check" => {
            println!("ok: {} transform(s)", compiled.transforms.transforms.len());
        }
        "run" => {
            let generated = CBackend.emit(&compiled.transforms).map_err(|diagnostics| {
                for diagnostic in diagnostics {
                    eprint!("{}", diagnostic.render(&compiled.source));
                }
            })?;
            let artifact = ClangCompiler::default()
                .compile(&generated, "build")
                .map_err(|error| {
                    eprintln!("error: {error}");
                })?;
            let native = NativeModule::load(&artifact, &compiled.transforms).map_err(|error| {
                eprintln!("error: could not load native transform artifact: {error}");
            })?;
            let execution =
                tima::runtime::execute_native(&compiled, &native).map_err(|diagnostics| {
                    for diagnostic in diagnostics {
                        eprint!("{}", diagnostic.render(&compiled.source));
                    }
                })?;
            for (name, value) in execution.bindings {
                println!("{name} = {}", display(&value));
            }
        }
        "emit-c" => {
            let artifact = CBackend.emit(&compiled.transforms).map_err(|diagnostics| {
                for diagnostic in diagnostics {
                    eprint!("{}", diagnostic.render(&compiled.source));
                }
            })?;
            print!("{}", artifact.source);
        }
        _ => {
            eprintln!("error: unknown command `{command}`");
            eprintln!("usage: tima <check|run|emit-c> <file.tima>");
            return Err(());
        }
    }
    Ok(())
}

fn display(value: &OuterValue) -> String {
    match &value.data {
        ValueData::Null => "null".to_owned(),
        ValueData::Bool(value) => value.to_string(),
        ValueData::Integer(value) => value.to_string(),
        ValueData::Float(value) => value.to_string(),
        ValueData::String(value) => format!("{value:?}"),
        ValueData::List(values) => format!(
            "[{}]",
            values.iter().map(display).collect::<Vec<_>>().join(", ")
        ),
        ValueData::Record(values) => format!(
            "{{{}}}",
            values
                .iter()
                .map(|(name, value)| format!("{name}: {}", display(value)))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        ValueData::Asset(asset) => format!("asset({:?})", asset.locator),
        ValueData::Transform(id) => format!("<transform {}>", id.0),
    }
}
