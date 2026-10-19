//! PaperEngine CLI: an outer adapter around the library's bounded runners.

mod cli;

fn main() {
    let mut args = std::env::args().skip(1);
    let result = match args.next().as_deref() {
        Some(
            command @ ("rank" | "discover" | "lookup" | "acquire" | "extract" | "index" | "search"
            | "inspect" | "self-test"),
        ) => cli::dispatch(command, args),
        Some("--version") | Some("-V") => {
            println!("paperengine {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        _ => Err(cli::usage().into()),
    };

    if let Err(error) = result {
        eprintln!("paperengine: {error}");
        std::process::exit(2);
    }
}
