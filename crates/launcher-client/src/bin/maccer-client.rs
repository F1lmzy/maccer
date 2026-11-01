use anyhow::{Context, Result, bail};
use launcher_client::Client;
use launcher_protocol::Route;
use std::path::PathBuf;

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let socket = match args.next().as_deref() {
        Some("--socket") => PathBuf::from(args.next().context("--socket requires a path")?),
        _ => bail!(
            "usage: maccer-client --socket PATH (home [QUERY] | browse PROVIDER [QUERY] | providers)"
        ),
    };
    let command = args.next().context("expected home, browse, or providers")?;
    let mut client = Client::connect(socket)?;
    match command.as_str() {
        "providers" => println!("{}", serde_json::to_string_pretty(&client.providers()?)?),
        "home" => {
            let query = args.collect::<Vec<_>>().join(" ");
            let view = client.open_view(Route::Home, &query, 20)?;
            println!("{}", serde_json::to_string_pretty(&view)?);
        }
        "browse" => {
            let provider = args.next().context("browse requires a provider ID")?;
            let query = args.collect::<Vec<_>>().join(" ");
            let view = client.open_view(
                Route::Provider {
                    provider_id: provider,
                },
                &query,
                20,
            )?;
            println!("{}", serde_json::to_string_pretty(&view)?);
        }
        _ => bail!("unknown command {command:?}; expected home, browse, or providers"),
    }
    Ok(())
}
